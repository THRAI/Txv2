use tx_ext4_format::pager::{BlockImage, Page4K, BLOCK_SIZE};
use tx_substrate::page_allocator::{self, ZeroPolicy};
use tx_substrate::step_v3::StepOutcome;
use tx_subsystems::device::{BlockDevice, PhysicalBlockNumber};
use tx_subsystems::page_backed::Frame;

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
        match self
            .block
            .read_blocks_bootstrap(PhysicalBlockNumber::new(lba), &mut frames)
        {
            StepOutcome::Done(()) => {
                let ptr = page_allocator::frame_kernel_addr(ppn)
                    .map_err(|_| tx_ext4_format::Ext4FormatError::OutOfBounds)?;
                unsafe {
                    out.copy_from_slice(core::slice::from_raw_parts(ptr.cast_const(), BLOCK_SIZE));
                }
                drop(owned);
                Ok(())
            }
            StepOutcome::Err(_) => {
                drop(owned);
                Err(tx_ext4_format::Ext4FormatError::OutOfBounds)
            }
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                drop(owned);
                Err(tx_ext4_format::Ext4FormatError::Unsupported)
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
        let result = match self
            .block
            .write_blocks_bootstrap(PhysicalBlockNumber::new(lba), &frames)
        {
            StepOutcome::Done(()) => Ok(()),
            StepOutcome::Err(_) => Err(tx_ext4_format::Ext4FormatError::OutOfBounds),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(tx_ext4_format::Ext4FormatError::Unsupported)
            }
        };
        drop(owned);
        result
    }

    fn barrier(&mut self) -> tx_ext4_format::Result<()> {
        match self.block.barrier_bootstrap() {
            StepOutcome::Done(()) => Ok(()),
            StepOutcome::Err(_) => Err(tx_ext4_format::Ext4FormatError::OutOfBounds),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                Err(tx_ext4_format::Ext4FormatError::Unsupported)
            }
        }
    }
}

const fn sectors_per_page(block_size: u32) -> Option<u64> {
    let block_size = block_size as usize;
    if block_size == 0 || !BLOCK_SIZE.is_multiple_of(block_size) {
        return None;
    }
    Some((BLOCK_SIZE / block_size) as u64)
}
