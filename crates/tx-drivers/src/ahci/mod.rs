//! AHCI 1.x polling block transport.
//!
//! The bootstrap implementation deliberately owns one command slot and a
//! private 4 KiB bounce buffer. That keeps caller-owned page frames out of the
//! controller's 32-bit DMA domain and gives early rootfs reads a synchronous,
//! bounded completion path before block interrupts are enabled.

mod dma;
mod regs;

use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::{marker::PhantomData, mem::ManuallyDrop};

use crate::adapter::step_engine::{self as step_engine, NoProgress, StepOutcome};
use dma::{DmaWorkspace, DmaWorkspaceError};
use regs::{AhciRegisterIo, VolatileMmio};
use step_engine::page_allocator;
use step_engine::SpinMutex;
use tx_hal::{DmaDirection, DmaDomain, DmaIf, MmioRegion, TxPlatform};
use tx_subsystems::device::{
    BlockDevice, BlockDeviceOps, BlockDurabilityCapabilities, PhysicalBlockNumber,
};
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::page_backed::Frame;

const PAGE_SIZE: usize = 4096;
const LOGICAL_BLOCK_SIZE: u32 = 512;
const SECTORS_PER_PAGE: u64 = PAGE_SIZE as u64 / LOGICAL_BLOCK_SIZE as u64;

const COMMAND_LIST_OFFSET: usize = 0x000;
const COMMAND_LIST_LEN: usize = 0x400;
const RECEIVED_FIS_OFFSET: usize = 0x400;
const RECEIVED_FIS_LEN: usize = 0x100;
const COMMAND_TABLE_OFFSET: usize = 0x500;
const COMMAND_TABLE_LEN: usize = 0x100;
const COMMAND_FIS_OFFSET: usize = COMMAND_TABLE_OFFSET;
const PRDT_OFFSET: usize = COMMAND_TABLE_OFFSET + 0x80;
const BOUNCE_OFFSET: usize = 0x1000;
const BOUNCE_LEN: usize = PAGE_SIZE;
const WORKSPACE_LEN: usize = BOUNCE_OFFSET + BOUNCE_LEN;

const COMMAND_HEADER_CFL_DWORDS: u16 = 5;
const COMMAND_HEADER_WRITE: u16 = 1 << 6;
const COMMAND_SLOT: u32 = 1;
const ATA_CMD_IDENTIFY_DEVICE: u8 = 0xec;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xea;
const AHCI_DURABILITY_CAPABILITIES: BlockDurabilityCapabilities = BlockDurabilityCapabilities {
    fua: false,
    flush: true,
};
const FIS_TYPE_REG_H2D: u8 = 0x27;
const FIS_COMMAND: u8 = 1 << 7;
const ATA_DEVICE_LBA: u8 = 1 << 6;

const HOST_TIMEOUT_NS: u64 = 1_000_000_000;
const COMMAND_TIMEOUT_NS: u64 = 5_000_000_000;
const FLUSH_TIMEOUT_NS: u64 = 60_000_000_000;
const POLL_ITERATION_LIMIT: usize = 10_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AhciIdentity {
    pub total_blocks: u64,
    pub logical_block_size: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AhciError {
    InvalidMmio,
    InvalidController,
    BiosHandoffTimeout,
    HostResetTimeout,
    NoImplementedPort,
    LinkTimeout,
    UnsupportedDeviceSignature(u32),
    PortStopTimeout,
    PortStartTimeout,
    PortBusyTimeout,
    DmaAllocation,
    DmaAddress,
    DmaKernelImageOverlap,
    CommandSlotBusy,
    CommandTimeout,
    TaskFileError,
    InvalidIdentify,
    UnsupportedAddressing,
    UnsupportedLogicalBlockSize(u32),
    OutOfRange,
    FrameMapping,
}

impl AhciError {
    pub const fn label(self) -> &'static str {
        match self {
            Self::InvalidMmio => "invalid-mmio",
            Self::InvalidController => "invalid-controller",
            Self::BiosHandoffTimeout => "bios-handoff-timeout",
            Self::HostResetTimeout => "host-reset-timeout",
            Self::NoImplementedPort => "no-implemented-port",
            Self::LinkTimeout => "link-timeout",
            Self::UnsupportedDeviceSignature(_) => "unsupported-device-signature",
            Self::PortStopTimeout => "port-stop-timeout",
            Self::PortStartTimeout => "port-start-timeout",
            Self::PortBusyTimeout => "port-busy-timeout",
            Self::DmaAllocation => "dma-allocation",
            Self::DmaAddress => "dma-address",
            Self::DmaKernelImageOverlap => "dma-kernel-image-overlap",
            Self::CommandSlotBusy => "command-slot-busy",
            Self::CommandTimeout => "command-timeout",
            Self::TaskFileError => "task-file-error",
            Self::InvalidIdentify => "invalid-identify",
            Self::UnsupportedAddressing => "unsupported-addressing",
            Self::UnsupportedLogicalBlockSize(_) => "unsupported-logical-block-size",
            Self::OutOfRange => "out-of-range",
            Self::FrameMapping => "frame-mapping",
        }
    }
}

pub struct AhciBlock<P: TxPlatform> {
    region: MmioRegion,
    dma_domain: &'static DmaDomain,
    state: SpinMutex<Option<AhciState<P>>>,
    initialized: AtomicBool,
    total_blocks: AtomicU64,
    block_size: AtomicU32,
    runtime_failures: RuntimeFailureObservation,
    _platform: PhantomData<fn() -> P>,
}

struct RuntimeFailureObservation {
    count: AtomicU64,
    first_reported: AtomicBool,
}

