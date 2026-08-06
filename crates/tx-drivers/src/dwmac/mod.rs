//! Synopsys DWMAC 5.x/EQoS network device core.
//!
//! The core consumes one already-decoded MMIO region and DMA domain. Board and
//! firmware policy stays in the static binder adapter; this module only owns
//! standard MAC, MTL, DMA-ring, and Clause-22 MDIO mechanics.

mod dma;
mod regs;

use alloc::vec::Vec;
use core::marker::PhantomData;
use core::sync::atomic::{fence, AtomicBool, AtomicU64, Ordering};

use crate::adapter::step_engine::{NoProgress, SpinMutex, StepOutcome};
use dma::{DmaBuffer, DmaBufferError};
use regs::*;
use tx_hal::{DmaDirection, DmaDomain, DmaIf, MmioRegion, TxPlatform};
use tx_substrate::step::Errno;
use tx_subsystems::execution::Guard;
use tx_subsystems::net::device::{EthernetAddress, NetDeviceIrqOutcome, NetDeviceOps};
use tx_subsystems::net::packet::{PacketTxReadiness, RxFrame};

const TX_RING_LEN: usize = 8;
// Interrupt-driven receive needs enough descriptors to absorb a network burst
// while the delegated network task is waiting for reactor time. Keep this
// above the 32-frame software poll budget so the task can refill a substantial
// portion of the ring before hardware reaches the one-descriptor boundary.
const RX_RING_LEN: usize = 64;
const DESC_SIZE: usize = 16;
const DESC_STRIDE: usize = 64;
// JH7110's DWMAC AXI data bus is 64-bit. The descriptor-skip-length
// register is expressed in AXI bus-width units, not bytes.
const AXI_BUS_WIDTH_BYTES: usize = 8;
const DESC_SKIP_LENGTH: usize = (DESC_STRIDE - DESC_SIZE) / AXI_BUS_WIDTH_BYTES;
const RX_DESC_OFFSET: usize = TX_RING_LEN * DESC_STRIDE;
const DESCRIPTOR_STORAGE_LEN: usize = RX_DESC_OFFSET + RX_RING_LEN * DESC_STRIDE;
const PACKET_BUFFER_SIZE: usize = 2048;
const DEFAULT_MTU: u16 = 1500;
const RESET_POLL_LIMIT: usize = 1_000_000;
const MDIO_POLL_LIMIT: usize = 100_000;

const DESC3_OWN: u32 = 1 << 31;
const DESC3_RX_IOC: u32 = 1 << 30;
const DESC3_FD: u32 = 1 << 29;
const DESC3_LD: u32 = 1 << 28;
const DESC3_BUF1V: u32 = 1 << 24;
const DESC3_PACKET_LEN_MASK: u32 = 0x7fff;
const DESC2_TX_IOC: u32 = 1 << 31;

pub struct DwmacNet<P: TxPlatform> {
    region: MmioRegion,
    dma_domain: &'static DmaDomain,
    state: SpinMutex<Option<DwmacState<P>>>,
    initialized: AtomicBool,
    mac: AtomicU64,
    _platform: PhantomData<fn() -> P>,
}

