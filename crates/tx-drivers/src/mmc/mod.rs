//! DesignWare MSHC (MMC/SD host controller) block driver for the StarFive
//! JH7110 (VisionFive 2) SD card slot.
//!
//! The command sequence (`card_init`, `send_cmd`, single-block CMD17/CMD24
//! transfers) and the register access pattern are ported from Chronix
//! (GPLv3) `os/src/drivers/block/mmc/mod.rs`, which in turn follows the
//! Linux `dw_mmc` driver and the SD physical-layer spec. Cross-checked
//! against Del0n1x (GPLv3) `os/src/drivers/vf2/dw/vf2_sdio.rs` — the two are
//! independent implementations of the same controller. This file is
//! GPLv3 per upstream.
//!
//! Differences from the reference: this port is PIO-only (no internal DMA /
//! descriptor rings — the JH7110 DMA path has cache-coherency pitfalls and
//! PIO is fast enough for the contest workload), it is parameterized on the
//! controller's mapped MMIO base VA (supplied by the board), and it exposes
//! the multi-block `BlockDeviceOps` shape the kernel expects rather than the
//! reference's per-block `BlockDevice` trait.

pub mod register;

use crate::adapter::step_engine::{page_allocator, NoProgress, SpinMutex, StepOutcome};
use tx_subsystems::{
    device::{BlockDevice, BlockDeviceOps, BlockDurabilityCapabilities, PhysicalBlockNumber},
    execution::{Errno, Guard},
    page_backed::Frame,
};

use register::{
    CtypeCardWidth, BLKSIZ, BMOD, BYTCNT, CDETECT, CID, CLKDIV, CLKENA, CMD, CMDARG, CTRL, CTYPE,
    RESP, RINSTS, STATUS,
};

/// SD block size (bytes) and page geometry.
const SD_BLOCK_SIZE: usize = 512;
const PAGE_SIZE: usize = 4096;
const BLOCKS_PER_PAGE: usize = PAGE_SIZE / SD_BLOCK_SIZE; // 8
/// One SD block expressed as machine words (512 / 8 = 64 usize).
const WORDS_PER_BLOCK: usize = SD_BLOCK_SIZE / core::mem::size_of::<usize>();
/// Synopsys DWC data FIFO offset (fixed on this controller).
const FIFO_OFFSET: usize = 0x600;

/// Bounded spin used while polling controller status bits.
macro_rules! wait_for {
    ($cond:expr) => {{
        let mut timeout = 10_000_000usize;
        while !$cond && timeout > 0 {
            core::hint::spin_loop();
            timeout -= 1;
        }
    }};
}

/// DesignWare MSHC SD-card block device.
///
/// `base` is the controller's mapped MMIO base virtual address, provided by
/// the board when it discovers the `mmc@…` node. All register access is
/// `read_volatile`/`write_volatile` at `base + offset`.
pub struct Vf2Mmc {
    base: usize,
    /// Serializes the controller-wide command, FIFO, and interrupt-status
    /// registers across callers on different harts.
    io_gate: SpinMutex<()>,
    fifo_offset: SpinMutex<usize>,
    total_blocks: SpinMutex<u64>,
    ready: SpinMutex<bool>,
}

unsafe impl Send for Vf2Mmc {}
unsafe impl Sync for Vf2Mmc {}

impl Vf2Mmc {
    /// Construct over the controller's mapped MMIO base VA. Call
    /// [`Vf2Mmc::card_init`] before registering the device.
    pub const fn new(base: usize) -> Self {
        Self {
            base,
            io_gate: SpinMutex::new(()),
            fifo_offset: SpinMutex::new(FIFO_OFFSET),
            total_blocks: SpinMutex::new(0),
            ready: SpinMutex::new(false),
        }
    }

    fn with_io_transaction<R>(&self, operation: impl FnOnce() -> R) -> R {
        let _gate = self.io_gate.lock();
        operation()
    }

    // ---- raw register access ----

    #[inline]
    fn read_reg<T: Copy>(&self, offset: usize) -> T {
        unsafe { ((self.base + offset) as *const T).read_volatile() }
    }

    #[inline]
    fn write_reg<T>(&self, offset: usize, value: T) {
        unsafe { ((self.base + offset) as *mut T).write_volatile(value) }
    }

    fn read_fifo(&self) -> usize {
        let mut off = self.fifo_offset.lock();
        let value = unsafe { ((self.base + *off) as *const usize).read_volatile() };
        *off += core::mem::size_of::<usize>();
        value
    }