impl RuntimeFailureObservation {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            first_reported: AtomicBool::new(false),
        }
    }

    fn observe(&self) -> bool {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.first_reported
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeOperation {
    Read,
    Write,
    Flush,
}

impl RuntimeOperation {
    const fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Flush => "flush",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimeFailureSnapshot {
    operation: RuntimeOperation,
    lba: u64,
    error: AhciError,
    interrupt_status: u32,
    task_file: u32,
    sata_error: u32,
    command_issue: u32,
}

struct AhciState<P: TxPlatform> {
    regs: VolatileMmio,
    port: usize,
    workspace: ManuallyDrop<DmaWorkspace<P>>,
    identity: AhciIdentity,
    faulted: bool,
}

impl<P: TxPlatform> Drop for AhciState<P> {
    fn drop(&mut self) {
        if !detach_workspace::<P, _>(&self.regs, self.port) {
            // The HBA may still DMA through CLB/FB. Keep the allocation and
            // its pins quarantined instead of returning live pages.
            return;
        }
        unsafe {
            ManuallyDrop::drop(&mut self.workspace);
        }
    }
}

impl<P: TxPlatform> AhciBlock<P> {
    pub const fn new(region: MmioRegion, dma_domain: &'static DmaDomain) -> Self {
        Self {
            region,
            dma_domain,
            state: SpinMutex::new(None),
            initialized: AtomicBool::new(false),
            total_blocks: AtomicU64::new(0),
            block_size: AtomicU32::new(LOGICAL_BLOCK_SIZE),
            runtime_failures: RuntimeFailureObservation::new(),
            _platform: PhantomData,
        }
    }

    pub fn init(&self) -> Result<AhciIdentity, AhciError> {
        let mut state_slot = self.state.lock();
        if let Some(state) = state_slot.as_ref() {
            return Ok(state.identity);
        }

        let state = initialize_controller::<P>(self.region, self.dma_domain)?;
        let identity = state.identity;
        self.total_blocks
            .store(identity.total_blocks, Ordering::Release);
        self.block_size
            .store(identity.logical_block_size, Ordering::Release);
        *state_slot = Some(state);
        self.initialized.store(true, Ordering::Release);
        Ok(identity)
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    fn observe_runtime_failure(
        &self,
        state: &AhciState<P>,
        operation: RuntimeOperation,
        lba: u64,
        error: AhciError,
    ) {
        if !self.runtime_failures.observe() {
            return;
        }
        report_runtime_failure::<P>(capture_runtime_failure(state, operation, lba, error));
    }
}

impl<P: TxPlatform> BlockDeviceOps for AhciBlock<P> {
    fn durability_capabilities(&self) -> BlockDurabilityCapabilities {
        AHCI_DURABILITY_CAPABILITIES
    }

    fn read_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        target: &mut [Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let mut state = self.state.lock();
        let Some(state) = state.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV);
        };
        if target.is_empty() {
            return StepOutcome::Done(());
        }
        if state.faulted {
            return StepOutcome::Err(Errno::EIO);
        }

        let start = block_id.as_u64();
        let Some(sector_count) = (target.len() as u64).checked_mul(SECTORS_PER_PAGE) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        if validate_io_range(start, sector_count, state.identity.total_blocks).is_err() {
            return StepOutcome::Err(Errno::EINVAL);
        }

        for (index, frame) in target.iter_mut().enumerate() {
            let Some(lba) = start.checked_add(index as u64 * SECTORS_PER_PAGE) else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            if let Err(error) = issue_read_dma_ext::<P>(state, lba) {
                state.faulted = true;
                self.observe_runtime_failure(state, RuntimeOperation::Read, lba, error);
                return StepOutcome::Err(Errno::EIO);
            }
            let Ok(src) = state.workspace.ptr_at(BOUNCE_OFFSET) else {
                return StepOutcome::Err(Errno::EIO);
            };
            let target_start = frame.ppn().0.saturating_mul(PAGE_SIZE);
            let target_end = target_start.saturating_add(PAGE_SIZE);
            let kernel = <P as tx_hal::BootInfoIf>::boot_info().kernel_image;
            let kernel_end = kernel.start.0.saturating_add(kernel.size);
            if target_start < kernel_end && kernel.start.0 < target_end {
                return StepOutcome::Err(Errno::EIO);
            }
            let Ok(dst) = page_allocator::frame_kernel_addr(frame.ppn()) else {
                return StepOutcome::Err(Errno::EIO);
            };
            unsafe {
                core::ptr::copy_nonoverlapping(src.cast_const(), dst, PAGE_SIZE);
            }
        }
        StepOutcome::Done(())
    }

    fn write_blocks(
        &self,
        block_id: PhysicalBlockNumber,
        source: &[Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        let mut state = self.state.lock();
        let Some(state) = state.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV);
        };
        if source.is_empty() {
            return StepOutcome::Done(());
        }
        if state.faulted {
            return StepOutcome::Err(Errno::EIO);
        }

        let start = block_id.as_u64();
        let Some(sector_count) = (source.len() as u64).checked_mul(SECTORS_PER_PAGE) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        if validate_io_range(start, sector_count, state.identity.total_blocks).is_err() {
            return StepOutcome::Err(Errno::EINVAL);
        }

        for (index, frame) in source.iter().enumerate() {
            let Some(lba) = start.checked_add(index as u64 * SECTORS_PER_PAGE) else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            let Ok(src) = frame_kernel_ptr::<P>(frame) else {
                return StepOutcome::Err(Errno::EIO);
            };
            let Ok(dst) = state.workspace.ptr_at(BOUNCE_OFFSET) else {
                return StepOutcome::Err(Errno::EIO);
            };
            unsafe {
                core::ptr::copy_nonoverlapping(src.cast_const(), dst, PAGE_SIZE);
            }
            if let Err(error) = issue_write_dma_ext::<P>(state, lba) {
                state.faulted = true;
                self.observe_runtime_failure(state, RuntimeOperation::Write, lba, error);
                return StepOutcome::Err(Errno::EIO);
            }
        }
        StepOutcome::Done(())
    }

    fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        let mut state = self.state.lock();
        let Some(state) = state.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV);
        };
        if state.faulted {
            return StepOutcome::Err(Errno::EIO);
        }
        if let Err(error) = issue_flush_cache_ext::<P>(state) {
            state.faulted = true;
            self.observe_runtime_failure(state, RuntimeOperation::Flush, 0, error);
            return StepOutcome::Err(Errno::EIO);
        }
        StepOutcome::Done(())
    }
}

impl<P: TxPlatform> BlockDevice for AhciBlock<P> {
    fn total_blocks(&self) -> u64 {
        self.total_blocks.load(Ordering::Acquire)
    }

    fn block_size(&self) -> u32 {
        self.block_size.load(Ordering::Acquire)
    }
}

