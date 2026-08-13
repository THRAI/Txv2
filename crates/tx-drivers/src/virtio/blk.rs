use alloc::boxed::Box;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::adapter::step_engine::{page_allocator, NoProgress, SpinMutex, StepOutcome};
use tx_hal::TxPlatform;
use tx_subsystems::{
    device::{
        BlockAsyncCompletion, BlockAsyncSubmit, BlockDevice, BlockDeviceOps,
        BlockDurabilityCapabilities, BlockWriteOptions, PhysicalBlockNumber,
    },
    execution::{Errno, Guard},
    page_backed::Frame,
};
use virtio_drivers::{
    device::blk::{BlkReq, BlkResp, VirtIOBlk},
    transport::pci::PciTransport,
};

use super::{
    block_async::{AsyncBlockOp, AsyncBlockState, InFlightChunk, MAX_DMA_CHUNK_PAGES},
    dma::TxVirtioHal,
    pci,
};

const PAGE_SIZE: usize = 4096;
const VIRTIO_BLK_SECTOR_SIZE: u32 = 512;

pub struct VirtioPciBlock<P: TxPlatform> {
    ecam_region_name: &'static str,
    mmio32_region_name: &'static str,
    inner: SpinMutex<Option<VirtIOBlk<TxVirtioHal<P>, PciTransport>>>,
    async_state: SpinMutex<AsyncBlockState>,
    io_gate: SpinMutex<()>,
    initialized: AtomicBool,
    total_blocks: AtomicU64,
    block_size: AtomicU32,
    _platform: PhantomData<fn() -> P>,
}

impl<P: TxPlatform> VirtioPciBlock<P> {
    pub const fn new(ecam_region_name: &'static str, mmio32_region_name: &'static str) -> Self {
        Self {
            ecam_region_name,
            mmio32_region_name,
            inner: SpinMutex::new(None),
            async_state: SpinMutex::new(AsyncBlockState::new()),
            io_gate: SpinMutex::new(()),
            initialized: AtomicBool::new(false),
            total_blocks: AtomicU64::new(0),
            block_size: AtomicU32::new(VIRTIO_BLK_SECTOR_SIZE),
            _platform: PhantomData,
        }
    }

    pub fn init(&'static self) -> Result<(), pci::VirtioPciError> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let ecam = pci::mmio_region::<P>(self.ecam_region_name)?;
        let mmio32 = pci::mmio_region::<P>(self.mmio32_region_name)?;
        let transport = pci::find_virtio_blk_transport::<P>(ecam, mmio32)?;
        let blk = VirtIOBlk::<TxVirtioHal<P>, PciTransport>::new(transport)
            .map_err(|_| pci::VirtioPciError::Transport)?;

        self.total_blocks.store(blk.capacity(), Ordering::Release);
        self.block_size
            .store(VIRTIO_BLK_SECTOR_SIZE, Ordering::Release);
        *self.inner.lock() = Some(blk);
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }
}

impl<P: TxPlatform> VirtioPciBlock<P> {
    fn read_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
    ) -> StepOutcome<(), NoProgress> {
        let _gate = self.io_gate.lock();
        self.quiesce_async();
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        let sectors_per_page = sectors_per_page(self.block_size());
        if sectors_per_page == 0 {
            return StepOutcome::Err(Errno::EINVAL.into());
        }

        let mut first = 0usize;
        while first < target.len() {
            let end = contiguous_chunk_end(target, first);
            let Some(lba) = block_id
                .as_u64()
                .checked_add(first as u64 * sectors_per_page as u64)
            else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            let Some(buf) = contiguous_frame_slice_mut(&mut target[first..end]) else {
                return StepOutcome::Err(Errno::EIO.into());
            };
            if blk.read_blocks(lba as usize, buf).is_err() {
                return StepOutcome::Err(Errno::EIO.into());
            }
            first = end;
        }
        StepOutcome::Done(())
    }

    fn write_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
    ) -> StepOutcome<(), NoProgress> {
        let _gate = self.io_gate.lock();
        self.quiesce_async();
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        let sectors_per_page = sectors_per_page(self.block_size());
        if sectors_per_page == 0 {
            return StepOutcome::Err(Errno::EINVAL.into());
        }

        let mut first = 0usize;
        while first < source.len() {
            let end = contiguous_chunk_end(source, first);
            let Some(lba) = block_id
                .as_u64()
                .checked_add(first as u64 * sectors_per_page as u64)
            else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            let Some(buf) = contiguous_frame_slice(&source[first..end]) else {
                return StepOutcome::Err(Errno::EIO.into());
            };
            if blk.write_blocks(lba as usize, buf).is_err() {
                return StepOutcome::Err(Errno::EIO.into());
            }
            first = end;
        }
        StepOutcome::Done(())
    }

    fn barrier_bootstrap(&self) -> StepOutcome<(), NoProgress> {
        let _gate = self.io_gate.lock();
        self.quiesce_async();
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        if blk.flush().is_err() {
            return StepOutcome::Err(Errno::EIO.into());
        }
        StepOutcome::Done(())
    }
}