    fn write_fifo(&self, value: usize) {
        let mut off = self.fifo_offset.lock();
        unsafe { ((self.base + *off) as *mut usize).write_volatile(value) };
        *off += core::mem::size_of::<usize>();
    }

    fn reset_fifo_offset(&self) {
        *self.fifo_offset.lock() = FIFO_OFFSET;
    }

    fn status(&self) -> STATUS {
        self.read_reg::<STATUS>(STATUS::offset())
    }

    fn fifo_filled_cnt(&self) -> usize {
        self.status().fifo_count()
    }

    fn control_reg(&self) -> CTRL {
        self.read_reg::<CTRL>(CTRL::offset())
    }

    fn card_detect(&self) -> usize {
        let cdetect = self.read_reg::<CDETECT>(CDETECT::offset());
        !cdetect.card_detect_n() & 0xFFFF
    }

    fn dma_enabled(&self) -> bool {
        self.read_reg::<BMOD>(BMOD::offset()).idmac_enable()
    }

    fn set_dma(&self, enable: bool) {
        let bmod = self
            .read_reg::<BMOD>(BMOD::offset())
            .with_idmac_enable(enable)
            .with_software_reset(true);
        self.write_reg(BMOD::offset(), bmod);
        let ctrl = self
            .read_reg::<CTRL>(CTRL::offset())
            .with_dma_reset(true)
            .with_use_internal_dmac(enable);
        self.write_reg(CTRL::offset(), ctrl);
    }

    fn set_controller_bus_width(&self, card_index: usize, width: CtypeCardWidth) {
        let ctype = CTYPE::set_card_width(card_index, width);
        self.write_reg(CTYPE::offset(), ctype);
    }

    fn set_size(&self, block_size: usize, byte_cnt: usize) {
        self.write_reg(BLKSIZ::offset(), BLKSIZ::new().with_block_size(block_size));
        self.write_reg(BYTCNT::offset(), BYTCNT::new().with_byte_count(byte_cnt));
    }

    fn reset_clock(&self) {
        self.write_reg(CLKENA::offset(), CLKENA::new().with_cclk_enable(0));
        self.send_cmd(CMD::clock_cmd(), CMDARG::empty(), None, false);
        // Magic divider: ~400 KHz for identification.
        self.write_reg(CLKDIV::offset(), CLKDIV::new().with_clk_divider0(4));
        self.write_reg(CLKENA::offset(), CLKENA::new().with_cclk_enable(1));
        self.send_cmd(CMD::clock_cmd(), CMDARG::empty(), None, false);
    }

    fn reset_fifo(&self) {
        let ctrl = self.control_reg().with_fifo_reset(true);
        self.write_reg(CTRL::offset(), ctrl);
    }

    // ---- command engine (ported from Chronix send_cmd, PIO only) ----

    fn send_cmd(
        &self,
        cmd: CMD,
        arg: CMDARG,
        buffer: Option<&mut [usize]>,
        is_read: bool,
    ) -> Option<RESP> {
        let mut buffer_offset = 0usize;
        let buf_len = buffer.as_ref().map(|b| b.len()).unwrap_or(0);

        // Wait until the controller can accept a command.
        wait_for!(self.read_reg::<CMD>(CMD::offset()).can_send_cmd());
        if cmd.data_expected() {
            wait_for!(!self.status().data_busy());
        }

        self.write_reg(CMDARG::offset(), arg);
        self.write_reg(CMD::offset(), cmd);

        wait_for!(self.read_reg::<CMD>(CMD::offset()).can_send_cmd());

        if cmd.response_expected() {
            wait_for!(self.read_reg::<RINSTS>(RINSTS::offset()).command_done());
        }

        if cmd.data_expected() {
            let buffer = buffer;
            if is_read {
                if let Some(buffer) = buffer {
                    wait_for!({
                        let rinsts = self.read_reg::<RINSTS>(RINSTS::offset());
                        if rinsts.receive_data_request() && !self.dma_enabled() {
                            while self.fifo_filled_cnt() >= 2 {
                                if buffer_offset >= buf_len {
                                    break;
                                }
                                buffer[buffer_offset] = self.read_fifo();
                                buffer_offset += 1;
                            }
                        }
                        rinsts.data_transfer_over() || !rinsts.no_error()
                    });
                }
            } else if let Some(buffer) = buffer {
                wait_for!({
                    let rinsts = self.read_reg::<RINSTS>(RINSTS::offset());
                    if rinsts.transmit_data_request() && !self.dma_enabled() {
                        while self.fifo_filled_cnt() < 120 {
                            if buffer_offset >= buf_len {
                                break;
                            }
                            self.write_fifo(buffer[buffer_offset]);
                            buffer_offset += 1;
                        }
                    }
                    rinsts.data_transfer_over() || !rinsts.no_error()
                });
            }
            self.reset_fifo_offset();
        }

        let rinsts = self.read_reg::<RINSTS>(RINSTS::offset());
        // Clear interrupt status by writing back the bits (write-1-to-clear).
        self.write_reg(RINSTS::offset(), rinsts);

        let resp = self.read_reg::<RESP>(RESP::offset());
        if rinsts.no_error() && !rinsts.command_conflict() {
            if cmd.update_clock_register_only() {
                return None;
            }
            Some(resp)
        } else {
            None
        }
    }