fn initialize_controller<P: TxPlatform>(
    region: MmioRegion,
    dma_domain: &'static DmaDomain,
) -> Result<AhciState<P>, AhciError> {
    if region.virt.start.0 == 0
        || region.virt.size < regs::PORT_BASE + regs::PORT_STRIDE
        || region.phys.size < regs::PORT_BASE + regs::PORT_STRIDE
    {
        return Err(AhciError::InvalidMmio);
    }
    let regs = VolatileMmio::new(region.virt.start.0);
    let cap = regs.read32(regs::HOST_CAP);
    let version = regs.read32(regs::HOST_VS);
    if cap == u32::MAX || version == 0 || version == u32::MAX {
        return Err(AhciError::InvalidController);
    }

    if regs.read32(regs::HOST_CAP2) & regs::HOST_CAP2_BOH != 0 {
        regs.modify32(regs::HOST_BOHC, 0, regs::HOST_BOHC_OOS);
        if !wait_for_mask::<P, _>(
            &regs,
            regs::HOST_BOHC,
            regs::HOST_BOHC_BOS | regs::HOST_BOHC_BB,
            0,
            HOST_TIMEOUT_NS,
        ) {
            return Err(AhciError::BiosHandoffTimeout);
        }
    }

    regs.modify32(
        regs::HOST_GHC,
        regs::HOST_GHC_IE,
        regs::HOST_GHC_AE | regs::HOST_GHC_HR,
    );
    if !wait_for_mask::<P, _>(&regs, regs::HOST_GHC, regs::HOST_GHC_HR, 0, HOST_TIMEOUT_NS) {
        return Err(AhciError::HostResetTimeout);
    }
    regs.modify32(regs::HOST_GHC, regs::HOST_GHC_IE, regs::HOST_GHC_AE);

    let implemented = regs.read32(regs::HOST_PI);
    if implemented == 0 || implemented == u32::MAX {
        return Err(AhciError::NoImplementedPort);
    }
    let port = implemented.trailing_zeros() as usize;
    power_up_port(&regs, port, cap);
    if !wait_for_link::<P, _>(&regs, port, HOST_TIMEOUT_NS) {
        return Err(AhciError::LinkTimeout);
    }

    stop_port::<P, _>(&regs, port)?;
    let workspace =
        DmaWorkspace::<P>::allocate(dma_domain, WORKSPACE_LEN).map_err(map_dma_allocation_error)?;
    workspace
        .clear(0, WORKSPACE_LEN)
        .map_err(map_dma_runtime_error)?;
    workspace
        .sync_for_device(0, WORKSPACE_LEN, DmaDirection::Bidirectional)
        .map_err(map_dma_runtime_error)?;

    let command_list = workspace
        .dma_addr_at(COMMAND_LIST_OFFSET)
        .map_err(map_dma_runtime_error)?;
    let received_fis = workspace
        .dma_addr_at(RECEIVED_FIS_OFFSET)
        .map_err(map_dma_runtime_error)?;
    let mut state = AhciState {
        regs,
        port,
        workspace: ManuallyDrop::new(workspace),
        identity: AhciIdentity {
            total_blocks: 0,
            logical_block_size: LOGICAL_BLOCK_SIZE,
        },
        faulted: false,
    };
    let port_base = |register| regs::port_reg(port, register);
    regs.write32(port_base(regs::PORT_CLB), command_list as u32);
    regs.write32(port_base(regs::PORT_CLBU), (command_list >> 32) as u32);
    regs.write32(port_base(regs::PORT_FB), received_fis as u32);
    regs.write32(port_base(regs::PORT_FBU), (received_fis >> 32) as u32);
    regs.write32(port_base(regs::PORT_IE), 0);
    clear_port_status(&regs, port);
    <P as DmaIf>::publish_to_device();
    start_port::<P, _>(&regs, port)?;
    if !wait_for_mask::<P, _>(
        &regs,
        port_base(regs::PORT_TFD),
        regs::PORT_TFD_BSY | regs::PORT_TFD_DRQ,
        0,
        HOST_TIMEOUT_NS,
    ) {
        return Err(AhciError::PortBusyTimeout);
    }
    let signature = regs.read32(port_base(regs::PORT_SIG));
    if signature != regs::SATA_SIG_ATA {
        return Err(AhciError::UnsupportedDeviceSignature(signature));
    }

    let identify = issue_identify::<P>(&mut state)?;
    state.identity = parse_identify(&identify)?;
    Ok(state)
}

fn power_up_port<R: AhciRegisterIo>(regs: &R, port: usize, cap: u32) {
    let command = regs::port_reg(port, regs::PORT_CMD);
    let mut set = regs::PORT_CMD_ICC_ACTIVE;
    if cap & regs::HOST_CAP_SSS != 0 {
        set |= regs::PORT_CMD_SUD;
    }
    regs.modify32(command, regs::PORT_CMD_ICC_MASK, set);
    let _ = regs.read32(command);
}

fn stop_port<P: TxPlatform, R: AhciRegisterIo>(regs: &R, port: usize) -> Result<(), AhciError> {
    stop_port_with(
        regs,
        port,
        P::read_ns,
        HOST_TIMEOUT_NS,
        POLL_ITERATION_LIMIT,
    )
}

fn stop_port_with<R: AhciRegisterIo>(
    regs: &R,
    port: usize,
    mut now_ns: impl FnMut() -> u64,
    timeout_ns: u64,
    iteration_limit: usize,
) -> Result<(), AhciError> {
    let command = regs::port_reg(port, regs::PORT_CMD);
    regs.modify32(command, regs::PORT_CMD_ST, 0);
    if !wait_for_mask_with(
        regs,
        command,
        regs::PORT_CMD_CR,
        0,
        &mut now_ns,
        timeout_ns,
        iteration_limit,
    ) {
        return Err(AhciError::PortStopTimeout);
    }
    regs.modify32(command, regs::PORT_CMD_FRE, 0);
    if !wait_for_mask_with(
        regs,
        command,
        regs::PORT_CMD_FR,
        0,
        &mut now_ns,
        timeout_ns,
        iteration_limit,
    ) {
        return Err(AhciError::PortStopTimeout);
    }
    Ok(())
}

fn detach_workspace<P: TxPlatform, R: AhciRegisterIo>(regs: &R, port: usize) -> bool {
    if !detach_workspace_with(
        regs,
        port,
        P::read_ns,
        HOST_TIMEOUT_NS,
        POLL_ITERATION_LIMIT,
    ) {
        return false;
    }
    <P as DmaIf>::publish_to_device();
    true
}

fn detach_workspace_with<R: AhciRegisterIo>(
    regs: &R,
    port: usize,
    now_ns: impl FnMut() -> u64,
    timeout_ns: u64,
    iteration_limit: usize,
) -> bool {
    let port_base = |register| regs::port_reg(port, register);
    regs.write32(port_base(regs::PORT_IE), 0);
    if stop_port_with(regs, port, now_ns, timeout_ns, iteration_limit).is_err() {
        return false;
    }

    clear_port_status(regs, port);
    regs.write32(port_base(regs::PORT_CLB), 0);
    regs.write32(port_base(regs::PORT_CLBU), 0);
    regs.write32(port_base(regs::PORT_FB), 0);
    regs.write32(port_base(regs::PORT_FBU), 0);
    true
}

fn start_port<P: TxPlatform, R: AhciRegisterIo>(regs: &R, port: usize) -> Result<(), AhciError> {
    let command = regs::port_reg(port, regs::PORT_CMD);
    regs.modify32(command, 0, regs::PORT_CMD_FRE);
    if !wait_for_mask::<P, _>(
        regs,
        command,
        regs::PORT_CMD_FR,
        regs::PORT_CMD_FR,
        HOST_TIMEOUT_NS,
    ) {
        return Err(AhciError::PortStartTimeout);
    }
    regs.modify32(command, 0, regs::PORT_CMD_ST);
    Ok(())
}

