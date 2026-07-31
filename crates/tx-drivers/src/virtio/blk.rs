use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::adapter::step_engine::{page_allocator, NoProgress, SpinMutex, StepOutcome};
use tx_hal::TxPlatform;
use tx_subsystems::{
    device::{BlockDevice, BlockDeviceOps, PhysicalBlockNumber},
    execution::{Errno, Guard},
    page_backed::Frame,
};
use virtio_drivers::{device::blk::VirtIOBlk, transport::pci::PciTransport};

use super::{dma::TxVirtioHal, pci};

const PAGE_SIZE: usize = 4096;
const VIRTIO_BLK_SECTOR_SIZE: u32 = 512;

pub struct VirtioPciBlock<P: TxPlatform> {
    ecam_region_name: &'static str,
    mmio32_region_name: &'static str,
    inner: SpinMutex<Option<VirtIOBlk<TxVirtioHal<P>, PciTransport>>>,
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
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        let sectors_per_page = sectors_per_page(self.block_size());
        if sectors_per_page == 0 {
            return StepOutcome::Err(Errno::EINVAL.into());
        }

        if let Some(buf) = contiguous_frame_slice_mut(target) {
            let Ok(lba) = usize::try_from(block_id.as_u64()) else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            return if blk.read_blocks(lba, buf).is_ok() {
                StepOutcome::Done(())
            } else {
                StepOutcome::Err(Errno::EIO.into())
            };
        }

        for (idx, frame) in target.iter_mut().enumerate() {
            let Some(lba) = block_id
                .as_u64()
                .checked_add(idx as u64 * sectors_per_page as u64)
            else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            let Some(buf) = frame_slice_mut(*frame) else {
                return StepOutcome::Err(Errno::EIO.into());
            };
            if blk.read_blocks(lba as usize, buf).is_err() {
                return StepOutcome::Err(Errno::EIO.into());
            }
        }
        StepOutcome::Done(())
    }

    fn write_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
    ) -> StepOutcome<(), NoProgress> {
        let mut inner = self.inner.lock();
        let Some(blk) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV.into());
        };
        let sectors_per_page = sectors_per_page(self.block_size());
        if sectors_per_page == 0 {
            return StepOutcome::Err(Errno::EINVAL.into());
        }

        if let Some(buf) = contiguous_frame_slice(source) {
            let Ok(lba) = usize::try_from(block_id.as_u64()) else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            return if blk.write_blocks(lba, buf).is_ok() {
                StepOutcome::Done(())
            } else {
                StepOutcome::Err(Errno::EIO.into())
            };
        }

        for (idx, frame) in source.iter().enumerate() {
            let Some(lba) = block_id
                .as_u64()
                .checked_add(idx as u64 * sectors_per_page as u64)
            else {
                return StepOutcome::Err(Errno::EINVAL.into());
            };
            let Some(buf) = frame_slice(*frame) else {
                return StepOutcome::Err(Errno::EIO.into());
            };
            if blk.write_blocks(lba as usize, buf).is_err() {
                return StepOutcome::Err(Errno::EIO.into());
            }
        }
        StepOutcome::Done(())
    }

    fn barrier_bootstrap(&self) -> StepOutcome<(), NoProgress> {
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

fn frame_slice_mut(frame: Frame) -> Option<&'static mut [u8]> {
    let ptr = page_allocator::frame_kernel_addr(frame.ppn()).ok()?;
    Some(unsafe { core::slice::from_raw_parts_mut(ptr, PAGE_SIZE) })
}

fn frame_slice(frame: Frame) -> Option<&'static [u8]> {
    let ptr = page_allocator::frame_kernel_addr(frame.ppn()).ok()?;
    Some(unsafe { core::slice::from_raw_parts(ptr, PAGE_SIZE) })
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
