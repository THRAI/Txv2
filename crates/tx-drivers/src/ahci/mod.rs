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
use tx_subsystems::device::{BlockDevice, BlockDeviceOps, PhysicalBlockNumber};
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
const COMMAND_SLOT: u32 = 1;
const ATA_CMD_IDENTIFY_DEVICE: u8 = 0xec;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const FIS_TYPE_REG_H2D: u8 = 0x27;
const FIS_COMMAND: u8 = 1 << 7;
const ATA_DEVICE_LBA: u8 = 1 << 6;

const HOST_TIMEOUT_NS: u64 = 1_000_000_000;
const COMMAND_TIMEOUT_NS: u64 = 5_000_000_000;
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
    _platform: PhantomData<fn() -> P>,
}

struct AhciState<P: TxPlatform> {
    regs: VolatileMmio,
    port: usize,
    workspace: ManuallyDrop<DmaWorkspace<P>>,
    identity: AhciIdentity,
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
}

impl<P: TxPlatform> BlockDeviceOps for AhciBlock<P> {
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

        let start = block_id.as_u64();
        let Some(sector_count) = (target.len() as u64).checked_mul(SECTORS_PER_PAGE) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        let Some(end) = start.checked_add(sector_count) else {
            return StepOutcome::Err(Errno::EINVAL);
        };
        if end > state.identity.total_blocks {
            return StepOutcome::Err(Errno::EINVAL);
        }

        for (index, frame) in target.iter_mut().enumerate() {
            let Some(lba) = start.checked_add(index as u64 * SECTORS_PER_PAGE) else {
                return StepOutcome::Err(Errno::EINVAL);
            };
            if issue_read_dma_ext::<P>(state, lba).is_err() {
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
        _block_id: PhysicalBlockNumber,
        _source: &[Frame],
        _guard: &Guard<'_>,
    ) -> StepOutcome<(), NoProgress> {
        StepOutcome::Err(Errno::EROFS)
    }

    fn barrier(&self, _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        StepOutcome::Err(Errno::EROFS)
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
    issue_data_in_command::<P>(state, fis, 512)?;
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
    if lba >> 48 != 0 {
        return Err(AhciError::OutOfRange);
    }
    let fis = read_dma_ext_fis(lba, SECTORS_PER_PAGE as u16);
    issue_data_in_command::<P>(state, fis, PAGE_SIZE)
}

fn issue_data_in_command<P: TxPlatform>(
    state: &mut AhciState<P>,
    fis: [u8; 20],
    transfer_len: usize,
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

    prepare_data_in_command(&state.workspace, fis, transfer_len)?;
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
    state
        .workspace
        .sync_for_device(BOUNCE_OFFSET, transfer_len, DmaDirection::FromDevice)
        .map_err(map_dma_runtime_error)?;
    <P as DmaIf>::publish_to_device();
    regs.write32(port_base(regs::PORT_CI), COMMAND_SLOT);

    poll_command_with(
        &regs,
        port,
        P::read_ns,
        COMMAND_TIMEOUT_NS,
        POLL_ITERATION_LIMIT,
    )?;
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
    state
        .workspace
        .sync_for_cpu(BOUNCE_OFFSET, transfer_len, DmaDirection::FromDevice)
        .map_err(map_dma_runtime_error)?;
    fence(Ordering::Acquire);

    let transferred = read_command_byte_count(&state.workspace)?;
    clear_port_status(&regs, port);
    if transferred != transfer_len as u32 {
        return Err(AhciError::TaskFileError);
    }
    Ok(())
}

fn prepare_data_in_command<P: TxPlatform>(
    workspace: &DmaWorkspace<P>,
    fis: [u8; 20],
    transfer_len: usize,
) -> Result<(), AhciError> {
    if transfer_len == 0 || transfer_len > BOUNCE_LEN {
        return Err(AhciError::OutOfRange);
    }
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
    let bounce = workspace
        .dma_addr_at(BOUNCE_OFFSET)
        .map_err(map_dma_runtime_error)?;
    let header = CommandHeader {
        flags: COMMAND_HEADER_CFL_DWORDS,
        prdt_length: 1,
        prd_byte_count: 0,
        command_table_base: command_table as u32,
        command_table_base_upper: (command_table >> 32) as u32,
        reserved: [0; 4],
    };
    let prdt = PrdtEntry {
        data_base: bounce as u32,
        data_base_upper: (bounce >> 32) as u32,
        reserved: 0,
        byte_count_and_interrupt: (transfer_len - 1) as u32,
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
        core::ptr::write(
            workspace
                .ptr_at(PRDT_OFFSET)
                .map_err(map_dma_runtime_error)?
                .cast::<PrdtEntry>(),
            prdt,
        );
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
    let command_issue = regs::port_reg(port, regs::PORT_CI);
    let interrupt_status = regs::port_reg(port, regs::PORT_IS);
    let task_file = regs::port_reg(port, regs::PORT_TFD);
    for _ in 0..iteration_limit {
        let port_is = regs.read32(interrupt_status);
        let tfd = regs.read32(task_file);
        if port_is & regs::PORT_IS_ERROR != 0 || tfd & regs::PORT_TFD_ERR != 0 {
            return Err(AhciError::TaskFileError);
        }
        if regs.read32(command_issue) & COMMAND_SLOT == 0 {
            return Ok(());
        }
        if now_ns().wrapping_sub(started) >= timeout_ns {
            return Err(AhciError::CommandTimeout);
        }
        core::hint::spin_loop();
    }
    Err(AhciError::CommandTimeout)
}

fn identify_fis() -> [u8; 20] {
    register_h2d_fis(ATA_CMD_IDENTIFY_DEVICE, 0, 0, false)
}

fn read_dma_ext_fis(lba: u64, sectors: u16) -> [u8; 20] {
    register_h2d_fis(ATA_CMD_READ_DMA_EXT, lba, sectors, true)
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
    let total_blocks = if lba48_supported {
        u64::from(word(100))
            | (u64::from(word(101)) << 16)
            | (u64::from(word(102)) << 32)
            | (u64::from(word(103)) << 48)
    } else {
        u64::from(word(60)) | (u64::from(word(61)) << 16)
    };
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