struct DwmacState<P: TxPlatform> {
    regs: RegisterBlock,
    descriptors: DmaBuffer<P>,
    rx_buffers: DmaBuffer<P>,
    tx_buffers: DmaBuffer<P>,
    rx_index: usize,
    tx_index: usize,
    /// Byte offset of the exclusive RX descriptor boundary last published to
    /// DMA. The initial boundary is one descriptor past the ring; after the
    /// first refill it follows the software refill cursor around the ring.
    rx_tail_offset: usize,
    rx_stall_capture_count: usize,
    pending_rx_stall: Option<RxDmaStallSnapshot>,
    rx_irqs_masked: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RxDmaStallSnapshot {
    status: u32,
    rx_control: u32,
    current_rx_desc: u32,
    current_rx_buf: u32,
    rx_tail: u32,
    software_rx_index: u32,
    descriptor_word3: u32,
    sequence: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PhyLink {
    address: u8,
    speed: LinkSpeed,
    full_duplex: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LinkSpeed {
    Mbps10,
    Mbps100,
    Mbps1000,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DwmacError {
    InvalidMmio,
    UnsupportedVersion(u32),
    InvalidMacAddress,
    ResetTimeout,
    DmaAllocation,
    DmaAddressWidth,
    MdioTimeout,
    PhyMissing,
    LinkDown,
}

impl From<DmaBufferError> for DwmacError {
    fn from(_error: DmaBufferError) -> Self {
        Self::DmaAllocation
    }
}

impl DwmacError {
    /// Stable, allocation-free probe label suitable for early serial logs.
    pub const fn label(self) -> &'static str {
        match self {
            Self::InvalidMmio => "invalid-mmio",
            Self::UnsupportedVersion(_) => "unsupported-version",
            Self::InvalidMacAddress => "invalid-mac-address",
            Self::ResetTimeout => "reset-timeout",
            Self::DmaAllocation => "dma-allocation",
            Self::DmaAddressWidth => "dma-address-width",
            Self::MdioTimeout => "mdio-timeout",
            Self::PhyMissing => "phy-missing",
            Self::LinkDown => "link-down",
        }
    }
}

impl<P: TxPlatform> DwmacNet<P> {
    pub const fn new(region: MmioRegion, dma_domain: &'static DmaDomain) -> Self {
        Self {
            region,
            dma_domain,
            state: SpinMutex::new(None),
            initialized: AtomicBool::new(false),
            mac: AtomicU64::new(0),
            _platform: PhantomData,
        }
    }

    pub fn init(&'static self) -> Result<(), DwmacError> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.region.virt.start.0 == 0 || self.region.virt.size < 0x1200 {
            return Err(DwmacError::InvalidMmio);
        }
        let regs = RegisterBlock::new(self.region.virt.start.0);
        let version = regs.read(MAC_VERSION);
        if version == 0 || version == u32::MAX {
            return Err(DwmacError::UnsupportedVersion(version));
        }

        let mac = read_mac(regs)
            .unwrap_or_else(|| fallback_local_mac(self.region.phys.start.0 as u64, P::read_ns()));
        let phy = discover_phy(regs)?;
        let descriptors = DmaBuffer::<P>::allocate(self.dma_domain, DESCRIPTOR_STORAGE_LEN)?;
        let rx_buffers =
            DmaBuffer::<P>::allocate(self.dma_domain, RX_RING_LEN * PACKET_BUFFER_SIZE)?;
        let tx_buffers =
            DmaBuffer::<P>::allocate(self.dma_domain, TX_RING_LEN * PACKET_BUFFER_SIZE)?;

        reset_core(regs)?;
        write_mac(regs, mac);
        configure_link(regs, phy);
        configure_mtl(regs);
        configure_rings::<P>(regs, &descriptors, &rx_buffers, &tx_buffers)?;

        self.mac.store(pack_mac(mac), Ordering::Release);
        *self.state.lock() = Some(DwmacState {
            regs,
            descriptors,
            rx_buffers,
            tx_buffers,
            rx_index: 0,
            tx_index: 0,
            rx_tail_offset: rx_initial_tail_offset(),
            rx_stall_capture_count: 0,
            pending_rx_stall: None,
            rx_irqs_masked: false,
        });
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    fn enable_irqs(&self) {
        if let Some(state) = self.state.lock().as_ref() {
            state.regs.write(
                DMA_CH0_INTR_ENABLE,
                DMA_CH0_INTR_NORMAL | DMA_CH0_INTR_ABNORMAL,
            );
        }
    }

    fn acknowledge_irq(&self) -> NetDeviceIrqOutcome {
        let mut state = self.state.lock();
        let Some(state) = state.as_mut() else {
            return NetDeviceIrqOutcome::default();
        };
        let status = state.regs.read(DMA_CH0_STATUS);
        if status & (DMA_CH0_STATUS_RI | DMA_CH0_STATUS_RPS | DMA_CH0_STATUS_RBU) != 0 {
            state.regs.modify(
                DMA_CH0_INTR_ENABLE,
                DMA_CH0_INTR_RI | DMA_CH0_INTR_RX_STALL,
                0,
            );
            state.rx_irqs_masked = true;
        }
        if status & (DMA_CH0_STATUS_RPS | DMA_CH0_STATUS_RBU) != 0 {
            // One RBU/RPS observation is enough to hand recovery to the
            // bounded receive poll. RX sources stay masked until that poll
            // has refilled descriptors and restarted the channel.
            let sequence = state.rx_stall_capture_count.saturating_add(1) as u32;
            let snapshot = capture_rx_dma_stall(state, status, sequence);
            if state.rx_stall_capture_count < 8 {
                state.pending_rx_stall = Some(snapshot);
                state.rx_stall_capture_count += 1;
            }
        }
        let acknowledged = status
            & (DMA_CH0_STATUS_NIS
                | DMA_CH0_STATUS_AIS
                | DMA_CH0_STATUS_FBE
                | DMA_CH0_STATUS_RPS
                | DMA_CH0_STATUS_RBU
                | DMA_CH0_STATUS_RI
                | DMA_CH0_STATUS_TI);
        if acknowledged == 0 {
            return NetDeviceIrqOutcome::default();
        }
        state.regs.write(DMA_CH0_STATUS, acknowledged);
        // RI owns the normal poll path. A first RBU/RPS also schedules that
        // same bounded poll after masking its source, allowing one ordered
        // tail retry without creating a competing receive owner or IRQ loop.
        let rx_ready = status & (DMA_CH0_STATUS_RI | DMA_CH0_STATUS_RBU | DMA_CH0_STATUS_RPS) != 0;
        let tx_completed = usize::from(status & DMA_CH0_STATUS_TI != 0);
        let poll_wakes = if rx_ready || tx_completed != 0 {
            tx_subsystems::net::delegate::net_delegate_kick_poll()
        } else {
            0
        };
        NetDeviceIrqOutcome {
            rx_ready,
            tx_completed,
            tx_completed_bytes: 0,
            poll_wakes,
        }
    }
}

fn capture_rx_dma_stall<P: TxPlatform>(
    state: &DwmacState<P>,
    status: u32,
    sequence: u32,
) -> RxDmaStallSnapshot {
    let descriptor_offset = rx_desc_offset(state.rx_index);
    state
        .descriptors
        .sync_for_cpu(descriptor_offset, DESC_SIZE, DmaDirection::FromDevice);
    let descriptor = read_descriptor(&state.descriptors, descriptor_offset);

    RxDmaStallSnapshot {
        status,
        rx_control: state.regs.read(DMA_CH0_RX_CONTROL),
        current_rx_desc: state.regs.read(DMA_CH0_CURRENT_RXDESC),
        current_rx_buf: state.regs.read(DMA_CH0_CURRENT_RXBUF),
        rx_tail: state.regs.read(DMA_CH0_RXDESC_TAIL),
        software_rx_index: state.rx_index as u32,
        descriptor_word3: descriptor[3],
        sequence,
    }
}

fn publish_rx_tail<P: TxPlatform>(state: &mut DwmacState<P>, descriptor_offset: usize) {
    let Ok(tail) = state.descriptors.dma_addr_at(descriptor_offset) else {
        return;
    };
    <P as DmaIf>::publish_to_device();
    state.regs.write(DMA_CH0_RXDESC_TAIL, tail as u32);
    state.rx_tail_offset = descriptor_offset;
}

fn restart_rx_dma<P: TxPlatform>(state: &mut DwmacState<P>) {
    let tail_offset = state.rx_tail_offset;
    publish_rx_tail(state, tail_offset);
    // Linux stmmac performs the same start_rx operation after every refill:
    // a tail write publishes work, while SR wakes a channel that already
    // entered the suspended state after reaching the previous boundary.
    state
        .regs
        .modify(DMA_CH0_RX_CONTROL, 0, DMA_CH0_RX_CONTROL_SR);
}

fn report_rx_dma_stall<P: TxPlatform>(snapshot: RxDmaStallSnapshot) {
    tx_hal::console_write_str::<P>("txkernel:dwmac:rx-dma-stall\n");
    console_write_hex_u32::<P>("  sequence=", snapshot.sequence);
    console_write_hex_u32::<P>("  status=", snapshot.status);
    console_write_hex_u32::<P>("  rx_control=", snapshot.rx_control);
    console_write_hex_u32::<P>("  current_rx_desc=", snapshot.current_rx_desc);
    console_write_hex_u32::<P>("  current_rx_buf=", snapshot.current_rx_buf);
    console_write_hex_u32::<P>("  rx_tail=", snapshot.rx_tail);
    console_write_hex_u32::<P>("  software_rx_index=", snapshot.software_rx_index);
    console_write_hex_u32::<P>("  descriptor_word3=", snapshot.descriptor_word3);
}

fn console_write_hex_u32<P: TxPlatform>(label: &str, value: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = *b"0x00000000\n";
    for index in 0..8 {
        let shift = (7 - index) * 4;
        encoded[index + 2] = HEX[((value >> shift) & 0xf) as usize];
    }
    tx_hal::console_write_str::<P>(label);
    tx_hal::console_write_bytes::<P>(&encoded);
}

impl<P: TxPlatform> NetDeviceOps for DwmacNet<P> {
    fn receive(&self) -> Option<RxFrame> {
        let (frame, snapshot) = {
            let mut state = self.state.lock();
            let state = state.as_mut()?;
            let frame = receive_frame(state);
            let snapshot = state.pending_rx_stall.take();
            if frame.is_none() {
                if state.rx_irqs_masked {
                    state.regs.write(
                        DMA_CH0_STATUS,
                        DMA_CH0_STATUS_RI | DMA_CH0_STATUS_RBU | DMA_CH0_STATUS_RPS,
                    );
                }
                // Re-publish the software-owned exclusive boundary and wake a
                // channel that may have suspended at the old boundary.
                restart_rx_dma(state);
                if state.rx_irqs_masked {
                    state.regs.modify(
                        DMA_CH0_INTR_ENABLE,
                        0,
                        DMA_CH0_INTR_RI | DMA_CH0_INTR_RX_STALL,
                    );
                    state.rx_irqs_masked = false;
                }
            }
            (frame, snapshot)
        };
        if let Some(snapshot) = snapshot {
            report_rx_dma_stall::<P>(snapshot);
        }
        frame
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        if frame.is_empty() || frame.len() > PACKET_BUFFER_SIZE {
            return StepOutcome::Err(Errno::EMSGSIZE);
        }
        let mut state = self.state.lock();
        let Some(state) = state.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV);
        };
        transmit_frame(state, frame)
    }

    fn tx_readiness(&self, _guard: &Guard<'_>) -> PacketTxReadiness {
        let state = self.state.lock();
        let Some(state) = state.as_ref() else {
            return PacketTxReadiness::Busy;
        };
        if descriptor_owned(&state.descriptors, tx_desc_offset(state.tx_index)) {
            PacketTxReadiness::Busy
        } else {
            PacketTxReadiness::Ready
        }
    }

    fn mac_addr(&self) -> EthernetAddress {
        unpack_mac(self.mac.load(Ordering::Acquire))
    }

    fn mtu(&self) -> u16 {
        DEFAULT_MTU
    }

    fn enable_interrupts(&self) {
        self.enable_irqs();
    }

    fn ack_interrupt_and_fire(&self) -> NetDeviceIrqOutcome {
        self.acknowledge_irq()
    }
}

fn reset_core(regs: RegisterBlock) -> Result<(), DwmacError> {
    regs.write(DMA_CH0_INTR_ENABLE, 0);
    regs.modify(DMA_CH0_TX_CONTROL, DMA_CH0_TX_CONTROL_ST, 0);
    regs.modify(DMA_CH0_RX_CONTROL, DMA_CH0_RX_CONTROL_SR, 0);
    regs.modify(DMA_MODE, 0, DMA_MODE_SWR);
    for _ in 0..RESET_POLL_LIMIT {
        if regs.read(DMA_MODE) & DMA_MODE_SWR == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(DwmacError::ResetTimeout)
}

fn configure_link(regs: RegisterBlock, link: PhyLink) {
    let speed_bits = match link.speed {
        LinkSpeed::Mbps1000 => 0,
        LinkSpeed::Mbps100 => MAC_CONFIGURATION_PS | MAC_CONFIGURATION_FES,
        LinkSpeed::Mbps10 => MAC_CONFIGURATION_PS,
    };
    let duplex = if link.full_duplex {
        MAC_CONFIGURATION_DM
    } else {
        0
    };
    regs.modify(
        MAC_CONFIGURATION,
        MAC_CONFIGURATION_PS | MAC_CONFIGURATION_FES | MAC_CONFIGURATION_DM,
        speed_bits | duplex,
    );
}

fn configure_mtl(regs: RegisterBlock) {
    let feature = regs.read(MAC_HW_FEATURE1);
    let tx_fifo = 128usize << ((feature >> 6) & 0x1f);
    let rx_fifo = 128usize << (feature & 0x1f);
    let tqs = (tx_fifo / 256).saturating_sub(1).min(0x1ff) as u32;
    let rqs = (rx_fifo / 256).saturating_sub(1).min(0x3ff) as u32;

    regs.modify(
        MTL_TXQ0_OPERATION_MODE,
        (0x1ff << 16) | (0x3 << 2),
        (tqs << 16) | (2 << 2) | (1 << 1),
    );
    regs.write(MTL_TXQ0_QUANTUM_WEIGHT, 0x10);
    regs.modify(MTL_RXQ0_OPERATION_MODE, 0x3ff << 20, (rqs << 20) | (1 << 5));
    regs.modify(MAC_RXQ_CTRL0, 0x3, 0x2);
    regs.modify(MAC_RXQ_CTRL1, 0, 1 << 20);
    regs.modify(MAC_PACKET_FILTER, 0, 1);
    regs.modify(MAC_Q0_TX_FLOW_CTRL, 0, (0xffff << 16) | (1 << 1));
    regs.modify(MAC_RX_FLOW_CTRL, 0, 1);
    regs.modify(
        MAC_CONFIGURATION,
        MAC_CONFIGURATION_GPSLCE
            | MAC_CONFIGURATION_WD
            | MAC_CONFIGURATION_JD
            | MAC_CONFIGURATION_JE,
        MAC_CONFIGURATION_CST | MAC_CONFIGURATION_ACS,
    );
}

fn configure_rings<P: TxPlatform>(
    regs: RegisterBlock,
    descriptors: &DmaBuffer<P>,
    rx_buffers: &DmaBuffer<P>,
    _tx_buffers: &DmaBuffer<P>,
) -> Result<(), DwmacError> {
    for index in 0..TX_RING_LEN {
        write_descriptor(descriptors, tx_desc_offset(index), [0; 4]);
    }
    for index in 0..RX_RING_LEN {
        let buffer = rx_buffers.dma_addr_at(index * PACKET_BUFFER_SIZE)?;
        validate_dma40(buffer)?;
        write_descriptor(descriptors, rx_desc_offset(index), rx_descriptor(buffer));
    }
    descriptors.sync_for_device(0, descriptors.len(), DmaDirection::Bidirectional);
    rx_buffers.sync_for_device(0, rx_buffers.len(), DmaDirection::FromDevice);

    // Descriptor ownership and buffer addresses must reach coherent memory
    // before MMIO exposes the ring to DMA. A volatile register write alone
    // does not provide this normal-memory-to-device ordering on RISC-V.
    <P as DmaIf>::publish_to_device();

    let tx_base = descriptors.dma_addr_at(0)?;
    let rx_base = descriptors.dma_addr_at(RX_DESC_OFFSET)?;
    validate_dma40(tx_base)?;
    validate_dma40(rx_base)?;
    regs.write(DMA_CH0_TXDESC_LIST_HI, (tx_base >> 32) as u32);
    regs.write(DMA_CH0_TXDESC_LIST_LO, tx_base as u32);
    regs.write(DMA_CH0_RXDESC_LIST_HI, (rx_base >> 32) as u32);
    regs.write(DMA_CH0_RXDESC_LIST_LO, rx_base as u32);
    regs.write(DMA_CH0_TXDESC_RING_LEN, (TX_RING_LEN - 1) as u32);
    regs.write(DMA_CH0_RXDESC_RING_LEN, (RX_RING_LEN - 1) as u32);
    regs.write(DMA_CH0_TXDESC_TAIL, tx_base as u32);
    // DWMAC4 treats the RX tail as an exclusive producer boundary. Linux
    // stmmac initially publishes `base + ring_len * descriptor_stride`, i.e.
    // one descriptor past the ring, and publishes the next refill cursor
    // thereafter. The programmed ring length performs the actual wrap.
    regs.write(
        DMA_CH0_RXDESC_TAIL,
        descriptors.dma_addr_at(rx_initial_tail_offset())? as u32,
    );

    regs.modify(
        DMA_CH0_CONTROL,
        (DMA_CH0_CONTROL_DSL_MASK << DMA_CH0_CONTROL_DSL_SHIFT) | DMA_CH0_CONTROL_PBLX8,
        (DESC_SKIP_LENGTH as u32) << DMA_CH0_CONTROL_DSL_SHIFT,
    );
    regs.modify(DMA_CH0_TX_CONTROL, 0x3f << 16, (1 << 4) | (16 << 16));
    regs.modify(
        DMA_CH0_RX_CONTROL,
        (0x3f << 16) | (0x3fff << 1),
        (8 << 16) | ((PACKET_BUFFER_SIZE as u32) << 1),
    );
    regs.write(
        DMA_SYSBUS_MODE,
        (2 << 16) | (1 << 11) | (1 << 3) | (1 << 2) | (1 << 1),
    );
    regs.modify(DMA_CH0_TX_CONTROL, 0, DMA_CH0_TX_CONTROL_ST);
    regs.modify(DMA_CH0_RX_CONTROL, 0, DMA_CH0_RX_CONTROL_SR);
    regs.modify(
        MAC_CONFIGURATION,
        0,
        MAC_CONFIGURATION_TE | MAC_CONFIGURATION_RE,
    );
    Ok(())
}

fn transmit_frame<P: TxPlatform>(
    state: &mut DwmacState<P>,
    frame: &[u8],
) -> StepOutcome<(), NoProgress> {
    let descriptor_offset = tx_desc_offset(state.tx_index);
    if descriptor_owned(&state.descriptors, descriptor_offset) {
        let token = tx_subsystems::net::delegate::net_delegate_wait_token();
        return StepOutcome::yield_on_wait_source(NoProgress, token.source_id(), token.interest());
    }
    let buffer_offset = state.tx_index * PACKET_BUFFER_SIZE;
    let Ok(buffer_ptr) = state.tx_buffers.ptr_at(buffer_offset) else {
        return StepOutcome::Err(Errno::EIO);
    };
    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer_ptr, frame.len());
    }
    state
        .tx_buffers
        .sync_for_device(buffer_offset, frame.len(), DmaDirection::ToDevice);
    let Ok(buffer_dma) = state.tx_buffers.dma_addr_at(buffer_offset) else {
        return StepOutcome::Err(Errno::EIO);
    };
    write_descriptor(
        &state.descriptors,
        descriptor_offset,
        tx_descriptor(buffer_dma, frame.len()),
    );
    state
        .descriptors
        .sync_for_device(descriptor_offset, DESC_SIZE, DmaDirection::ToDevice);
    state.tx_index = (state.tx_index + 1) % TX_RING_LEN;
    let Ok(tail) = state
        .descriptors
        .dma_addr_at(tx_desc_offset(state.tx_index))
    else {
        return StepOutcome::Err(Errno::EIO);
    };
    // Publish OWN before notifying DMA through the tail register.
    <P as DmaIf>::publish_to_device();
    state.regs.write(DMA_CH0_TXDESC_TAIL, tail as u32);
    StepOutcome::Done(())
}