impl<P: TxPlatform> BlockDeviceOps for VirtioPciBlock<P> {
    fn read_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        self.read_blocks_bootstrap(block_id, target)
    }

    fn write_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        self.write_blocks_bootstrap(block_id, source)
    }

    fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        self.barrier_bootstrap()
    }

    fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
        // virtio-drivers exposes a durable flush but no FUA write option.
        BlockDurabilityCapabilities {
            fua: false,
            flush: true,
        }
    }

    fn supports_async_blocks(&self) -> bool {
        true
    }

    fn submit_read_blocks_async(
        &self,
        cookie: u64,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
        _guard: &Guard<'_>,
    ) -> BlockAsyncSubmit {
        self.enqueue_async(cookie, AsyncBlockOp::Read, block_id, target)
    }

    fn submit_write_blocks_async(
        &self,
        cookie: u64,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        options: BlockWriteOptions,
        _guard: &Guard<'_>,
    ) -> BlockAsyncSubmit {
        if options.fua {
            return BlockAsyncSubmit::Complete(Err(Errno::EOPNOTSUPP));
        }
        self.enqueue_async(cookie, AsyncBlockOp::Write, block_id, source)
    }

    fn poll_async_blocks(&self, budget: usize, _guard: &Guard<'_>) -> Vec<BlockAsyncCompletion> {
        self.poll_async_completions(budget)
    }

    fn read_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
    ) -> StepOutcome<(), NoProgress> {
        self.read_blocks_bootstrap(block_id, target)
    }

    fn write_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
    ) -> StepOutcome<(), NoProgress> {
        self.write_blocks_bootstrap(block_id, source)
    }

    fn barrier_bootstrap(&self) -> StepOutcome<(), NoProgress> {
        self.barrier_bootstrap()
    }
}

impl<P: TxPlatform> VirtioPciBlock<P> {
    fn enqueue_async(
        &self,
        cookie: u64,
        op: AsyncBlockOp,
        block_id: PhysicalBlockNumber,
        frames: &[Frame],
    ) -> BlockAsyncSubmit {
        let _gate = self.io_gate.lock();
        if !self.is_initialized() {
            return BlockAsyncSubmit::Complete(Err(Errno::ENODEV));
        }
        let sectors = sectors_per_page(self.block_size());
        match self
            .async_state
            .lock()
            .enqueue(cookie, op, block_id.as_u64(), sectors, frames)
        {
            Ok(()) => BlockAsyncSubmit::Submitted,
            Err(error) => BlockAsyncSubmit::Complete(Err(error)),
        }
    }

    fn poll_async_completions(&self, budget: usize) -> Vec<BlockAsyncCompletion> {
        let mut completions = self.async_state.lock().take_ready(budget);
        if completions.len() < budget {
            completions.extend(self.drive_async_device(budget - completions.len()));
        }
        completions
    }

    fn quiesce_async(&self) {
        while self.async_state.lock().has_pending() {
            let completions = self.drive_async_device(usize::MAX);
            if completions.is_empty() {
                core::hint::spin_loop();
            } else {
                self.async_state.lock().stash_ready(completions);
            }
        }
    }

    fn drive_async_device(&self, budget: usize) -> Vec<BlockAsyncCompletion> {
        let mut completions = Vec::new();
        if budget == 0 {
            return completions;
        }
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return completions;
        };
        let mut state = self.async_state.lock();

        for _ in 0..budget {
            let Some(token) = blk.peek_used() else {
                break;
            };
            let Some(mut part) = state.in_flight.remove(&token) else {
                break;
            };
            let result = complete_pci_part(blk, token, &mut part);
            if let Some(completion) = state.finish_part(part.cookie, result) {
                completions.push(completion);
            }
        }

        loop {
            let Some((cookie, op, chunk)) = state.next_chunk() else {
                break;
            };
            let mut part = InFlightChunk {
                cookie,
                op,
                chunk,
                request: Box::new(BlkReq::default()),
                response: Box::new(BlkResp::default()),
            };
            match submit_pci_part(blk, &mut part) {
                Ok(token) => {
                    state
                        .submitted(token, part)
                        .expect("virtio-pci returned an already in-flight queue token");
                }
                Err(virtio_drivers::Error::QueueFull) => {
                    state.requeue_front(cookie, part.chunk);
                    break;
                }
                Err(_) => {
                    if let Some(completion) = state.fail_submission(cookie, Errno::EIO) {
                        completions.push(completion);
                    }
                }
            }
        }
        completions
    }
}