fn wait_for_link<P: TxPlatform, R: AhciRegisterIo>(regs: &R, port: usize, timeout_ns: u64) -> bool {
    wait_until(
        || {
            let status = regs.read32(regs::port_reg(port, regs::PORT_SSTS));
            status & regs::PORT_SSTS_DET_MASK == regs::PORT_SSTS_DET_PRESENT
                && status & regs::PORT_SSTS_IPM_MASK == regs::PORT_SSTS_IPM_ACTIVE
        },
        P::read_ns,
        timeout_ns,
        POLL_ITERATION_LIMIT,
    )
}

fn wait_for_mask<P: TxPlatform, R: AhciRegisterIo>(
    regs: &R,
    offset: usize,
    mask: u32,
    expected: u32,
    timeout_ns: u64,
) -> bool {
    wait_for_mask_with(
        regs,
        offset,
        mask,
        expected,
        P::read_ns,
        timeout_ns,
        POLL_ITERATION_LIMIT,
    )
}

fn wait_for_mask_with<R: AhciRegisterIo>(
    regs: &R,
    offset: usize,
    mask: u32,
    expected: u32,
    now_ns: impl FnMut() -> u64,
    timeout_ns: u64,
    iteration_limit: usize,
) -> bool {
    wait_until(
        || regs.read32(offset) & mask == expected,
        now_ns,
        timeout_ns,
        iteration_limit,
    )
}

fn wait_until(
    mut predicate: impl FnMut() -> bool,
    mut now_ns: impl FnMut() -> u64,
    timeout_ns: u64,
    iteration_limit: usize,
) -> bool {
    let started = now_ns();
    for _ in 0..iteration_limit {
        if predicate() {
            return true;
        }
        if now_ns().wrapping_sub(started) >= timeout_ns {
            return false;
        }
        core::hint::spin_loop();
    }
    false
}

fn issue_identify<P: TxPlatform>(state: &mut AhciState<P>) -> Result<[u8; 512], AhciError> {
    let fis = identify_fis();
    issue_command::<P>(state, fis, CommandTransfer::DataIn(512), COMMAND_TIMEOUT_NS)?;
    let mut identify = [0u8; 512];
    let source = state
        .workspace
        .ptr_at(BOUNCE_OFFSET)
        .map_err(map_dma_runtime_error)?;
    unsafe {
        core::ptr::copy_nonoverlapping(source.cast_const(), identify.as_mut_ptr(), identify.len());
    }
    Ok(identify)
}

fn issue_read_dma_ext<P: TxPlatform>(state: &mut AhciState<P>, lba: u64) -> Result<(), AhciError> {
    validate_io_range(lba, SECTORS_PER_PAGE, state.identity.total_blocks)?;
    let fis = read_dma_ext_fis(lba, SECTORS_PER_PAGE as u16);
    issue_command::<P>(
        state,
        fis,
        CommandTransfer::DataIn(PAGE_SIZE),
        COMMAND_TIMEOUT_NS,
    )
}

fn issue_write_dma_ext<P: TxPlatform>(state: &mut AhciState<P>, lba: u64) -> Result<(), AhciError> {
    validate_io_range(lba, SECTORS_PER_PAGE, state.identity.total_blocks)?;
    let fis = write_dma_ext_fis(lba, SECTORS_PER_PAGE as u16);
    issue_command::<P>(
        state,
        fis,
        CommandTransfer::DataOut(PAGE_SIZE),
        COMMAND_TIMEOUT_NS,
    )
}