fn receive_frame<P: TxPlatform>(state: &mut DwmacState<P>) -> Option<RxFrame> {
    let descriptor_offset = rx_desc_offset(state.rx_index);
    state
        .descriptors
        .sync_for_cpu(descriptor_offset, DESC_SIZE, DmaDirection::FromDevice);
    let descriptor = read_descriptor(&state.descriptors, descriptor_offset);
    if descriptor[3] & DESC3_OWN != 0 {
        return None;
    }
    let length = (descriptor[3] & DESC3_PACKET_LEN_MASK) as usize;
    let valid = descriptor[3] & (DESC3_FD | DESC3_LD) == (DESC3_FD | DESC3_LD)
        && length >= 14
        && length <= PACKET_BUFFER_SIZE;
    let buffer_offset = state.rx_index * PACKET_BUFFER_SIZE;
    let mut bytes = Vec::new();
    if valid {
        state
            .rx_buffers
            .sync_for_cpu(buffer_offset, length, DmaDirection::FromDevice);
        let ptr = state.rx_buffers.ptr_at(buffer_offset).ok()?;
        bytes.reserve_exact(length);
        unsafe {
            bytes.extend_from_slice(core::slice::from_raw_parts(ptr, length));
        }
    }

    let buffer_dma = state.rx_buffers.dma_addr_at(buffer_offset).ok()?;

    // Match StarFive's cache publication sequence before returning OWN. The
    // descriptors are cache-line separated, but the two phases also ensure
    // DMA cannot observe a recycled address while its old completion is
    // still being invalidated.
    write_descriptor_word0(&state.descriptors, descriptor_offset, 0);
    fence(Ordering::Release);
    state
        .descriptors
        .sync_for_device(descriptor_offset, DESC_SIZE, DmaDirection::ToDevice);
    state
        .rx_buffers
        .sync_for_device(buffer_offset, PACKET_BUFFER_SIZE, DmaDirection::FromDevice);
    write_descriptor(
        &state.descriptors,
        descriptor_offset,
        rx_descriptor(buffer_dma),
    );
    state
        .descriptors
        .sync_for_device(descriptor_offset, DESC_SIZE, DmaDirection::ToDevice);

    let next_rx_index = (state.rx_index + 1) % RX_RING_LEN;
    // Match Linux stmmac's refill order: publish OWN, advance the refill
    // cursor, publish that cursor as the exclusive tail, then wake RX DMA in
    // case it suspended at the previous boundary.
    publish_rx_tail(state, rx_desc_offset(next_rx_index));
    state
        .regs
        .modify(DMA_CH0_RX_CONTROL, 0, DMA_CH0_RX_CONTROL_SR);
    state.rx_index = next_rx_index;
    valid.then(|| RxFrame::new(bytes))
}

