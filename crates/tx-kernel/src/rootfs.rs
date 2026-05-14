use crate::adapter::step_engine::{page_allocator, NoProgress, StepOutcome};
use page_allocator::ZeroPolicy;
use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_ext4_format::Ext4FormatError;
use tx_subsystems::device::{BlockDevice, PhysicalBlockNumber};
use tx_subsystems::page_backed::Frame;

/// Collapse a bootstrap one-shot `StepOutcome` into `Result`.
///
/// Bootstrap block-device I/O never yields or continues — every
/// operation is a single synchronous step. `Continue` / `Yield`
/// outcomes are surface as `Unsupported` (defence in depth).
fn step_to_result(outcome: StepOutcome<(), NoProgress>) -> Result<(), Ext4FormatError> {
    match outcome {
        StepOutcome::Done(()) => Ok(()),
        StepOutcome::Err(_) => Err(Ext4FormatError::OutOfBounds),
        _ => Err(Ext4FormatError::Unsupported),
    }
}

pub(crate) struct RootBlockImage {
    block: &'static dyn BlockDevice,
}

impl RootBlockImage {
    pub(crate) const fn new(block: &'static dyn BlockDevice) -> Self {
        Self { block }
    }

    fn read_page(
        &self,
        block: u64,
        out: &mut Page4K,
    ) -> Result<(), tx_ext4_format::Ext4FormatError> {
        let sectors_per_page = sectors_per_page(self.block.block_size())
            .ok_or(tx_ext4_format::Ext4FormatError::Unsupported)?;
        let lba = block
            .checked_mul(sectors_per_page)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;

        let reservation = page_allocator::reserve_frame(ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        let ppn = reservation.ppn();
        let owned = reservation.commit();
        let mut frames = [Frame::new(ppn)];
        let outcome = self
            .block
            .read_blocks_bootstrap(PhysicalBlockNumber::new(lba), &mut frames);
        match step_to_result(outcome) {
            Ok(()) => {
                let ptr = page_allocator::frame_kernel_addr(ppn)
                    .map_err(|_| Ext4FormatError::OutOfBounds)?;
                unsafe {
                    out.copy_from_slice(core::slice::from_raw_parts(ptr.cast_const(), BLOCK_SIZE));
                }
                drop(owned);
                Ok(())
            }
            Err(e) => {
                drop(owned);
                Err(e)
            }
        }
    }
}

impl BlockImage for RootBlockImage {
    fn total_blocks(&self) -> u64 {
        let sectors_per_page = match sectors_per_page(self.block.block_size()) {
            Some(value) => value,
            None => return 0,
        };
        self.block.total_blocks() / sectors_per_page
    }

    fn read_block(&self, block: u64, out: &mut Page4K) -> tx_ext4_format::Result<()> {
        if block >= self.total_blocks() {
            return Err(tx_ext4_format::Ext4FormatError::OutOfBounds);
        }
        self.read_page(block, out)
    }

    fn write_block(&mut self, block: u64, data: &Page4K) -> tx_ext4_format::Result<()> {
        let sectors_per_page = sectors_per_page(self.block.block_size())
            .ok_or(tx_ext4_format::Ext4FormatError::Unsupported)?;
        let lba = block
            .checked_mul(sectors_per_page)
            .ok_or(tx_ext4_format::Ext4FormatError::OutOfBounds)?;

        let reservation = page_allocator::reserve_frame(ZeroPolicy::UninitFullOverwrite)
            .map_err(|_| tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        let ppn = reservation.ppn();
        let owned = reservation.commit();
        let ptr = page_allocator::frame_kernel_addr(ppn)
            .map_err(|_| tx_ext4_format::Ext4FormatError::OutOfBounds)?;
        unsafe {
            core::slice::from_raw_parts_mut(ptr, BLOCK_SIZE).copy_from_slice(data);
        }
        let frames = [Frame::new(ppn)];
        let result = step_to_result(
            self.block
                .write_blocks_bootstrap(PhysicalBlockNumber::new(lba), &frames),
        );
        drop(owned);
        result
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        step_to_result(self.block.barrier_bootstrap())
    }
}

const fn sectors_per_page(block_size: u32) -> Option<u64> {
    let block_size = block_size as usize;
    if block_size == 0 || !BLOCK_SIZE.is_multiple_of(block_size) {
        return None;
    }
    Some((BLOCK_SIZE / block_size) as u64)
}
