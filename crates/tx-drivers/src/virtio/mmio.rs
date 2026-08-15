//! VirtIO MMIO block transport for RISC-V QEMU virt and similar boards.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::adapter::step_engine::{self as step_engine, NoProgress, StepOutcome};
use step_engine::page_allocator;
use step_engine::SpinMutex;
use tx_hal::{MmioRegion, PlatformInfoIf, TxPlatform};
use tx_subsystems::device::{
    BlockAsyncCompletion, BlockAsyncSubmit, BlockDevice, BlockDeviceOps,
    BlockDurabilityCapabilities, BlockWriteOptions, PhysicalBlockNumber,
};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::page_backed::Frame;
use virtio_drivers::device::blk::{BlkReq, BlkResp, VirtIOBlk};
use virtio_drivers::transport::mmio::{MmioTransport, VirtIOHeader};

use super::block_async::{AsyncBlockOp, AsyncBlockState, InFlightChunk, MAX_DMA_CHUNK_PAGES};
use super::dma::TxVirtioHal;

const PAGE_SIZE: usize = 4096;
const VIRTIO_BLK_SECTOR_SIZE: u32 = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VirtioMmioError {
    MissingMmioRegion(&'static str),
    NullHeader,
    Transport,
    Device,
}

/// Source of a virtio-mmio register window. Static boards can retain the
/// name-based path, while firmware-discovered devices carry the exact region.
#[derive(Clone, Copy)]
pub(crate) enum RegionSource {
    Name(&'static str),
    Region(MmioRegion),
}

const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x08;

pub(crate) fn peek_device_type(
    region: MmioRegion,
) -> Option<virtio_drivers::transport::DeviceType> {
    let base = region.virt.start.0 as *const u8;
    if base.is_null() {
        return None;
    }
    // SAFETY: the caller passes a platform-published MMIO region and these
    // two words are the immutable virtio-mmio identification registers.
    let magic = unsafe { core::ptr::read_volatile(base.cast::<u32>()) };
    if magic != VIRTIO_MMIO_MAGIC {
        return None;
    }
    let device_id =
        unsafe { core::ptr::read_volatile(base.add(VIRTIO_MMIO_DEVICE_ID_OFFSET).cast::<u32>()) };
    virtio_drivers::transport::DeviceType::try_from(device_id).ok()
}

pub struct VirtioMmioBlock<P: TxPlatform> {
    region_source: RegionSource,
    inner: SpinMutex<Option<VirtIOBlk<TxVirtioHal<P>, MmioTransport<'static>>>>,
    async_state: SpinMutex<AsyncBlockState>,
    io_gate: SpinMutex<()>,
    initialized: AtomicBool,
    total_blocks: AtomicU64,
    block_size: AtomicU32,
    _platform: PhantomData<fn() -> P>,
}

// SAFETY: `MmioTransport<'static>` only contains pointers to a fixed hardware
// MMIO region whose mapping is `'static`. Concurrent access is mediated by the
// `SpinMutex`. The `Send` requirement only matters for cross-CPU ownership of
// the inner `Option<VirtIOBlk<...>>`, which is sound because the underlying
// MMIO addresses are valid on every hart.
unsafe impl<P: TxPlatform> Send for VirtioMmioBlock<P> {}
unsafe impl<P: TxPlatform> Sync for VirtioMmioBlock<P> {}

impl<P: TxPlatform> VirtioMmioBlock<P> {
    pub const fn new(mmio_region_name: &'static str) -> Self {
        Self::with_source(RegionSource::Name(mmio_region_name))
    }

    pub const fn from_region(region: MmioRegion) -> Self {
        Self::with_source(RegionSource::Region(region))
    }

    const fn with_source(region_source: RegionSource) -> Self {
        Self {
            region_source,
            inner: SpinMutex::new(None),
            async_state: SpinMutex::new(AsyncBlockState::new()),
            io_gate: SpinMutex::new(()),
            initialized: AtomicBool::new(false),
            total_blocks: AtomicU64::new(0),
            block_size: AtomicU32::new(VIRTIO_BLK_SECTOR_SIZE),
            _platform: PhantomData,
        }
    }

    pub fn init(&'static self) -> Result<(), VirtioMmioError> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let region = match self.region_source {
            RegionSource::Name(name) => mmio_region::<P>(name)?,
            RegionSource::Region(region) => region,
        };
        if peek_device_type(region) != Some(virtio_drivers::transport::DeviceType::Block) {
            return Err(VirtioMmioError::Device);
        }
        let header = NonNull::new(region.virt.start.0 as *mut VirtIOHeader)
            .ok_or(VirtioMmioError::NullHeader)?;

        // SAFETY: `header` points to the `virtio0` MMIO region mapped at
        // direct-map VA during early boot (see board `boot_static.rs`).
        // The mapping is `'static` for the lifetime of the kernel.
        let transport = unsafe { MmioTransport::new(header, region.virt.size) }
            .map_err(|_| VirtioMmioError::Transport)?;

        let blk = VirtIOBlk::<TxVirtioHal<P>, MmioTransport<'static>>::new(transport)
            .map_err(|_| VirtioMmioError::Device)?;

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

fn mmio_region<P: TxPlatform>(name: &'static str) -> Result<MmioRegion, VirtioMmioError> {
    <P as PlatformInfoIf>::platform_info()
        .mmio_regions
        .iter()
        .copied()
        .find(|region| region.name == name)
        .ok_or(VirtioMmioError::MissingMmioRegion(name))
}

impl<P: TxPlatform> BlockDeviceOps for VirtioMmioBlock<P> {
    fn read_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let _gate = self.io_gate.lock();
        self.quiesce_async();
        // Keep every data frame live until virtio-drivers has completed the
        // request and copied a bounce buffer back into the original direct-map
        // address.  A naked `Frame` does not itself carry that lifetime.
        let _dma_pins = match target
            .iter()
            .map(|frame| page_allocator::acquire_dma_pin(frame.ppn()))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(pins) => pins,
            Err(_) => return StepOutcome::Err(Errno::EIO.into()),
        };
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

    fn write_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let _gate = self.io_gate.lock();
        self.quiesce_async();
        let _dma_pins = match source
            .iter()
            .map(|frame| page_allocator::acquire_dma_pin(frame.ppn()))
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(pins) => pins,
            Err(_) => return StepOutcome::Err(Errno::EIO.into()),
        };
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

    fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
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

    fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
        BlockDurabilityCapabilities {
            fua: false,
            flush: true,
        }
    }

    fn supports_async_blocks(&self) -> bool {
        // Keep the page/block service graph asynchronous, but complete each
        // MMIO request through the proven synchronous virtio-drivers path.
        // The merged non-blocking path retains raw frame destinations across
        // scheduler turns and is not enabled until that lifetime contract is
        // independently pinned and stress-verified.
        false
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
}

impl<P: TxPlatform> VirtioMmioBlock<P> {
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
            let result = complete_mmio_part(blk, token, &mut part);
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
            match submit_mmio_part(blk, &mut part) {
                Ok(token) => {
                    state
                        .submitted(token, part)
                        .expect("virtio-mmio returned an already in-flight queue token");
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

fn submit_mmio_part<P: TxPlatform>(
    blk: &mut VirtIOBlk<TxVirtioHal<P>, MmioTransport<'static>>,
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

fn complete_mmio_part<P: TxPlatform>(
    blk: &mut VirtIOBlk<TxVirtioHal<P>, MmioTransport<'static>>,
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

impl<P: TxPlatform> BlockDevice for VirtioMmioBlock<P> {
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
    // sequence is contiguous and the kernel direct map preserves physical
    // adjacency, so the run is one writable byte slice.
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
    // SAFETY: the verified PPN sequence is contiguous in the kernel direct
    // map and the device only reads from this immutable source range.
    Some(unsafe { core::slice::from_raw_parts(ptr, len) })
}