fn discover_phy(regs: RegisterBlock) -> Result<PhyLink, DwmacError> {
    let mut first = None;
    for address in 0u8..32 {
        let id1 = mdio_read(regs, address, 2)?;
        let id2 = mdio_read(regs, address, 3)?;
        if matches!((id1, id2), (0, 0) | (0xffff, 0xffff)) {
            continue;
        }
        let link = negotiated_link(regs, address)?;
        if link.is_some() {
            return Ok(link.expect("checked Some"));
        }
        first.get_or_insert(address);
    }
    if first.is_some() {
        Err(DwmacError::LinkDown)
    } else {
        Err(DwmacError::PhyMissing)
    }
}

fn negotiated_link(regs: RegisterBlock, address: u8) -> Result<Option<PhyLink>, DwmacError> {
    let _ = mdio_read(regs, address, 1)?;
    let bmsr = mdio_read(regs, address, 1)?;
    if bmsr & (1 << 2) == 0 {
        return Ok(None);
    }
    let bmcr = mdio_read(regs, address, 0)?;
    let (speed, full_duplex) = if bmcr & (1 << 12) != 0 {
        select_autoneg_link(
            mdio_read(regs, address, 4)?,
            mdio_read(regs, address, 5)?,
            mdio_read(regs, address, 9)?,
            mdio_read(regs, address, 10)?,
        )
        .ok_or(DwmacError::LinkDown)?
    } else {
        let speed = if bmcr & (1 << 6) != 0 {
            LinkSpeed::Mbps1000
        } else if bmcr & (1 << 13) != 0 {
            LinkSpeed::Mbps100
        } else {
            LinkSpeed::Mbps10
        };
        (speed, bmcr & (1 << 8) != 0)
    };
    Ok(Some(PhyLink {
        address,
        speed,
        full_duplex,
    }))
}