fn submit_pci_part<P: TxPlatform>(
    blk: &mut VirtIOBlk<TxVirtioHal<P>, PciTransport>,
    part: &mut InFlightChunk,
) -> virtio_drivers::Result<u16> {
    match part.op {
        AsyncBlockOp::Read => {
            let buf = contiguous_frame_slice_mut(&mut part.chunk.frames)
                .ok_or(virtio_drivers::Error::InvalidParam)?;
            unsafe {
                blk.read_blocks_nb(part.chunk.lba, &mut part.request, buf, &mut part.response)
            }
        }
        AsyncBlockOp::Write => {
            let buf = contiguous_frame_slice(&part.chunk.frames)
                .ok_or(virtio_drivers::Error::InvalidParam)?;
            unsafe {
                blk.write_blocks_nb(part.chunk.lba, &mut part.request, buf, &mut part.response)
            }
        }
    }
}

fn complete_pci_part<P: TxPlatform>(
    blk: &mut VirtIOBlk<TxVirtioHal<P>, PciTransport>,
    token: u16,
    part: &mut InFlightChunk,
) -> Result<(), Errno> {
    let result = match part.op {
        AsyncBlockOp::Read => {
            let Some(buf) = contiguous_frame_slice_mut(&mut part.chunk.frames) else {
                return Err(Errno::EIO);
            };
            unsafe { blk.complete_read_blocks(token, &part.request, buf, &mut part.response) }
        }
        AsyncBlockOp::Write => {
            let Some(buf) = contiguous_frame_slice(&part.chunk.frames) else {
                return Err(Errno::EIO);
            };
            unsafe { blk.complete_write_blocks(token, &part.request, buf, &mut part.response) }
        }
    };
    result.map_err(|_| Errno::EIO)
}

impl<P: TxPlatform> BlockDevice for VirtioPciBlock<P> {
    fn total_blocks(&self) -> u64 {
        self.total_blocks.load(Ordering::Acquire)
    }

    fn block_size(&self) -> u32 {
        self.block_size.load(Ordering::Acquire)
    }
}

fn sectors_per_page(block_size: u32) -> u32 {
    if block_size == 0 || !PAGE_SIZE.is_multiple_of(block_size as usize) {
        return 0;
    }
    (PAGE_SIZE / block_size as usize) as u32
}

fn contiguous_chunk_end(frames: &[Frame], first: usize) -> usize {
    let mut end = first + 1;
    while end < frames.len()
        && end - first < MAX_DMA_CHUNK_PAGES
        && frames[end].ppn().0 == frames[end - 1].ppn().0.saturating_add(1)
    {
        end += 1;
    }
    end
}

fn contiguous_frame_slice_mut(frames: &mut [Frame]) -> Option<&'static mut [u8]> {
    let first = *frames.first()?;
    let base = first.ppn().0;
    for (index, frame) in frames.iter().enumerate() {
        if frame.ppn().0 != base.checked_add(index)? {
            return None;
        }
    }
    let len = frames.len().checked_mul(PAGE_SIZE)?;
    let ptr = page_allocator::frame_kernel_addr(first.ppn()).ok()?;
    // SAFETY: the caller supplies exclusive DMA targets.  The verified PPN
    // sequence is contiguous and direct-map virtual addresses preserve that
    // adjacency.
    Some(unsafe { core::slice::from_raw_parts_mut(ptr, len) })
}

fn contiguous_frame_slice(frames: &[Frame]) -> Option<&'static [u8]> {
    let first = *frames.first()?;
    let base = first.ppn().0;
    for (index, frame) in frames.iter().enumerate() {
        if frame.ppn().0 != base.checked_add(index)? {
            return None;
        }
    }
    let len = frames.len().checked_mul(PAGE_SIZE)?;
    let ptr = page_allocator::frame_kernel_addr(first.ppn()).ok()?;
    // SAFETY: the verified PPN sequence is one immutable direct-map range.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sectors_per_page_rejects_non_dividing_block_sizes() {
        assert_eq!(sectors_per_page(512), 8);
        assert_eq!(sectors_per_page(4096), 1);
        assert_eq!(sectors_per_page(1000), 0);
        assert_eq!(sectors_per_page(0), 0);
    }
}