fn issue_flush_cache_ext<P: TxPlatform>(state: &mut AhciState<P>) -> Result<(), AhciError> {
    issue_command::<P>(
        state,
        flush_cache_ext_fis(),
        CommandTransfer::NonData,
        FLUSH_TIMEOUT_NS,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandTransfer {
    DataIn(usize),
    DataOut(usize),
    NonData,
}

impl CommandTransfer {
    const fn data_len(self) -> Option<usize> {
        match self {
            Self::DataIn(len) | Self::DataOut(len) => Some(len),
            Self::NonData => None,
        }
    }

    const fn dma_direction(self) -> Option<DmaDirection> {
        match self {
            Self::DataIn(_) => Some(DmaDirection::FromDevice),
            Self::DataOut(_) => Some(DmaDirection::ToDevice),
            Self::NonData => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CommandLayout {
    header_flags: u16,
    prdt_length: u16,
    prdt_byte_count: Option<u32>,
}

fn command_layout(transfer: CommandTransfer) -> Result<CommandLayout, AhciError> {
    let Some(transfer_len) = transfer.data_len() else {
        return Ok(CommandLayout {
            header_flags: COMMAND_HEADER_CFL_DWORDS,
            prdt_length: 0,
            prdt_byte_count: None,
        });
    };
    if transfer_len == 0 || transfer_len > BOUNCE_LEN {
        return Err(AhciError::OutOfRange);
    }
    let write_flag = if matches!(transfer, CommandTransfer::DataOut(_)) {
        COMMAND_HEADER_WRITE
    } else {
        0
    };
    Ok(CommandLayout {
        header_flags: COMMAND_HEADER_CFL_DWORDS | write_flag,
        prdt_length: 1,
        prdt_byte_count: Some((transfer_len - 1) as u32),
    })
}

fn issue_command<P: TxPlatform>(
    state: &mut AhciState<P>,
    fis: [u8; 20],
    transfer: CommandTransfer,
    timeout_ns: u64,
) -> Result<(), AhciError> {
    let regs = state.regs;
    let port = state.port;
    let port_base = |register| regs::port_reg(port, register);
    if regs.read32(port_base(regs::PORT_CI)) & COMMAND_SLOT != 0
        || regs.read32(port_base(regs::PORT_SACT)) & COMMAND_SLOT != 0
    {
        return Err(AhciError::CommandSlotBusy);
    }
    if !wait_for_mask::<P, _>(
        &regs,
        port_base(regs::PORT_TFD),
        regs::PORT_TFD_BSY | regs::PORT_TFD_DRQ,
        0,
        HOST_TIMEOUT_NS,
    ) {
        return Err(AhciError::PortBusyTimeout);
    }

    prepare_command(&state.workspace, fis, transfer)?;
    clear_port_status(&regs, port);
    state
        .workspace
        .sync_for_device(
            COMMAND_LIST_OFFSET,
            COMMAND_HEADER_LEN,
            DmaDirection::Bidirectional,
        )
        .map_err(map_dma_runtime_error)?;
    state
        .workspace
        .sync_for_device(
            COMMAND_TABLE_OFFSET,
            COMMAND_TABLE_LEN,
            DmaDirection::ToDevice,
        )
        .map_err(map_dma_runtime_error)?;
    state
        .workspace
        .sync_for_device(
            RECEIVED_FIS_OFFSET,
            RECEIVED_FIS_LEN,
            DmaDirection::FromDevice,
        )
        .map_err(map_dma_runtime_error)?;
    if let (Some(transfer_len), Some(direction)) = (transfer.data_len(), transfer.dma_direction()) {
        state
            .workspace
            .sync_for_device(BOUNCE_OFFSET, transfer_len, direction)
            .map_err(map_dma_runtime_error)?;
    }
    <P as DmaIf>::publish_to_device();
    regs.write32(port_base(regs::PORT_CI), COMMAND_SLOT);

    poll_command_with(&regs, port, P::read_ns, timeout_ns, POLL_ITERATION_LIMIT)?;
    state
        .workspace
        .sync_for_cpu(
            COMMAND_LIST_OFFSET,
            COMMAND_HEADER_LEN,
            DmaDirection::FromDevice,
        )
        .map_err(map_dma_runtime_error)?;
    state
        .workspace
        .sync_for_cpu(
            RECEIVED_FIS_OFFSET,
            RECEIVED_FIS_LEN,
            DmaDirection::FromDevice,
        )
        .map_err(map_dma_runtime_error)?;
    if let CommandTransfer::DataIn(transfer_len) = transfer {
        state
            .workspace
            .sync_for_cpu(BOUNCE_OFFSET, transfer_len, DmaDirection::FromDevice)
            .map_err(map_dma_runtime_error)?;
    }
    fence(Ordering::Acquire);

    let transferred = if transfer.data_len().is_some() {
        Some(read_command_byte_count(&state.workspace)?)
    } else {
        None
    };
    clear_port_status(&regs, port);
    if let (Some(transferred), Some(expected)) = (transferred, transfer.data_len()) {
        if transferred != expected as u32 {
            return Err(AhciError::TaskFileError);
        }
    }
    Ok(())
}

fn prepare_command<P: TxPlatform>(
    workspace: &DmaWorkspace<P>,
    fis: [u8; 20],
    transfer: CommandTransfer,
) -> Result<(), AhciError> {
    let layout = command_layout(transfer)?;
    workspace
        .clear(COMMAND_LIST_OFFSET, COMMAND_LIST_LEN)
        .map_err(map_dma_runtime_error)?;
    workspace
        .clear(RECEIVED_FIS_OFFSET, RECEIVED_FIS_LEN)
        .map_err(map_dma_runtime_error)?;
    workspace
        .clear(COMMAND_TABLE_OFFSET, COMMAND_TABLE_LEN)
        .map_err(map_dma_runtime_error)?;

    let command_table = workspace
        .dma_addr_at(COMMAND_TABLE_OFFSET)
        .map_err(map_dma_runtime_error)?;
    let header = CommandHeader {
        flags: layout.header_flags,
        prdt_length: layout.prdt_length,
        prd_byte_count: 0,
        command_table_base: command_table as u32,
        command_table_base_upper: (command_table >> 32) as u32,
        reserved: [0; 4],
    };
    unsafe {
        core::ptr::write(
            workspace
                .ptr_at(COMMAND_LIST_OFFSET)
                .map_err(map_dma_runtime_error)?
                .cast::<CommandHeader>(),
            header,
        );
        core::ptr::copy_nonoverlapping(
            fis.as_ptr(),
            workspace
                .ptr_at(COMMAND_FIS_OFFSET)
                .map_err(map_dma_runtime_error)?,
            fis.len(),
        );
        if let Some(byte_count_and_interrupt) = layout.prdt_byte_count {
            let bounce = workspace
                .dma_addr_at(BOUNCE_OFFSET)
                .map_err(map_dma_runtime_error)?;
            let prdt = PrdtEntry {
                data_base: bounce as u32,
                data_base_upper: (bounce >> 32) as u32,
                reserved: 0,
                byte_count_and_interrupt,
            };
            core::ptr::write(
                workspace
                    .ptr_at(PRDT_OFFSET)
                    .map_err(map_dma_runtime_error)?
                    .cast::<PrdtEntry>(),
                prdt,
            );
        }
    }
    Ok(())
}

fn read_command_byte_count<P: TxPlatform>(workspace: &DmaWorkspace<P>) -> Result<u32, AhciError> {
    let header = workspace
        .ptr_at(COMMAND_LIST_OFFSET)
        .map_err(map_dma_runtime_error)?
        .cast::<CommandHeader>();
    Ok(unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*header).prd_byte_count)) })
}

fn capture_runtime_failure<P: TxPlatform>(
    state: &AhciState<P>,
    operation: RuntimeOperation,
    lba: u64,
    error: AhciError,
) -> RuntimeFailureSnapshot {
    let port_base = |register| regs::port_reg(state.port, register);
    RuntimeFailureSnapshot {
        operation,
        lba,
        error,
        interrupt_status: state.regs.read32(port_base(regs::PORT_IS)),
        task_file: state.regs.read32(port_base(regs::PORT_TFD)),
        sata_error: state.regs.read32(port_base(regs::PORT_SERR)),
        command_issue: state.regs.read32(port_base(regs::PORT_CI)),
    }
}

fn report_runtime_failure<P: TxPlatform>(snapshot: RuntimeFailureSnapshot) {
    let mut line = [0u8; 256];
    let len = format_runtime_failure(snapshot, &mut line);
    tx_hal::console_write_bytes::<P>(&line[..len]);
}

fn format_runtime_failure(snapshot: RuntimeFailureSnapshot, out: &mut [u8]) -> usize {
    let mut len = 0;
    append_bytes(out, &mut len, b"txkernel:ahci:io-error:op=");
    append_bytes(out, &mut len, snapshot.operation.label().as_bytes());
    append_bytes(out, &mut len, b":lba=");
    append_hex(out, &mut len, snapshot.lba, 16);
    append_bytes(out, &mut len, b":error=");
    append_bytes(out, &mut len, snapshot.error.label().as_bytes());
    append_bytes(out, &mut len, b":is=");
    append_hex(out, &mut len, u64::from(snapshot.interrupt_status), 8);
    append_bytes(out, &mut len, b":tfd=");
    append_hex(out, &mut len, u64::from(snapshot.task_file), 8);
    append_bytes(out, &mut len, b":serr=");
    append_hex(out, &mut len, u64::from(snapshot.sata_error), 8);
    append_bytes(out, &mut len, b":ci=");
    append_hex(out, &mut len, u64::from(snapshot.command_issue), 8);
    append_bytes(out, &mut len, b"\n");
    len
}

fn append_bytes(out: &mut [u8], len: &mut usize, bytes: &[u8]) {
    let remaining = out.len().saturating_sub(*len);
    let count = remaining.min(bytes.len());
    out[*len..*len + count].copy_from_slice(&bytes[..count]);
    *len += count;
}

fn append_hex(out: &mut [u8], len: &mut usize, value: u64, digits: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    append_bytes(out, len, b"0x");
    for index in 0..digits {
        let shift = (digits - index - 1) * 4;
        let nibble = ((value >> shift) & 0xf) as usize;
        append_bytes(out, len, &HEX[nibble..nibble + 1]);
    }
}

fn clear_port_status<R: AhciRegisterIo>(regs: &R, port: usize) {
    let interrupt_status = regs::port_reg(port, regs::PORT_IS);
    let sata_error = regs::port_reg(port, regs::PORT_SERR);
    regs.write32(interrupt_status, u32::MAX);
    regs.write32(sata_error, u32::MAX);
    regs.write32(regs::HOST_IS, 1u32 << port);
}

fn poll_command_with<R: AhciRegisterIo>(
    regs: &R,
    port: usize,
    mut now_ns: impl FnMut() -> u64,
    timeout_ns: u64,
    iteration_limit: usize,
) -> Result<(), AhciError> {
    let started = now_ns();
    let mut last_now = started;
    let mut stagnant_iterations = 0usize;
    let command_issue = regs::port_reg(port, regs::PORT_CI);
    let interrupt_status = regs::port_reg(port, regs::PORT_IS);
    let task_file = regs::port_reg(port, regs::PORT_TFD);
    loop {
        let port_is = regs.read32(interrupt_status);
        let tfd = regs.read32(task_file);
        if port_is & regs::PORT_IS_ERROR != 0 || tfd & regs::PORT_TFD_ERR != 0 {
            return Err(AhciError::TaskFileError);
        }
        if regs.read32(command_issue) & COMMAND_SLOT == 0 {
            return Ok(());
        }
        let now = now_ns();
        if now.wrapping_sub(started) >= timeout_ns {
            return Err(AhciError::CommandTimeout);
        }
        if now == last_now {
            stagnant_iterations = stagnant_iterations.saturating_add(1);
            if stagnant_iterations >= iteration_limit {
                return Err(AhciError::CommandTimeout);
            }
        } else {
            last_now = now;
            stagnant_iterations = 0;
        }
        core::hint::spin_loop();
    }
}

fn identify_fis() -> [u8; 20] {
    register_h2d_fis(ATA_CMD_IDENTIFY_DEVICE, 0, 0, false)
}

fn read_dma_ext_fis(lba: u64, sectors: u16) -> [u8; 20] {
    register_h2d_fis(ATA_CMD_READ_DMA_EXT, lba, sectors, true)
}

fn write_dma_ext_fis(lba: u64, sectors: u16) -> [u8; 20] {
    register_h2d_fis(ATA_CMD_WRITE_DMA_EXT, lba, sectors, true)
}

fn flush_cache_ext_fis() -> [u8; 20] {
    register_h2d_fis(ATA_CMD_FLUSH_CACHE_EXT, 0, 0, false)
}

fn register_h2d_fis(command: u8, lba: u64, sectors: u16, lba_mode: bool) -> [u8; 20] {
    let mut fis = [0u8; 20];
    fis[0] = FIS_TYPE_REG_H2D;
    fis[1] = FIS_COMMAND;
    fis[2] = command;
    fis[4] = lba as u8;
    fis[5] = (lba >> 8) as u8;
    fis[6] = (lba >> 16) as u8;
    fis[7] = if lba_mode { ATA_DEVICE_LBA } else { 0 };
    fis[8] = (lba >> 24) as u8;
    fis[9] = (lba >> 32) as u8;
    fis[10] = (lba >> 40) as u8;
    fis[12] = sectors as u8;
    fis[13] = (sectors >> 8) as u8;
    fis
}

fn validate_io_range(start: u64, sector_count: u64, total_blocks: u64) -> Result<(), AhciError> {
    let end = start
        .checked_add(sector_count)
        .ok_or(AhciError::OutOfRange)?;
    if start >> 48 != 0 || end > (1u64 << 48) || end > total_blocks {
        return Err(AhciError::OutOfRange);
    }
    Ok(())
}

fn frame_kernel_ptr<P: TxPlatform>(frame: &Frame) -> Result<*mut u8, AhciError> {
    let frame_start = frame
        .ppn()
        .0
        .checked_mul(PAGE_SIZE)
        .ok_or(AhciError::FrameMapping)?;
    let frame_end = frame_start
        .checked_add(PAGE_SIZE)
        .ok_or(AhciError::FrameMapping)?;
    let kernel = <P as tx_hal::BootInfoIf>::boot_info().kernel_image;
    let kernel_end = kernel
        .start
        .0
        .checked_add(kernel.size)
        .ok_or(AhciError::FrameMapping)?;
    if frame_start < kernel_end && kernel.start.0 < frame_end {
        return Err(AhciError::DmaKernelImageOverlap);
    }
    page_allocator::frame_kernel_addr(frame.ppn()).map_err(|_| AhciError::FrameMapping)
}

fn parse_identify(bytes: &[u8; 512]) -> Result<AhciIdentity, AhciError> {
    let word = |index: usize| {
        let offset = index * 2;
        u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
    };
    let word_106 = word(106);
    let logical_block_size = if word_106 >> 14 == 1 && word_106 & (1 << 12) != 0 {
        let words_per_sector = u32::from(word(117)) | (u32::from(word(118)) << 16);
        words_per_sector
            .checked_mul(2)
            .ok_or(AhciError::InvalidIdentify)?
    } else {
        LOGICAL_BLOCK_SIZE
    };
    if logical_block_size != LOGICAL_BLOCK_SIZE {
        return Err(AhciError::UnsupportedLogicalBlockSize(logical_block_size));
    }

    let lba48_supported = word(83) & (1 << 10) != 0;
    if !lba48_supported {
        return Err(AhciError::UnsupportedAddressing);
    }
    let total_blocks = u64::from(word(100))
        | (u64::from(word(101)) << 16)
        | (u64::from(word(102)) << 32)
        | (u64::from(word(103)) << 48);
    if total_blocks == 0 {
        return Err(AhciError::InvalidIdentify);
    }
    Ok(AhciIdentity {
        total_blocks,
        logical_block_size,
    })
}

fn map_dma_allocation_error(_error: DmaWorkspaceError) -> AhciError {
    match _error {
        DmaWorkspaceError::KernelImageOverlap => AhciError::DmaKernelImageOverlap,
        _ => AhciError::DmaAllocation,
    }
}

fn map_dma_runtime_error(error: DmaWorkspaceError) -> AhciError {
    match error {
        DmaWorkspaceError::AddressOutside32Bit | DmaWorkspaceError::TranslationOverflow => {
            AhciError::DmaAddress
        }
        DmaWorkspaceError::KernelImageOverlap => AhciError::DmaKernelImageOverlap,
        _ => AhciError::DmaAllocation,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CommandHeader {
    flags: u16,
    prdt_length: u16,
    prd_byte_count: u32,
    command_table_base: u32,
    command_table_base_upper: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PrdtEntry {
    data_base: u32,
    data_base_upper: u32,
    reserved: u32,
    byte_count_and_interrupt: u32,
}

const COMMAND_HEADER_LEN: usize = core::mem::size_of::<CommandHeader>();

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};

    struct MockRegisters {
        words: RefCell<alloc::vec::Vec<u32>>,
        ci_reads: Cell<usize>,
        clear_ci_after: Cell<Option<usize>>,
    }

    impl MockRegisters {
        fn new() -> Self {
            Self {
                words: RefCell::new(alloc::vec![0; 0x200 / 4]),
                ci_reads: Cell::new(0),
                clear_ci_after: Cell::new(None),
            }
        }

        fn set(&self, offset: usize, value: u32) {
            self.words.borrow_mut()[offset / 4] = value;
        }
    }

    impl AhciRegisterIo for MockRegisters {
        fn read32(&self, offset: usize) -> u32 {
            if offset == regs::port_reg(0, regs::PORT_CI) {
                let reads = self.ci_reads.get() + 1;
                self.ci_reads.set(reads);
                if self
                    .clear_ci_after
                    .get()
                    .is_some_and(|threshold| reads >= threshold)
                {
                    self.set(offset, 0);
                }
            }
            self.words.borrow()[offset / 4]
        }

        fn write32(&self, offset: usize, value: u32) {
            self.set(offset, value);
        }
    }

    #[test]
    fn command_structures_match_ahci_layout() {
        assert_eq!(core::mem::size_of::<CommandHeader>(), 32);
        assert_eq!(core::mem::size_of::<PrdtEntry>(), 16);
        assert_eq!(COMMAND_LIST_OFFSET % 1024, 0);
        assert_eq!(RECEIVED_FIS_OFFSET % 256, 0);
        assert_eq!(COMMAND_TABLE_OFFSET % 128, 0);
        assert!(PRDT_OFFSET + core::mem::size_of::<PrdtEntry>() <= BOUNCE_OFFSET);
        assert_eq!(WORKSPACE_LEN, 8192);
    }

    #[test]
    fn read_dma_ext_fis_encodes_lba48_and_sector_count() {
        let fis = read_dma_ext_fis(0x1234_5678_9abc, 8);
        assert_eq!(fis[0], FIS_TYPE_REG_H2D);
        assert_eq!(fis[1], FIS_COMMAND);
        assert_eq!(fis[2], ATA_CMD_READ_DMA_EXT);
        assert_eq!(&fis[4..7], &[0xbc, 0x9a, 0x78]);
        assert_eq!(fis[7], ATA_DEVICE_LBA);
        assert_eq!(&fis[8..11], &[0x56, 0x34, 0x12]);
        assert_eq!(&fis[12..14], &[8, 0]);
    }

    #[test]
    fn write_dma_ext_fis_encodes_lba48_and_sector_count() {
        let fis = write_dma_ext_fis(0x1234_5678_9abc, 8);
        assert_eq!(fis[0], FIS_TYPE_REG_H2D);
        assert_eq!(fis[1], FIS_COMMAND);
        assert_eq!(fis[2], ATA_CMD_WRITE_DMA_EXT);
        assert_eq!(&fis[4..7], &[0xbc, 0x9a, 0x78]);
        assert_eq!(fis[7], ATA_DEVICE_LBA);
        assert_eq!(&fis[8..11], &[0x56, 0x34, 0x12]);
        assert_eq!(&fis[12..14], &[8, 0]);
    }

    #[test]
    fn command_header_direction_and_prdt_shape_match_protocol() {
        let read = command_layout(CommandTransfer::DataIn(PAGE_SIZE)).unwrap();
        assert_eq!(read.header_flags, COMMAND_HEADER_CFL_DWORDS);
        assert_eq!(read.prdt_length, 1);
        assert_eq!(read.prdt_byte_count, Some((PAGE_SIZE - 1) as u32));
        assert_eq!(
            CommandTransfer::DataIn(PAGE_SIZE).dma_direction(),
            Some(DmaDirection::FromDevice)
        );

        let write = command_layout(CommandTransfer::DataOut(PAGE_SIZE)).unwrap();
        assert_eq!(
            write.header_flags,
            COMMAND_HEADER_CFL_DWORDS | COMMAND_HEADER_WRITE
        );
        assert_eq!(write.prdt_length, 1);
        assert_eq!(write.prdt_byte_count, Some((PAGE_SIZE - 1) as u32));
        assert_eq!(
            CommandTransfer::DataOut(PAGE_SIZE).dma_direction(),
            Some(DmaDirection::ToDevice)
        );
    }

    #[test]
    fn flush_cache_ext_is_non_data_and_has_no_prdt() {
        let fis = flush_cache_ext_fis();
        assert_eq!(fis[0], FIS_TYPE_REG_H2D);
        assert_eq!(fis[1], FIS_COMMAND);
        assert_eq!(fis[2], ATA_CMD_FLUSH_CACHE_EXT);

        let flush = command_layout(CommandTransfer::NonData).unwrap();
        assert_eq!(flush.header_flags, COMMAND_HEADER_CFL_DWORDS);
        assert_eq!(flush.prdt_length, 0);
        assert_eq!(flush.prdt_byte_count, None);
        assert_eq!(CommandTransfer::NonData.dma_direction(), None);
    }

    #[test]
    fn durability_declares_flush_without_fua() {
        assert_eq!(
            AHCI_DURABILITY_CAPABILITIES,
            BlockDurabilityCapabilities {
                fua: false,
                flush: true,
            }
        );
    }

    #[test]
    fn io_range_validation_rejects_overflow_capacity_and_non_lba48() {
        assert_eq!(validate_io_range(8, 8, 16), Ok(()));
        assert_eq!(
            validate_io_range(u64::MAX - 3, 8, u64::MAX),
            Err(AhciError::OutOfRange)
        );
        assert_eq!(validate_io_range(9, 8, 16), Err(AhciError::OutOfRange));
        assert_eq!(
            validate_io_range(1 << 48, 1, u64::MAX),
            Err(AhciError::OutOfRange)
        );
    }

    #[test]
    fn identify_prefers_lba48_capacity() {
        let mut identify = [0u8; 512];
        set_identify_word(&mut identify, 83, 1 << 10);
        set_identify_word(&mut identify, 100, 0x5678);
        set_identify_word(&mut identify, 101, 0x1234);
        let parsed = parse_identify(&identify).unwrap();
        assert_eq!(parsed.total_blocks, 0x1234_5678);
        assert_eq!(parsed.logical_block_size, 512);
    }

    #[test]
    fn identify_rejects_non_512_logical_sectors() {
        let mut identify = [0u8; 512];
        set_identify_word(&mut identify, 83, 1 << 10);
        set_identify_word(&mut identify, 100, 1);
        set_identify_word(&mut identify, 106, (1 << 14) | (1 << 12));
        set_identify_word(&mut identify, 117, 2048);
        assert_eq!(
            parse_identify(&identify),
            Err(AhciError::UnsupportedLogicalBlockSize(4096))
        );
    }

    #[test]
    fn identify_rejects_non_lba48_devices() {
        let mut identify = [0u8; 512];
        set_identify_word(&mut identify, 60, 1);
        assert_eq!(
            parse_identify(&identify),
            Err(AhciError::UnsupportedAddressing)
        );
    }

    #[test]
    fn command_poll_observes_ci_completion() {
        let regs = MockRegisters::new();
        regs.set(regs::port_reg(0, regs::PORT_CI), COMMAND_SLOT);
        regs.clear_ci_after.set(Some(3));
        let ticks = Cell::new(0u64);
        let result = poll_command_with(
            &regs,
            0,
            || {
                let next = ticks.get() + 1;
                ticks.set(next);
                next
            },
            100,
            10,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(regs.ci_reads.get(), 3);
    }

    #[test]
    fn command_poll_reports_task_file_error_before_timeout() {
        let regs = MockRegisters::new();
        regs.set(regs::port_reg(0, regs::PORT_CI), COMMAND_SLOT);
        regs.set(regs::port_reg(0, regs::PORT_IS), regs::PORT_IS_TFES);
        assert_eq!(
            poll_command_with(&regs, 0, || 0, 100, 10),
            Err(AhciError::TaskFileError)
        );
    }

    #[test]
    fn command_poll_has_iteration_fallback_when_clock_stalls() {
        let regs = MockRegisters::new();
        regs.set(regs::port_reg(0, regs::PORT_CI), COMMAND_SLOT);
        assert_eq!(
            poll_command_with(&regs, 0, || 7, u64::MAX, 4),
            Err(AhciError::CommandTimeout)
        );
        assert_eq!(regs.ci_reads.get(), 4);
    }

    #[test]
    fn command_poll_does_not_cap_a_progressing_clock_by_total_iterations() {
        let regs = MockRegisters::new();
        regs.set(regs::port_reg(0, regs::PORT_CI), COMMAND_SLOT);
        regs.clear_ci_after.set(Some(6));
        let ticks = Cell::new(0u64);
        assert_eq!(
            poll_command_with(
                &regs,
                0,
                || {
                    let next = ticks.get() + 1;
                    ticks.set(next);
                    next
                },
                100,
                2,
            ),
            Ok(())
        );
        assert_eq!(regs.ci_reads.get(), 6);
    }

    #[test]
    fn runtime_failure_observation_latches_first_error() {
        let observation = RuntimeFailureObservation::new();
        assert!(observation.observe());
        assert!(!observation.observe());
        assert_eq!(observation.count.load(Ordering::Relaxed), 2);
        assert!(observation.first_reported.load(Ordering::Acquire));
    }

    #[test]
    fn successful_commands_do_not_emit_runtime_failure() {
        let observation = RuntimeFailureObservation::new();
        assert_eq!(observation.count.load(Ordering::Relaxed), 0);
        assert!(!observation.first_reported.load(Ordering::Acquire));
    }

    #[test]
    fn runtime_failure_record_keeps_the_first_hardware_witness_compact() {
        let snapshot = RuntimeFailureSnapshot {
            operation: RuntimeOperation::Write,
            lba: 0x1234,
            error: AhciError::TaskFileError,
            interrupt_status: 0x4000_0000,
            task_file: 0x51,
            sata_error: 0x10,
            command_issue: 1,
        };
        let mut line = [0u8; 256];
        let len = format_runtime_failure(snapshot, &mut line);
        let line = core::str::from_utf8(&line[..len]).unwrap();
        assert_eq!(
            line,
            "txkernel:ahci:io-error:op=write:lba=0x0000000000001234:error=task-file-error:is=0x40000000:tfd=0x00000051:serr=0x00000010:ci=0x00000001\n"
        );
    }

    #[test]
    fn register_wait_is_deterministic_with_injected_clock() {
        let regs = MockRegisters::new();
        regs.set(regs::HOST_GHC, regs::HOST_GHC_HR);
        let ticks = Cell::new(0u64);
        assert!(!wait_for_mask_with(
            &regs,
            regs::HOST_GHC,
            regs::HOST_GHC_HR,
            0,
            || {
                let next = ticks.get() + 10;
                ticks.set(next);
                next
            },
            25,
            10,
        ));
    }

    #[test]
    fn workspace_detach_clears_dma_bases_only_after_port_stops() {
        let regs = MockRegisters::new();
        let port_base = |register| regs::port_reg(0, register);
        regs.set(
            port_base(regs::PORT_CMD),
            regs::PORT_CMD_ST | regs::PORT_CMD_FRE,
        );
        regs.set(port_base(regs::PORT_IE), u32::MAX);
        regs.set(port_base(regs::PORT_CLB), 0x1000);
        regs.set(port_base(regs::PORT_CLBU), 0x2000);
        regs.set(port_base(regs::PORT_FB), 0x3000);
        regs.set(port_base(regs::PORT_FBU), 0x4000);

        assert!(detach_workspace_with(&regs, 0, || 0, 100, 4));
        assert_eq!(regs.read32(port_base(regs::PORT_IE)), 0);
        assert_eq!(regs.read32(port_base(regs::PORT_CLB)), 0);
        assert_eq!(regs.read32(port_base(regs::PORT_CLBU)), 0);
        assert_eq!(regs.read32(port_base(regs::PORT_FB)), 0);
        assert_eq!(regs.read32(port_base(regs::PORT_FBU)), 0);
    }

    #[test]
    fn workspace_detach_keeps_dma_bases_when_port_cannot_stop() {
        let regs = MockRegisters::new();
        let port_base = |register| regs::port_reg(0, register);
        regs.set(
            port_base(regs::PORT_CMD),
            regs::PORT_CMD_ST | regs::PORT_CMD_FRE | regs::PORT_CMD_CR,
        );
        regs.set(port_base(regs::PORT_IE), u32::MAX);
        regs.set(port_base(regs::PORT_CLB), 0x1000);
        regs.set(port_base(regs::PORT_FB), 0x3000);

        assert!(!detach_workspace_with(&regs, 0, || 0, u64::MAX, 2));
        assert_eq!(regs.read32(port_base(regs::PORT_IE)), 0);
        assert_eq!(regs.read32(port_base(regs::PORT_CLB)), 0x1000);
        assert_eq!(regs.read32(port_base(regs::PORT_FB)), 0x3000);
    }

    fn set_identify_word(bytes: &mut [u8; 512], index: usize, value: u16) {
        bytes[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
    }
}