fn select_autoneg_link(
    advertise: u16,
    partner: u16,
    gigabit_control: u16,
    gigabit_status: u16,
) -> Option<(LinkSpeed, bool)> {
    if gigabit_control & (1 << 9) != 0 && gigabit_status & (1 << 11) != 0 {
        return Some((LinkSpeed::Mbps1000, true));
    }
    if gigabit_control & (1 << 8) != 0 && gigabit_status & (1 << 10) != 0 {
        return Some((LinkSpeed::Mbps1000, false));
    }
    let common = advertise & partner;
    for (bit, speed, full) in [
        (1 << 8, LinkSpeed::Mbps100, true),
        (1 << 7, LinkSpeed::Mbps100, false),
        (1 << 6, LinkSpeed::Mbps10, true),
        (1 << 5, LinkSpeed::Mbps10, false),
    ] {
        if common & bit != 0 {
            return Some((speed, full));
        }
    }
    None
}

fn mdio_read(regs: RegisterBlock, phy: u8, register: u8) -> Result<u16, DwmacError> {
    wait_mdio_idle(regs)?;
    // JH7110's CSR clock is 250-300 MHz, encoded as CR=5 by DWMAC5.
    let command = ((phy as u32) << 21) | ((register as u32) << 16) | (5 << 8) | (3 << 2) | 1;
    regs.write(MAC_MDIO_ADDRESS, command);
    wait_mdio_idle(regs)?;
    Ok((regs.read(MAC_MDIO_DATA) & 0xffff) as u16)
}

