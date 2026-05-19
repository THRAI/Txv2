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
            _ => return Err(FatFormatError::IO),
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
            _ => Err(FatFormatError::IO),
        }
    }
}