    /// Card identification + init handshake (ported from Chronix card_init,
    /// PIO). Returns `true` when the card powered up and reported ready.
    pub fn card_init(&self) -> bool {
        let card_idx = 0usize;
        const TEST_PATTERN: u32 = 0xAA;

        self.reset_clock();
        self.reset_fifo();
        self.set_controller_bus_width(card_idx, CtypeCardWidth::Width1);
        self.set_dma(false);

        // CMD0: go idle.
        self.send_cmd(CMD::reset_cmd0(card_idx), CMDARG::empty(), None, false);

        // CMD8: voltage check / SDHC v2 probe.
        let cmd = CMD::no_data_cmd(card_idx, 8);
        let arg = CMDARG::from((1u32 << 8) | TEST_PATTERN);
        let _ = self.send_cmd(cmd, arg, None, false);

        // ACMD41: power up. Loop on CMD55 + ACMD41 until the card is ready.
        let mut powered = false;
        for _ in 0..1000 {
            self.send_cmd(CMD::no_data_cmd(card_idx, 55), CMDARG::empty(), None, false);
            let acmd = CMD::no_data_cmd_no_crc(card_idx, 41);
            let arg = CMDARG::from((1u32 << 30) | (1u32 << 24) | 0x00FF_8000);
            if let Some(resp) = self.send_cmd(acmd, arg, None, false) {
                if resp.ocr() & (1 << 31) != 0 {
                    powered = true;
                    break;
                }
            }
            for _ in 0..100_000 {
                core::hint::spin_loop();
            }
        }
        if !powered {
            return false;
        }

        // CMD2: read CID (R2 long response, no CRC).
        let cmd = CMD::no_data_cmd_no_crc(card_idx, 2).with_response_length(true);
        let _cid = self
            .send_cmd(cmd, CMDARG::empty(), None, false)
            .map(|resp| CID::from(resp.resps_u128()));

        // CMD3: get the card's relative address (RCA).
        let rca = match self.send_cmd(CMD::no_data_cmd(card_idx, 3), CMDARG::empty(), None, false) {
            Some(resp) => resp.resp(0) >> 16,
            None => return false,
        };

        // CMD7: select the card by its RCA.
        let arg = CMDARG::from(rca << 16);
        self.send_cmd(CMD::no_data_cmd(card_idx, 7), arg, None, false);

        // Fix the transfer block length at 512 bytes.
        self.set_size(SD_BLOCK_SIZE, SD_BLOCK_SIZE);

        // Capacity: unknown without decoding the CSD; report a large lower
        // bound so mounts/reads are not clamped (the fs reads only what it
        // needs). Refined later if CSD decode is added.
        *self.total_blocks.lock() = 0x0800_0000; // 128M blocks * 512 = 64 GiB ceiling
        *self.ready.lock() = true;
        let _ = self.card_detect();
        true
    }

    // ---- single-block PIO transfers ----

    fn read_one_block(&self, lba: u32, out: &mut [usize]) -> bool {
        self.set_size(SD_BLOCK_SIZE, SD_BLOCK_SIZE);
        let cmd = CMD::data_cmd(0, 17); // CMD17 single-block read
        self.send_cmd(cmd, CMDARG::from(lba), Some(out), true)
            .is_some()
    }