fn wait_mdio_idle(regs: RegisterBlock) -> Result<(), DwmacError> {
    for _ in 0..MDIO_POLL_LIMIT {
        if regs.read(MAC_MDIO_ADDRESS) & 1 == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(DwmacError::MdioTimeout)
}

fn read_mac(regs: RegisterBlock) -> Option<EthernetAddress> {
    let low = regs.read(MAC_ADDRESS0_LOW);
    let high = regs.read(MAC_ADDRESS0_HIGH);
    let mac = EthernetAddress::new([
        low as u8,
        (low >> 8) as u8,
        (low >> 16) as u8,
        (low >> 24) as u8,
        high as u8,
        (high >> 8) as u8,
    ]);
    let bytes = mac.octets();
    (!bytes.iter().all(|byte| *byte == 0)
        && !bytes.iter().all(|byte| *byte == 0xff)
        && bytes[0] & 1 == 0)
        .then_some(mac)
}

fn write_mac(regs: RegisterBlock, mac: EthernetAddress) {
    let bytes = mac.octets();
    regs.write(
        MAC_ADDRESS0_LOW,
        u32::from(bytes[0])
            | (u32::from(bytes[1]) << 8)
            | (u32::from(bytes[2]) << 16)
            | (u32::from(bytes[3]) << 24),
    );
    regs.write(
        MAC_ADDRESS0_HIGH,
        u32::from(bytes[4]) | (u32::from(bytes[5]) << 8) | (1 << 31),
    );
}

fn fallback_local_mac(device_seed: u64, time_seed: u64) -> EthernetAddress {
    let mut mixed = device_seed ^ time_seed.rotate_left(23) ^ 0x9e37_79b9_7f4a_7c15;
    mixed ^= mixed >> 30;
    mixed = mixed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed ^= mixed >> 27;
    mixed = mixed.wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^= mixed >> 31;
    let mut bytes = mixed.to_le_bytes();
    bytes[0] = (bytes[0] & 0xfe) | 0x02;
    EthernetAddress::new(bytes[..6].try_into().expect("six-byte MAC slice"))
}

fn write_descriptor<P: TxPlatform>(buffer: &DmaBuffer<P>, offset: usize, words: [u32; 4]) {
    let ptr = buffer.ptr_at(offset).expect("descriptor offset validated") as *mut u32;
    unsafe {
        core::ptr::write_volatile(ptr, words[0]);
        core::ptr::write_volatile(ptr.add(1), words[1]);
        core::ptr::write_volatile(ptr.add(2), words[2]);
        fence(Ordering::Release);
        core::ptr::write_volatile(ptr.add(3), words[3]);
    }
}

fn write_descriptor_word0<P: TxPlatform>(buffer: &DmaBuffer<P>, offset: usize, value: u32) {
    let ptr = buffer.ptr_at(offset).expect("descriptor offset validated") as *mut u32;
    unsafe {
        core::ptr::write_volatile(ptr, value);
    }
}

fn rx_descriptor(buffer_dma: u64) -> [u32; 4] {
    [
        buffer_dma as u32,
        (buffer_dma >> 32) as u32,
        0,
        DESC3_OWN | DESC3_RX_IOC | DESC3_BUF1V,
    ]
}

fn tx_descriptor(buffer_dma: u64, length: usize) -> [u32; 4] {
    [
        buffer_dma as u32,
        (buffer_dma >> 32) as u32,
        DESC2_TX_IOC | length as u32,
        DESC3_OWN | DESC3_FD | DESC3_LD | length as u32,
    ]
}

fn read_descriptor<P: TxPlatform>(buffer: &DmaBuffer<P>, offset: usize) -> [u32; 4] {
    let ptr = buffer.ptr_at(offset).expect("descriptor offset validated") as *const u32;
    let words = unsafe {
        [
            core::ptr::read_volatile(ptr),
            core::ptr::read_volatile(ptr.add(1)),
            core::ptr::read_volatile(ptr.add(2)),
            core::ptr::read_volatile(ptr.add(3)),
        ]
    };
    fence(Ordering::Acquire);
    words
}

fn descriptor_owned<P: TxPlatform>(buffer: &DmaBuffer<P>, offset: usize) -> bool {
    buffer.sync_for_cpu(offset, DESC_SIZE, DmaDirection::FromDevice);
    read_descriptor(buffer, offset)[3] & DESC3_OWN != 0
}

const fn tx_desc_offset(index: usize) -> usize {
    index * DESC_STRIDE
}

const fn rx_desc_offset(index: usize) -> usize {
    RX_DESC_OFFSET + index * DESC_STRIDE
}

const fn rx_initial_tail_offset() -> usize {
    rx_desc_offset(RX_RING_LEN)
}

fn validate_dma40(address: u64) -> Result<(), DwmacError> {
    if address >> 40 == 0 {
        Ok(())
    } else {
        Err(DwmacError::DmaAddressWidth)
    }
}

fn pack_mac(mac: EthernetAddress) -> u64 {
    mac.octets()
        .into_iter()
        .enumerate()
        .fold(0, |packed, (index, byte)| {
            packed | ((byte as u64) << (index * 8))
        })
}

fn unpack_mac(packed: u64) -> EthernetAddress {
    let mut bytes = [0u8; 6];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (packed >> (index * 8)) as u8;
    }
    EthernetAddress::new(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_regions_are_cacheline_separated_and_fit_allocated_storage() {
        assert_eq!(DESC_SKIP_LENGTH, 6);
        assert_eq!(tx_desc_offset(1) - tx_desc_offset(0), DESC_STRIDE);
        assert_eq!(rx_desc_offset(0), TX_RING_LEN * DESC_STRIDE);
        assert!(rx_desc_offset(RX_RING_LEN - 1) + DESC_SIZE <= DESCRIPTOR_STORAGE_LEN);
    }

    #[test]
    fn interrupt_driven_descriptors_request_completion_events() {
        let rx = rx_descriptor(0x0123_4567_89ab_cdef);
        assert_eq!(rx[0], 0x89ab_cdef);
        assert_eq!(rx[1], 0x0123_4567);
        assert_eq!(
            rx[3] & (DESC3_OWN | DESC3_RX_IOC | DESC3_BUF1V),
            DESC3_OWN | DESC3_RX_IOC | DESC3_BUF1V
        );

        let tx = tx_descriptor(0x0123_4567_89ab_cdef, 1500);
        assert_eq!(tx[2] & DESC2_TX_IOC, DESC2_TX_IOC);
        assert_eq!(tx[2] & DESC3_PACKET_LEN_MASK, 1500);
        assert_eq!(
            tx[3] & (DESC3_OWN | DESC3_FD | DESC3_LD),
            DESC3_OWN | DESC3_FD | DESC3_LD
        );
    }

    #[test]
    fn initial_rx_ring_tail_is_exclusive_one_past_end() {
        assert_eq!(rx_initial_tail_offset(), DESCRIPTOR_STORAGE_LEN);
        assert_eq!(rx_initial_tail_offset(), rx_desc_offset(RX_RING_LEN));
    }

    #[test]
    fn standard_clause22_autoneg_prefers_fastest_common_full_duplex_mode() {
        assert_eq!(
            select_autoneg_link(1 << 8, 1 << 8, 1 << 9, 1 << 11),
            Some((LinkSpeed::Mbps1000, true))
        );
        assert_eq!(
            select_autoneg_link(1 << 8, 1 << 8, 0, 0),
            Some((LinkSpeed::Mbps100, true))
        );
        assert_eq!(select_autoneg_link(1 << 8, 1 << 5, 0, 0), None);
    }

    #[test]
    fn mac_pack_round_trip_is_byte_stable() {
        let mac = EthernetAddress::new([0x02, 0x11, 0x22, 0x33, 0x44, 0x55]);
        assert_eq!(unpack_mac(pack_mac(mac)), mac);
    }

    #[test]
    fn fallback_mac_is_locally_administered_unicast_and_seeded() {
        let first = fallback_local_mac(0x1603_0000, 1);
        let second = fallback_local_mac(0x1604_0000, 1);
        assert_eq!(first.octets()[0] & 0x03, 0x02);
        assert_ne!(first, second);
    }
}
