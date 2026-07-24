//! Adapter from `tx_subsystems::device::BlockDevice` to `tx_fat_format::BlockImage`.
//!
//! Mirrors `tx_ext4_bridge.rs` — bridges the kernel block-device registry (sector-LBA
//! addressed, page-frame DMA targets) to the format crate's `BlockImage` (512-byte
//! logical blocks).

use core::ptr::NonNull;

use crate::devfs::adapter::step_engine::{epoch, page_allocator, StepOutcome, ZeroPolicy};
use tx_fat_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_fat_format::{FatFormatError, Result};
use tx_subsystems::device::{BlockDevice, PhysicalBlockNumber};
use tx_subsystems::page_backed::Frame;

/// A `BlockImage` that reads through a kernel block device.
///
/// The underlying `&'static dyn BlockDevice` is expected to outlive this
/// adapter — typically by leaking the device through `Box::leak` at boot.
pub struct BlockDeviceImage {
    device: &'static dyn BlockDevice,
}

impl BlockDeviceImage {
    pub fn new(device: &'static dyn BlockDevice) -> Self {
        Self { device }
    }

    fn sectors_per_fat_block(&self) -> Option<u64> {
        let sector = self.device.block_size() as u64;
        if sector == 0 || !(BLOCK_SIZE as u64).is_multiple_of(sector) {
            return None;
        }
        Some(BLOCK_SIZE as u64 / sector)
    }
}

impl BlockImage for BlockDeviceImage {
    fn total_blocks(&self) -> u64 {
        let Some(spb) = self.sectors_per_fat_block() else {
            return 0;
        };
        self.device.total_blocks() / spb
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()> {
        let spb = self
            .sectors_per_fat_block()
            .ok_or(FatFormatError::Unsupported)?;
        let lba = block.checked_mul(spb).ok_or(FatFormatError::OutOfBounds)?;

        let reservation = page_allocator::reserve_run(1, 1, ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| FatFormatError::IO)?;
        let run = reservation.commit();
        let ppn = run.base();
        let mut frame = Frame::new(ppn);

        let guard = epoch::borrow_current_guard().unwrap_or_else(epoch::guard);
        let outcome = self.device.read_blocks(
            PhysicalBlockNumber::new(lba),
            core::slice::from_mut(&mut frame),
            &guard,
        );
        drop(guard);
        match outcome {
            StepOutcome::Done(()) => {}
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return Err(FatFormatError::WouldBlock);
            }
            StepOutcome::Err(_) => return Err(FatFormatError::IO),
        }

        let src = page_allocator::frame_kernel_addr(ppn).map_err(|_| FatFormatError::IO)?;
        let src_nn = NonNull::new(src).ok_or(FatFormatError::IO)?;
        // SAFETY: `src_nn` points to BLOCK_SIZE bytes of a frame we own
        // through `run`. `out` is a `&mut [u8; BLOCK_SIZE]`.
        unsafe {
            core::ptr::copy_nonoverlapping(src_nn.as_ptr(), out.as_mut_ptr(), BLOCK_SIZE);
        }
        drop(run);
        Ok(())
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> Result<()> {
        let spb = self
            .sectors_per_fat_block()
            .ok_or(FatFormatError::Unsupported)?;
        let lba = block.checked_mul(spb).ok_or(FatFormatError::OutOfBounds)?;

        let reservation = page_allocator::reserve_run(1, 1, ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| FatFormatError::IO)?;
        let run = reservation.commit();
        let ppn = run.base();
        let frame = Frame::new(ppn);

        let dst = page_allocator::frame_kernel_addr(ppn).map_err(|_| FatFormatError::IO)?;
        let dst_nn = NonNull::new(dst).ok_or(FatFormatError::IO)?;
        // SAFETY: `dst_nn` is the kernel direct-map VA of the freshly
        // allocated frame. `data` is `&[u8; BLOCK_SIZE]`.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), dst_nn.as_ptr(), BLOCK_SIZE);
        }

        let guard = epoch::borrow_current_guard().unwrap_or_else(epoch::guard);
        let outcome = self.device.write_blocks(
            PhysicalBlockNumber::new(lba),
            core::slice::from_ref(&frame),
            &guard,
        );
        drop(guard);
        drop(run);
        match outcome {
            StepOutcome::Done(()) => Ok(()),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(FatFormatError::WouldBlock)
            }
            StepOutcome::Err(_) => Err(FatFormatError::IO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devfs::adapter::step_engine::{page_allocator, NoProgress};
    use tx_subsystems::device::{BlockDeviceOps, PhysicalBlockNumber};
    use tx_subsystems::execution::Guard;

    #[derive(Clone, Copy)]
    enum BlockingMode {
        Continue,
        Yield,
    }

    struct BlockingBlockDevice {
        mode: BlockingMode,
    }

    impl BlockingBlockDevice {
        const fn new(mode: BlockingMode) -> Self {
            Self { mode }
        }

        fn outcome(&self) -> StepOutcome<(), NoProgress> {
            match self.mode {
                BlockingMode::Continue => StepOutcome::continue_with(NoProgress),
                BlockingMode::Yield => StepOutcome::yield_on_wait_source(NoProgress, 43, 0x1),
            }
        }
    }

    impl BlockDeviceOps for BlockingBlockDevice {
        fn read_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _target: &mut [Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            self.outcome()
        }

        fn write_blocks(
            &self,
            _block_id: PhysicalBlockNumber,
            _source: &[Frame],
            _guard: &Guard<'_>,
        ) -> StepOutcome<(), NoProgress> {
            self.outcome()
        }

        fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
            StepOutcome::done(())
        }
    }

    impl BlockDevice for BlockingBlockDevice {
        fn total_blocks(&self) -> u64 {
            64
        }

        fn block_size(&self) -> u32 {
            BLOCK_SIZE as u32
        }
    }

    static CONTINUE_DEVICE: BlockingBlockDevice = BlockingBlockDevice::new(BlockingMode::Continue);
    static YIELD_DEVICE: BlockingBlockDevice = BlockingBlockDevice::new(BlockingMode::Yield);

    fn init_bridge_test() {
        tx_test_support::init_host();
        match page_allocator::claim_zero_frame() {
            Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
            Err(error) => panic!("claim zero frame for FAT bridge tests: {error:?}"),
        }
    }

    #[test]
    fn fat_bridge_maps_retrying_read_to_would_block() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        let mut out = [0u8; BLOCK_SIZE];

        assert_eq!(
            BlockDeviceImage::new(&CONTINUE_DEVICE).read_block(0, &mut out),
            Err(FatFormatError::WouldBlock)
        );
        assert_eq!(
            BlockDeviceImage::new(&YIELD_DEVICE).read_block(0, &mut out),
            Err(FatFormatError::WouldBlock)
        );
    }

    #[test]
    fn fat_bridge_maps_retrying_write_to_would_block() {
        let _serial = crate::test_support::FS_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        init_bridge_test();
        let data = [0x5au8; BLOCK_SIZE];

        assert_eq!(
            BlockDeviceImage::new(&CONTINUE_DEVICE).write_block(0, &data),
            Err(FatFormatError::WouldBlock)
        );
        assert_eq!(
            BlockDeviceImage::new(&YIELD_DEVICE).write_block(0, &data),
            Err(FatFormatError::WouldBlock)
        );
    }
}