    fn write_one_block(&self, lba: u32, data: &mut [usize]) -> bool {
        self.set_size(SD_BLOCK_SIZE, SD_BLOCK_SIZE);
        let cmd = CMD::data_cmd(0, 24).with_read_write(true); // CMD24 single-block write
        self.send_cmd(cmd, CMDARG::from(lba), Some(data), false)
            .is_some()
    }
}

// ---- frame <-> word-slice helpers (same shape as the virtio block driver) ----

fn frame_words_mut(frame: Frame) -> Option<&'static mut [usize]> {
    let ptr = page_allocator::frame_kernel_addr(frame.ppn()).ok()?;
    Some(unsafe {
        core::slice::from_raw_parts_mut(
            ptr as *mut usize,
            PAGE_SIZE / core::mem::size_of::<usize>(),
        )
    })
}

impl BlockDeviceOps for Vf2Mmc {
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
        StepOutcome::Done(())
    }

    fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
        BlockDurabilityCapabilities {
            fua: false,
            flush: true,
        }
    }

    fn read_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
    ) -> StepOutcome<(), NoProgress> {
        self.with_io_transaction(|| {
            if !*self.ready.lock() {
                return StepOutcome::Err(Errno::ENODEV.into());
            }
            for (idx, frame) in target.iter_mut().enumerate() {
                let Some(words) = frame_words_mut(*frame) else {
                    return StepOutcome::Err(Errno::EIO.into());
                };
                for blk in 0..BLOCKS_PER_PAGE {
                    let lba = block_id.as_u64() + (idx * BLOCKS_PER_PAGE + blk) as u64;
                    let chunk = &mut words[blk * WORDS_PER_BLOCK..(blk + 1) * WORDS_PER_BLOCK];
                    if !self.read_one_block(lba as u32, chunk) {
                        return StepOutcome::Err(Errno::EIO.into());
                    }
                }
            }
            StepOutcome::Done(())
        })
    }

    fn write_blocks_bootstrap(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
    ) -> StepOutcome<(), NoProgress> {
        self.with_io_transaction(|| {
            if !*self.ready.lock() {
                return StepOutcome::Err(Errno::ENODEV.into());
            }
            for (idx, frame) in source.iter().enumerate() {
                let Some(words) = frame_words_mut(*frame) else {
                    return StepOutcome::Err(Errno::EIO.into());
                };
                for blk in 0..BLOCKS_PER_PAGE {
                    let lba = block_id.as_u64() + (idx * BLOCKS_PER_PAGE + blk) as u64;
                    let chunk = &mut words[blk * WORDS_PER_BLOCK..(blk + 1) * WORDS_PER_BLOCK];
                    if !self.write_one_block(lba as u32, chunk) {
                        return StepOutcome::Err(Errno::EIO.into());
                    }
                }
            }
            StepOutcome::Done(())
        })
    }

    fn barrier_bootstrap(&self) -> StepOutcome<(), NoProgress> {
        StepOutcome::Done(())
    }
}

impl BlockDevice for Vf2Mmc {
    fn total_blocks(&self) -> u64 {
        *self.total_blocks.lock()
    }

    fn block_size(&self) -> u32 {
        SD_BLOCK_SIZE as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{mpsc, Arc},
        time::Duration,
    };

    #[test]
    fn vf2_mmc_controller_transactions_are_exclusive() {
        let mmc = Arc::new(Vf2Mmc::new(0));
        let (first_entered_tx, first_entered_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_mmc = Arc::clone(&mmc);
        let first = std::thread::spawn(move || {
            first_mmc.with_io_transaction(|| {
                first_entered_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
            });
        });
        first_entered_rx.recv().unwrap();

        let (second_entered_tx, second_entered_rx) = mpsc::channel();
        let second_mmc = Arc::clone(&mmc);
        let second = std::thread::spawn(move || {
            second_mmc.with_io_transaction(|| second_entered_tx.send(()).unwrap());
        });

        let overlapped = second_entered_rx
            .recv_timeout(Duration::from_millis(100))
            .is_ok();
        release_first_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();

        assert!(!overlapped, "MMC controller transactions must not overlap");
    }

    #[test]
    fn vf2_mmc_reports_successful_barrier_as_flush_without_fua() {
        let mmc = Vf2Mmc::new(0);

        assert!(matches!(
            BlockDeviceOps::barrier_bootstrap(&mmc),
            StepOutcome::Done(())
        ));
        assert_eq!(
            BlockDeviceOps::durability_capabilities(&mmc),
            BlockDurabilityCapabilities {
                fua: false,
                flush: true,
            }
        );
    }
}
