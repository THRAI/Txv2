//! Synopsys DWMAC 3.x legacy network-device core.
//!
//! This backend follows the legacy DWMAC1000 register bank and normal
//! descriptor format used by Linux stmmac. It is intentionally separate from
//! the DWMAC5/EQoS backend because their DMA channel and descriptor contracts
//! are incompatible.

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

const PHY_ADDRESS: u8 = 0;
const TX_RING_LEN: usize = 16;
const RX_RING_LEN: usize = 64;
const DESCRIPTOR_SIZE: usize = 16;
// The 2K1000 U-Boot driver uses chained enhanced descriptors, each isolated in
// one LA264 cache line. Keep the hardware-visible descriptor itself at 16 bytes
// while advancing the software ring by the observed 64-byte stride.
const DESCRIPTOR_STRIDE: usize = 64;
const TX_DESCRIPTOR_BYTES: usize = TX_RING_LEN * DESCRIPTOR_STRIDE;
const DESCRIPTOR_STORAGE_BYTES: usize = (TX_RING_LEN + RX_RING_LEN) * DESCRIPTOR_STRIDE;
const PACKET_BUFFER_SIZE: usize = 2048;
const ETHERNET_MIN_FRAME_NO_FCS: usize = 60;
const RX_DESCRIPTOR_BUFFER_SIZE: usize = PACKET_BUFFER_SIZE - 1;
const ETHERNET_FCS_SIZE: usize = 4;
const DEFAULT_MTU: u16 = 1500;
const MIN_MMIO_SIZE: usize = 0x105c;
const RESET_TIMEOUT_NS: u64 = 2_000_000_000;
const RESET_FALLBACK_POLL_LIMIT: usize = 200_000_000;
const MDIO_POLL_LIMIT: usize = 100_000;

const DESC0_OWN: u32 = 1 << 31;
const RX_DESC0_FRAME_LENGTH_MASK: u32 = 0x3fff << 16;
const RX_DESC0_ERROR_SUMMARY: u32 = 1 << 15;
const RX_DESC0_LAST: u32 = 1 << 8;
const RX_DESC0_FIRST: u32 = 1 << 9;
const ENH_DESC1_BUFFER1_SIZE_MASK: u32 = 0x1fff;
const ENH_RX_DESC1_CHAIN: u32 = 1 << 14;
const ENH_TX_DESC0_CHAIN: u32 = 1 << 20;
const ENH_TX_DESC0_FIRST: u32 = 1 << 28;
const ENH_TX_DESC0_LAST: u32 = 1 << 29;
const ENH_TX_DESC0_INTERRUPT: u32 = 1 << 30;

pub struct Dwmac3Net<P: TxPlatform> {
    region: MmioRegion,
    dma_domain: &'static DmaDomain,
    configured_mac: Option<[u8; 6]>,
    state: SpinMutex<Option<Dwmac3State<P>>>,
    rx_irq: SpinMutex<RxIrqControl>,
    initialized: AtomicBool,
    mac: AtomicU64,
    _platform: PhantomData<fn() -> P>,
}

struct Dwmac3State<P: TxPlatform> {
    regs: RegisterBlock,
    descriptors: DmaBuffer<P>,
    rx_buffers: DmaBuffer<P>,
    tx_buffers: DmaBuffer<P>,
    rx_index: usize,
    tx_index: usize,
}

struct RxIrqControl {
    masked: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PhyLink {
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
pub enum Dwmac3Error {
    InvalidMmio,
    UnsupportedVersion(u32),
    ResetTimeout,
    StopTimeout,
    DmaAllocation,
    DmaAddressWidth,
    MdioTimeout,
    PhyMissing,
    LinkDown,
}

impl From<DmaBufferError> for Dwmac3Error {
    fn from(_error: DmaBufferError) -> Self {
        Self::DmaAllocation
    }
}

impl Dwmac3Error {
    pub const fn label(self) -> &'static str {
        match self {
            Self::InvalidMmio => "invalid-mmio",
            Self::UnsupportedVersion(_) => "unsupported-version",
            Self::ResetTimeout => "reset-timeout",
            Self::StopTimeout => "stop-timeout",
            Self::DmaAllocation => "dma-allocation",
            Self::DmaAddressWidth => "dma-address-width",
            Self::MdioTimeout => "mdio-timeout",
            Self::PhyMissing => "phy-missing",
            Self::LinkDown => "link-down",
        }
    }
}

impl<P: TxPlatform> Dwmac3Net<P> {
    pub const fn new(
        region: MmioRegion,
        dma_domain: &'static DmaDomain,
        configured_mac: Option<[u8; 6]>,
    ) -> Self {
        Self {
            region,
            dma_domain,
            configured_mac,
            state: SpinMutex::new(None),
            rx_irq: SpinMutex::new(RxIrqControl { masked: true }),
            initialized: AtomicBool::new(false),
            mac: AtomicU64::new(0),
            _platform: PhantomData,
        }
    }

    pub fn init(&'static self) -> Result<(), Dwmac3Error> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.region.virt.start.0 == 0 || self.region.virt.size < MIN_MMIO_SIZE {
            return Err(Dwmac3Error::InvalidMmio);
        }

        let regs = RegisterBlock::new(self.region.virt.start.0);
        let version = regs.read(GMAC_VERSION);
        if !supports_version(version) {
            return Err(Dwmac3Error::UnsupportedVersion(version));
        }

        let mac = self
            .configured_mac
            .map(EthernetAddress::new)
            .filter(|mac| valid_mac(*mac))
            .or_else(|| read_mac(regs))
            .unwrap_or_else(|| fallback_local_mac(self.region.phys.start.0 as u64, P::read_ns()));
        let phy = read_phy_link(regs, PHY_ADDRESS)?;
        let [descriptors, rx_buffers, tx_buffers] = allocate_staged(
            |len| DmaBuffer::<P>::allocate(self.dma_domain, len),
            [
                DESCRIPTOR_STORAGE_BYTES,
                RX_RING_LEN * PACKET_BUFFER_SIZE,
                TX_RING_LEN * PACKET_BUFFER_SIZE,
            ],
        )?;
        let ring_addresses = validate_ring_addresses(&descriptors, &rx_buffers, &tx_buffers)?;

        reset_dma::<P>(regs)?;
        write_mac(regs, mac);
        configure_mac(regs, phy);
        configure_rings::<P>(regs, &descriptors, &rx_buffers, ring_addresses);
        start_hardware(regs);

        self.mac.store(pack_mac(mac), Ordering::Release);
        *self.state.lock() = Some(Dwmac3State {
            regs,
            descriptors,
            rx_buffers,
            tx_buffers,
            rx_index: 0,
            tx_index: 0,
        });
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn stop(&self) -> Result<(), Dwmac3Error> {
        let mut state = self.state.lock();
        if let Some(state) = state.as_ref() {
            self.rx_irq.lock().masked = true;
            stop_hardware::<P>(state.regs)?;
        }
        *state = None;
        self.initialized.store(false, Ordering::Release);
        Ok(())
    }

    fn enable_irqs(&self) {
        if !self.initialized.load(Ordering::Acquire) {
            return;
        }
        let regs = RegisterBlock::new(self.region.virt.start.0);
        let mut rx_irq = self.rx_irq.lock();
        regs.write(DMA_STATUS, DMA_STATUS_W1C_MASK);
        let _ = regs.read(DMA_STATUS);
        regs.write(DMA_INTERRUPT_ENABLE, DMA_INTERRUPT_MASK);
        let _ = regs.read(DMA_INTERRUPT_ENABLE);
        rx_irq.masked = false;
    }

    fn acknowledge_irq(&self) -> NetDeviceIrqOutcome {
        if !self.initialized.load(Ordering::Acquire) {
            return NetDeviceIrqOutcome::default();
        }
        // Descriptor polling owns `state`. Device acknowledgement runs from
        // the deferred bottom half and serializes only the interrupt register
        // bank, so it never waits for a data-path operation to release state.
        let regs = RegisterBlock::new(self.region.virt.start.0);
        let (rx_ready, tx_completed) = {
            let mut rx_irq = self.rx_irq.lock();
            let status = regs.read(DMA_STATUS);
            let acknowledged = status & DMA_STATUS_W1C_MASK;
            if acknowledged == 0 {
                return NetDeviceIrqOutcome::default();
            }

            let rx_ready =
                status & (DMA_STATUS_RI | DMA_STATUS_RU | DMA_STATUS_RPS | DMA_STATUS_OVF) != 0;
            if rx_ready {
                regs.modify(DMA_INTERRUPT_ENABLE, DMA_INTERRUPT_RIE, 0);
                let _ = regs.read(DMA_INTERRUPT_ENABLE);
                rx_irq.masked = true;
            }
            regs.write(DMA_STATUS, acknowledged);
            let _ = regs.read(DMA_STATUS);
            (rx_ready, usize::from(status & DMA_STATUS_TI != 0))
        };
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

impl<P: TxPlatform> NetDeviceOps for Dwmac3Net<P> {
    fn receive(&self) -> Option<RxFrame> {
        let (frame, poll_again) = {
            let mut state = self.state.lock();
            let state = state.as_mut()?;
            let frame = receive_frame(state);
            let poll_again = frame.is_none() && self.finish_rx_poll(state);
            (frame, poll_again)
        };
        if poll_again {
            let _ = tx_subsystems::net::delegate::net_delegate_kick_poll();
        }
        frame
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        if frame.is_empty() || frame.len() > RX_DESCRIPTOR_BUFFER_SIZE {
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
        if descriptor_owned(&state.descriptors, tx_descriptor_offset(state.tx_index)) {
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

impl<P: TxPlatform> Dwmac3Net<P> {
    fn finish_rx_poll(&self, state: &mut Dwmac3State<P>) -> bool {
        let mut rx_irq = self.rx_irq.lock();
        if !rx_irq.masked {
            return false;
        }

        if !descriptor_owned(&state.descriptors, rx_descriptor_offset(state.rx_index)) {
            return true;
        }

        state.regs.write(DMA_RX_POLL_DEMAND, 1);
        state
            .regs
            .modify(DMA_INTERRUPT_ENABLE, 0, DMA_INTERRUPT_RIE);
        let _ = state.regs.read(DMA_INTERRUPT_ENABLE);
        rx_irq.masked = false;
        false
    }
}

fn supports_version(raw: u32) -> bool {
    raw & 0xff == 0x37
}

fn reset_dma<P: TxPlatform>(regs: RegisterBlock) -> Result<(), Dwmac3Error> {
    regs.write(DMA_INTERRUPT_ENABLE, 0);
    regs.modify(
        GMAC_CONFIGURATION,
        GMAC_CONFIGURATION_TE | GMAC_CONFIGURATION_RE,
        0,
    );
    regs.modify(
        DMA_OPERATION_MODE,
        DMA_OPERATION_MODE_ST | DMA_OPERATION_MODE_SR,
        0,
    );
    if regs.read(DMA_BUS_MODE) & DMA_BUS_MODE_SWR != 0 {
        return Err(Dwmac3Error::ResetTimeout);
    }
    regs.modify(DMA_BUS_MODE, 0, DMA_BUS_MODE_SWR);
    let start = P::read_ns();
    for _ in 0..RESET_FALLBACK_POLL_LIMIT {
        if regs.read(DMA_BUS_MODE) & DMA_BUS_MODE_SWR == 0 {
            return Ok(());
        }
        if P::read_ns().wrapping_sub(start) >= RESET_TIMEOUT_NS {
            return Err(Dwmac3Error::ResetTimeout);
        }
        core::hint::spin_loop();
    }
    Err(Dwmac3Error::ResetTimeout)
}

fn configure_mac(regs: RegisterBlock, link: PhyLink) {
    let speed = match link.speed {
        LinkSpeed::Mbps1000 => 0,
        LinkSpeed::Mbps100 => GMAC_CONFIGURATION_PS | GMAC_CONFIGURATION_FES,
        LinkSpeed::Mbps10 => GMAC_CONFIGURATION_PS,
    };
    let duplex = u32::from(link.full_duplex) * GMAC_CONFIGURATION_DM;
    regs.modify(
        GMAC_CONFIGURATION,
        GMAC_CONFIGURATION_PS | GMAC_CONFIGURATION_FES | GMAC_CONFIGURATION_DM,
        GMAC_CORE_INIT | speed | duplex,
    );
    regs.write(GMAC_FRAME_FILTER, 0);
    regs.modify(GMAC_INTERRUPT_MASK, 0, GMAC_INTERRUPT_UNSUPPORTED_MASK);
    let _ = regs.read(GMAC_INTERRUPT_MASK);
    regs.write(MMC_RX_INTERRUPT_MASK, MMC_INTERRUPT_MASK_ALL);
    regs.write(MMC_TX_INTERRUPT_MASK, MMC_INTERRUPT_MASK_ALL);
    regs.write(MMC_RX_IPC_INTERRUPT_MASK, MMC_INTERRUPT_MASK_ALL);
    let _ = regs.read(MMC_RX_INTERRUPT_MASK);
    let _ = regs.read(MMC_TX_INTERRUPT_MASK);
    let _ = regs.read(MMC_RX_IPC_INTERRUPT_MASK);
    // Loongson's GMAC integration does not implement MAC flow control.
    regs.write(GMAC_FLOW_CONTROL, 0);
}

#[derive(Clone, Copy)]
struct RingAddresses {
    tx_descriptors: [u32; TX_RING_LEN],
    rx_descriptors: [u32; RX_RING_LEN],
    tx_buffers: [u32; TX_RING_LEN],
    rx_buffers: [u32; RX_RING_LEN],
}

fn validate_ring_addresses<P: TxPlatform>(
    descriptors: &DmaBuffer<P>,
    rx_buffers: &DmaBuffer<P>,
    tx_buffers: &DmaBuffer<P>,
) -> Result<RingAddresses, Dwmac3Error> {
    validate_dma32(descriptors.dma_addr_at(descriptors.len() - 1)?)?;
    validate_dma32(rx_buffers.dma_addr_at(rx_buffers.len() - 1)?)?;
    validate_dma32(tx_buffers.dma_addr_at(tx_buffers.len() - 1)?)?;
    let mut tx_descriptor_addresses = [0u32; TX_RING_LEN];
    for (index, address) in tx_descriptor_addresses.iter_mut().enumerate() {
        *address = validate_dma32(descriptors.dma_addr_at(tx_descriptor_offset(index))?)?;
    }
    let mut rx_descriptor_addresses = [0u32; RX_RING_LEN];
    for (index, address) in rx_descriptor_addresses.iter_mut().enumerate() {
        *address = validate_dma32(descriptors.dma_addr_at(rx_descriptor_offset(index))?)?;
    }
    let mut tx_buffer_addresses = [0u32; TX_RING_LEN];
    for (index, address) in tx_buffer_addresses.iter_mut().enumerate() {
        *address = validate_dma32(tx_buffers.dma_addr_at(index * PACKET_BUFFER_SIZE)?)?;
    }
    let mut rx_buffer_addresses = [0u32; RX_RING_LEN];
    for (index, address) in rx_buffer_addresses.iter_mut().enumerate() {
        *address = validate_dma32(rx_buffers.dma_addr_at(index * PACKET_BUFFER_SIZE)?)?;
    }
    Ok(RingAddresses {
        tx_descriptors: tx_descriptor_addresses,
        rx_descriptors: rx_descriptor_addresses,
        tx_buffers: tx_buffer_addresses,
        rx_buffers: rx_buffer_addresses,
    })
}

fn configure_rings<P: TxPlatform>(
    regs: RegisterBlock,
    descriptors: &DmaBuffer<P>,
    rx_buffers: &DmaBuffer<P>,
    addresses: RingAddresses,
) {
    for index in 0..TX_RING_LEN {
        let next = (index + 1) % TX_RING_LEN;
        write_descriptor(
            descriptors,
            tx_descriptor_offset(index),
            tx_descriptor(
                addresses.tx_buffers[index],
                addresses.tx_descriptors[next],
                0,
                false,
            ),
        );
    }
    for index in 0..RX_RING_LEN {
        let next = (index + 1) % RX_RING_LEN;
        write_descriptor(
            descriptors,
            rx_descriptor_offset(index),
            rx_descriptor(addresses.rx_buffers[index], addresses.rx_descriptors[next]),
        );
    }
    descriptors.sync_for_device(0, descriptors.len(), DmaDirection::Bidirectional);
    rx_buffers.sync_for_device(0, rx_buffers.len(), DmaDirection::FromDevice);
    <P as DmaIf>::publish_to_device();

    regs.write(DMA_TX_DESCRIPTOR_BASE, addresses.tx_descriptors[0]);
    regs.write(DMA_RX_DESCRIPTOR_BASE, addresses.rx_descriptors[0]);
    regs.write(DMA_BUS_MODE, dma_bus_mode_value());
    regs.write(
        DMA_OPERATION_MODE,
        DMA_OPERATION_MODE_TSF | DMA_OPERATION_MODE_RSF | DMA_OPERATION_MODE_OSF,
    );
    regs.write(DMA_STATUS, DMA_STATUS_W1C_MASK);
}

fn start_hardware(regs: RegisterBlock) {
    regs.modify(
        GMAC_CONFIGURATION,
        0,
        GMAC_CONFIGURATION_TE | GMAC_CONFIGURATION_RE,
    );
    regs.modify(
        DMA_OPERATION_MODE,
        0,
        DMA_OPERATION_MODE_ST | DMA_OPERATION_MODE_SR,
    );
    regs.write(DMA_TX_POLL_DEMAND, 1);
    regs.write(DMA_RX_POLL_DEMAND, 1);
}

const fn dma_bus_mode_value() -> u32 {
    DMA_BUS_MODE_USP
        | DMA_BUS_MODE_PBLX8
        | (32 << DMA_BUS_MODE_PBL_SHIFT)
        | (32 << DMA_BUS_MODE_RPBL_SHIFT)
}

fn stop_hardware<P: TxPlatform>(regs: RegisterBlock) -> Result<(), Dwmac3Error> {
    regs.write(DMA_INTERRUPT_ENABLE, 0);
    regs.modify(
        DMA_OPERATION_MODE,
        DMA_OPERATION_MODE_ST | DMA_OPERATION_MODE_SR,
        0,
    );
    regs.modify(
        GMAC_CONFIGURATION,
        GMAC_CONFIGURATION_TE | GMAC_CONFIGURATION_RE,
        0,
    );
    let start = P::read_ns();
    for _ in 0..RESET_FALLBACK_POLL_LIMIT {
        let status = regs.read(DMA_STATUS);
        if status & (DMA_STATUS_TX_STATE_MASK | DMA_STATUS_RX_STATE_MASK) == 0 {
            return Ok(());
        }
        if P::read_ns().wrapping_sub(start) >= RESET_TIMEOUT_NS {
            return Err(Dwmac3Error::StopTimeout);
        }
        core::hint::spin_loop();
    }
    Err(Dwmac3Error::StopTimeout)
}

fn transmit_frame<P: TxPlatform>(
    state: &mut Dwmac3State<P>,
    frame: &[u8],
) -> StepOutcome<(), NoProgress> {
    let descriptor_offset = tx_descriptor_offset(state.tx_index);
    if descriptor_owned(&state.descriptors, descriptor_offset) {
        let token = tx_subsystems::net::delegate::net_delegate_wait_token();
        return StepOutcome::yield_on_wait_source(NoProgress, token.source_id(), token.interest());
    }

    let buffer_offset = state.tx_index * PACKET_BUFFER_SIZE;
    let Ok(buffer_ptr) = state.tx_buffers.ptr_at(buffer_offset) else {
        return StepOutcome::Err(Errno::EIO);
    };
    let dma_length = tx_dma_length(frame.len());
    unsafe {
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buffer_ptr, frame.len());
        if dma_length > frame.len() {
            core::ptr::write_bytes(buffer_ptr.add(frame.len()), 0, dma_length - frame.len());
        }
    }
    state
        .tx_buffers
        .sync_for_device(buffer_offset, dma_length, DmaDirection::ToDevice);
    let Ok(buffer_dma) = state.tx_buffers.dma_addr_at(buffer_offset) else {
        return StepOutcome::Err(Errno::EIO);
    };
    let Ok(buffer_dma) = validate_dma32(buffer_dma) else {
        return StepOutcome::Err(Errno::EIO);
    };
    let next_index = (state.tx_index + 1) % TX_RING_LEN;
    let Ok(next_descriptor_dma) = state
        .descriptors
        .dma_addr_at(tx_descriptor_offset(next_index))
    else {
        return StepOutcome::Err(Errno::EIO);
    };
    let Ok(next_descriptor_dma) = validate_dma32(next_descriptor_dma) else {
        return StepOutcome::Err(Errno::EIO);
    };

    write_descriptor(
        &state.descriptors,
        descriptor_offset,
        tx_descriptor(buffer_dma, next_descriptor_dma, dma_length, true),
    );
    state.descriptors.sync_for_device(
        descriptor_offset,
        DESCRIPTOR_SIZE,
        DmaDirection::Bidirectional,
    );
    <P as DmaIf>::publish_to_device();
    state.regs.write(DMA_TX_POLL_DEMAND, 1);
    state.tx_index = (state.tx_index + 1) % TX_RING_LEN;
    StepOutcome::Done(())
}

const fn tx_dma_length(frame_length: usize) -> usize {
    if frame_length < ETHERNET_MIN_FRAME_NO_FCS {
        ETHERNET_MIN_FRAME_NO_FCS
    } else {
        frame_length
    }
}

fn receive_frame<P: TxPlatform>(state: &mut Dwmac3State<P>) -> Option<RxFrame> {
    for _ in 0..RX_RING_LEN {
        let index = state.rx_index;
        let descriptor_offset = rx_descriptor_offset(index);
        state.descriptors.sync_for_cpu(
            descriptor_offset,
            DESCRIPTOR_SIZE,
            DmaDirection::FromDevice,
        );
        let descriptor = read_descriptor(&state.descriptors, descriptor_offset);
        if descriptor[0] & DESC0_OWN != 0 {
            return None;
        }

        let frame = if let Some(frame_length) = rx_payload_length(descriptor[0]) {
            let buffer_offset = index * PACKET_BUFFER_SIZE;
            state.rx_buffers.sync_for_cpu(
                buffer_offset,
                frame_length + ETHERNET_FCS_SIZE,
                DmaDirection::FromDevice,
            );
            let pointer = state.rx_buffers.ptr_at(buffer_offset).ok()?;
            let mut bytes = Vec::with_capacity(frame_length);
            unsafe {
                bytes.set_len(frame_length);
                core::ptr::copy_nonoverlapping(pointer, bytes.as_mut_ptr(), frame_length);
            }
            Some(RxFrame::new(bytes))
        } else {
            None
        };

        rearm_rx_descriptor(state, index, descriptor[2], descriptor[3]);
        state.rx_index = (index + 1) % RX_RING_LEN;
        if frame.is_some() {
            return frame;
        }
    }
    None
}

fn rx_payload_length(status: u32) -> Option<usize> {
    if status & (RX_DESC0_ERROR_SUMMARY | RX_DESC0_FIRST | RX_DESC0_LAST)
        != RX_DESC0_FIRST | RX_DESC0_LAST
    {
        return None;
    }
    let dma_length = ((status & RX_DESC0_FRAME_LENGTH_MASK) >> 16) as usize;
    if dma_length > RX_DESCRIPTOR_BUFFER_SIZE {
        return None;
    }
    // Linux stmmac leaves legacy ACS disabled and removes the received FCS in
    // software before publishing the Ethernet frame to the network stack.
    let payload_length = dma_length.checked_sub(ETHERNET_FCS_SIZE)?;
    (payload_length != 0).then_some(payload_length)
}

fn rearm_rx_descriptor<P: TxPlatform>(
    state: &mut Dwmac3State<P>,
    index: usize,
    buffer: u32,
    next_descriptor: u32,
) {
    let buffer_offset = index * PACKET_BUFFER_SIZE;
    state
        .rx_buffers
        .sync_for_device(buffer_offset, PACKET_BUFFER_SIZE, DmaDirection::FromDevice);
    let descriptor_offset = rx_descriptor_offset(index);
    write_descriptor(
        &state.descriptors,
        descriptor_offset,
        rx_descriptor(buffer, next_descriptor),
    );
    state.descriptors.sync_for_device(
        descriptor_offset,
        DESCRIPTOR_SIZE,
        DmaDirection::Bidirectional,
    );
    <P as DmaIf>::publish_to_device();
    state.regs.write(DMA_RX_POLL_DEMAND, 1);
}

fn read_phy_link(regs: RegisterBlock, address: u8) -> Result<PhyLink, Dwmac3Error> {
    let id1 = mdio_read(regs, address, 2)?;
    let id2 = mdio_read(regs, address, 3)?;
    if matches!((id1, id2), (0, 0) | (0xffff, 0xffff)) {
        return Err(Dwmac3Error::PhyMissing);
    }
    let _ = mdio_read(regs, address, 1)?;
    let status = mdio_read(regs, address, 1)?;
    let control = mdio_read(regs, address, 0)?;
    let advertise = mdio_read(regs, address, 4)?;
    let partner = mdio_read(regs, address, 5)?;
    let gigabit_control = mdio_read(regs, address, 9)?;
    let gigabit_status = mdio_read(regs, address, 10)?;
    parse_phy_link(
        control,
        status,
        advertise,
        partner,
        gigabit_control,
        gigabit_status,
    )
    .ok_or(Dwmac3Error::LinkDown)
}

fn parse_phy_link(
    control: u16,
    status: u16,
    advertise: u16,
    partner: u16,
    gigabit_control: u16,
    gigabit_status: u16,
) -> Option<PhyLink> {
    if status & (1 << 2) == 0 {
        return None;
    }
    if control & (1 << 12) == 0 {
        let speed = if control & (1 << 6) != 0 {
            LinkSpeed::Mbps1000
        } else if control & (1 << 13) != 0 {
            LinkSpeed::Mbps100
        } else {
            LinkSpeed::Mbps10
        };
        return Some(PhyLink {
            speed,
            full_duplex: control & (1 << 8) != 0,
        });
    }

    let (speed, full_duplex) =
        select_autoneg_link(advertise, partner, gigabit_control, gigabit_status)?;
    Some(PhyLink { speed, full_duplex })
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
    for (bit, speed, full_duplex) in [
        (1 << 8, LinkSpeed::Mbps100, true),
        (1 << 7, LinkSpeed::Mbps100, false),
        (1 << 6, LinkSpeed::Mbps10, true),
        (1 << 5, LinkSpeed::Mbps10, false),
    ] {
        if common & bit != 0 {
            return Some((speed, full_duplex));
        }
    }
    None
}

fn mdio_read(regs: RegisterBlock, phy: u8, register: u8) -> Result<u16, Dwmac3Error> {
    wait_mdio_idle(regs)?;
    regs.write(
        GMAC_MII_ADDRESS,
        encode_mii_address(phy, register, MII_CLOCK_100_150_MHZ, false),
    );
    wait_mdio_idle(regs)?;
    Ok((regs.read(GMAC_MII_DATA) & 0xffff) as u16)
}

fn encode_mii_address(phy: u8, register: u8, clock: u32, write: bool) -> u32 {
    ((u32::from(phy) & 0x1f) << MII_ADDRESS_PHY_SHIFT)
        | ((u32::from(register) & 0x1f) << MII_ADDRESS_REGISTER_SHIFT)
        | ((clock & 0xf) << MII_ADDRESS_CLOCK_SHIFT)
        | if write { MII_ADDRESS_WRITE } else { 0 }
        | MII_ADDRESS_BUSY
}

fn wait_mdio_idle(regs: RegisterBlock) -> Result<(), Dwmac3Error> {
    for _ in 0..MDIO_POLL_LIMIT {
        if regs.read(GMAC_MII_ADDRESS) & MII_ADDRESS_BUSY == 0 {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(Dwmac3Error::MdioTimeout)
}

fn read_mac(regs: RegisterBlock) -> Option<EthernetAddress> {
    let high = regs.read(GMAC_ADDRESS0_HIGH);
    let low = regs.read(GMAC_ADDRESS0_LOW);
    let mac = EthernetAddress::new([
        low as u8,
        (low >> 8) as u8,
        (low >> 16) as u8,
        (low >> 24) as u8,
        high as u8,
        (high >> 8) as u8,
    ]);
    valid_mac(mac).then_some(mac)
}

fn write_mac(regs: RegisterBlock, mac: EthernetAddress) {
    let bytes = mac.octets();
    // DWMAC 3.x latches the perfect-filter address when the low word is
    // written. Program the high word first, matching both stmmac and U-Boot.
    regs.write(
        GMAC_ADDRESS0_HIGH,
        u32::from(bytes[4]) | (u32::from(bytes[5]) << 8),
    );
    regs.write(
        GMAC_ADDRESS0_LOW,
        u32::from(bytes[0])
            | (u32::from(bytes[1]) << 8)
            | (u32::from(bytes[2]) << 16)
            | (u32::from(bytes[3]) << 24),
    );
}

fn valid_mac(mac: EthernetAddress) -> bool {
    let bytes = mac.octets();
    !bytes.iter().all(|byte| *byte == 0)
        && !bytes.iter().all(|byte| *byte == 0xff)
        && bytes[0] & 1 == 0
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

fn rx_descriptor(buffer_dma: u32, next_descriptor_dma: u32) -> [u32; 4] {
    [
        DESC0_OWN,
        (RX_DESCRIPTOR_BUFFER_SIZE as u32 & ENH_DESC1_BUFFER1_SIZE_MASK) | ENH_RX_DESC1_CHAIN,
        buffer_dma,
        next_descriptor_dma,
    ]
}

fn tx_descriptor(
    buffer_dma: u32,
    next_descriptor_dma: u32,
    length: usize,
    owned: bool,
) -> [u32; 4] {
    let packet_control = if length == 0 {
        0
    } else {
        ENH_TX_DESC0_FIRST | ENH_TX_DESC0_LAST | ENH_TX_DESC0_INTERRUPT
    };
    [
        ENH_TX_DESC0_CHAIN | packet_control | if owned { DESC0_OWN } else { 0 },
        length as u32 & ENH_DESC1_BUFFER1_SIZE_MASK,
        buffer_dma,
        next_descriptor_dma,
    ]
}

fn write_descriptor<P: TxPlatform>(buffer: &DmaBuffer<P>, offset: usize, words: [u32; 4]) {
    let pointer = buffer.ptr_at(offset).expect("descriptor offset validated") as *mut u32;
    unsafe {
        core::ptr::write_volatile(pointer.add(1), words[1]);
        core::ptr::write_volatile(pointer.add(2), words[2]);
        core::ptr::write_volatile(pointer.add(3), words[3]);
        fence(Ordering::Release);
        core::ptr::write_volatile(pointer, words[0]);
    }
}

fn read_descriptor<P: TxPlatform>(buffer: &DmaBuffer<P>, offset: usize) -> [u32; 4] {
    let pointer = buffer.ptr_at(offset).expect("descriptor offset validated") as *const u32;
    let word0 = unsafe { core::ptr::read_volatile(pointer) };
    fence(Ordering::Acquire);
    unsafe {
        [
            word0,
            core::ptr::read_volatile(pointer.add(1)),
            core::ptr::read_volatile(pointer.add(2)),
            core::ptr::read_volatile(pointer.add(3)),
        ]
    }
}

fn descriptor_owned<P: TxPlatform>(buffer: &DmaBuffer<P>, offset: usize) -> bool {
    buffer.sync_for_cpu(offset, DESCRIPTOR_SIZE, DmaDirection::FromDevice);
    read_descriptor(buffer, offset)[0] & DESC0_OWN != 0
}

const fn tx_descriptor_offset(index: usize) -> usize {
    index * DESCRIPTOR_STRIDE
}

const fn rx_descriptor_offset(index: usize) -> usize {
    TX_DESCRIPTOR_BYTES + index * DESCRIPTOR_STRIDE
}

fn validate_dma32(address: u64) -> Result<u32, Dwmac3Error> {
    u32::try_from(address).map_err(|_| Dwmac3Error::DmaAddressWidth)
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

fn allocate_staged<T, E>(
    mut allocate: impl FnMut(usize) -> Result<T, E>,
    lengths: [usize; 3],
) -> Result<[T; 3], E> {
    Ok([
        allocate(lengths[0])?,
        allocate(lengths[1])?,
        allocate(lengths[2])?,
    ])
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn accepts_only_the_dwmac_370_version_family_id() {
        assert!(supports_version(0x0012_0037));
        assert!(!supports_version(0));
        assert!(!supports_version(0xffff_ffff));
        assert!(!supports_version(0x35));
        assert!(!supports_version(0x40));
    }

    #[test]
    fn legacy_mii_address_uses_clause22_fields() {
        assert_eq!(encode_mii_address(0, 2, 0, false), (2 << 6) | 1);
        assert_eq!(
            encode_mii_address(3, 7, 5, true),
            (3 << 11) | (7 << 6) | (5 << 2) | (1 << 1) | 1
        );
    }

    #[test]
    fn loongson_dma_bus_mode_uses_separate_32_beat_pbl_fields() {
        let mode = dma_bus_mode_value();
        assert_eq!((mode >> DMA_BUS_MODE_PBL_SHIFT) & 0x3f, 32);
        assert_eq!((mode >> DMA_BUS_MODE_RPBL_SHIFT) & 0x3f, 32);
        assert_ne!(mode & DMA_BUS_MODE_USP, 0);
        assert_ne!(mode & DMA_BUS_MODE_PBLX8, 0);
    }

    #[test]
    fn interrupt_mask_only_arms_causes_with_recovery_paths() {
        let normal = DMA_INTERRUPT_NIE | DMA_INTERRUPT_RIE | DMA_INTERRUPT_TIE;
        let abnormal = DMA_INTERRUPT_AIE
            | DMA_INTERRUPT_FBE
            | DMA_INTERRUPT_RSE
            | DMA_INTERRUPT_RUE
            | DMA_INTERRUPT_TUE
            | DMA_INTERRUPT_TSE;

        assert_eq!(DMA_INTERRUPT_MASK, normal);
        assert_eq!(DMA_INTERRUPT_MASK & abnormal, 0);
        assert_eq!(DMA_STATUS_W1C_MASK, 0x0001_ffff);
        assert_eq!(DMA_STATUS_W1C_MASK & DMA_STATUS_NIS, DMA_STATUS_NIS);
        assert_eq!(DMA_STATUS_W1C_MASK & DMA_STATUS_AIS, DMA_STATUS_AIS);
        assert_eq!(DMA_STATUS_W1C_MASK & DMA_STATUS_FBI, DMA_STATUS_FBI);
        assert_eq!(DMA_STATUS_W1C_MASK & DMA_STATUS_TU, DMA_STATUS_TU);
        assert_eq!(DMA_STATUS_W1C_MASK & DMA_STATUS_TPS, DMA_STATUS_TPS);
        assert_eq!(DMA_STATUS_W1C_MASK & (1 << 14), 1 << 14);
        assert_eq!(DMA_STATUS_W1C_MASK & (1 << 10), 1 << 10);
        assert_eq!(DMA_STATUS_W1C_MASK & (1 << 9), 1 << 9);
        assert_eq!(DMA_STATUS_W1C_MASK & (1 << 5), 1 << 5);
        assert_eq!(DMA_STATUS_W1C_MASK & (1 << 3), 1 << 3);
    }

    #[test]
    fn mac_setup_masks_all_unsupported_optional_interrupts() {
        let mut mmio = vec![0u32; MIN_MMIO_SIZE / core::mem::size_of::<u32>()];
        let regs = RegisterBlock::new(mmio.as_mut_ptr() as usize);
        let inherited_mac_mask = 1 << 12;
        regs.write(GMAC_INTERRUPT_MASK, inherited_mac_mask);

        configure_mac(
            regs,
            PhyLink {
                speed: LinkSpeed::Mbps1000,
                full_duplex: true,
            },
        );

        assert_eq!(
            regs.read(GMAC_INTERRUPT_MASK),
            inherited_mac_mask | GMAC_INTERRUPT_UNSUPPORTED_MASK
        );
        assert_eq!(regs.read(MMC_RX_INTERRUPT_MASK), MMC_INTERRUPT_MASK_ALL);
        assert_eq!(regs.read(MMC_TX_INTERRUPT_MASK), MMC_INTERRUPT_MASK_ALL);
        assert_eq!(regs.read(MMC_RX_IPC_INTERRUPT_MASK), MMC_INTERRUPT_MASK_ALL);
    }

    #[test]
    fn enhanced_chain_descriptors_match_the_2k1000_uboot_layout() {
        let rx = rx_descriptor(0x1234_5000, 0x1234_1040);
        assert_eq!(rx[0], DESC0_OWN);
        assert_eq!(rx[1] & ENH_DESC1_BUFFER1_SIZE_MASK, 2047);
        assert_eq!(rx[1] & ENH_RX_DESC1_CHAIN, ENH_RX_DESC1_CHAIN);
        assert_eq!(rx[2], 0x1234_5000);
        assert_eq!(rx[3], 0x1234_1040);

        let tx = tx_descriptor(0x2345_6000, 0x2345_1040, 60, true);
        assert_eq!(tx[0] & DESC0_OWN, DESC0_OWN);
        assert_eq!(
            tx[0]
                & (ENH_TX_DESC0_CHAIN
                    | ENH_TX_DESC0_FIRST
                    | ENH_TX_DESC0_LAST
                    | ENH_TX_DESC0_INTERRUPT),
            ENH_TX_DESC0_CHAIN | ENH_TX_DESC0_FIRST | ENH_TX_DESC0_LAST | ENH_TX_DESC0_INTERRUPT
        );
        assert_eq!(tx[1] & ENH_DESC1_BUFFER1_SIZE_MASK, 60);
        assert_eq!(tx[2], 0x2345_6000);
        assert_eq!(tx[3], 0x2345_1040);
        assert_eq!(tx_descriptor_offset(1), 64);
        assert_eq!(rx_descriptor_offset(1) - rx_descriptor_offset(0), 64);
    }

    #[test]
    fn transmit_dma_length_pads_runt_frames_without_changing_normal_frames() {
        assert_eq!(tx_dma_length(42), 60);
        assert_eq!(tx_dma_length(60), 60);
        assert_eq!(tx_dma_length(1514), 1514);
    }

    #[test]
    fn address0_high_matches_the_dwmac_370_register_layout() {
        let bytes = EthernetAddress::new([0x0e, 0x40, 0xdf, 0xbc, 0x58, 0x4e]).octets();
        let high = u32::from(bytes[4]) | (u32::from(bytes[5]) << 8);
        assert_eq!(high, 0x0000_4e58);
    }

    #[test]
    fn core_configuration_keeps_receive_active_while_transmitting() {
        assert_ne!(GMAC_CORE_INIT & GMAC_CONFIGURATION_DO, 0);
    }

    #[test]
    fn receive_status_requires_one_complete_error_free_frame_and_strips_fcs() {
        let complete = RX_DESC0_FIRST | RX_DESC0_LAST | ((1518u32) << 16);
        assert_eq!(rx_payload_length(complete), Some(1514));
        assert_eq!(rx_payload_length(complete | RX_DESC0_ERROR_SUMMARY), None);
        assert_eq!(rx_payload_length(complete & !RX_DESC0_LAST), None);
    }

    #[test]
    fn phy_link_parser_prefers_gigabit_full_then_standard_common_modes() {
        assert_eq!(
            parse_phy_link(1 << 12, 1 << 2, 1 << 8, 1 << 8, 1 << 9, 1 << 11),
            Some(PhyLink {
                speed: LinkSpeed::Mbps1000,
                full_duplex: true,
            })
        );
        assert_eq!(
            parse_phy_link(1 << 12, 1 << 2, 1 << 8, 1 << 8, 0, 0),
            Some(PhyLink {
                speed: LinkSpeed::Mbps100,
                full_duplex: true,
            })
        );
        assert_eq!(parse_phy_link(1 << 12, 0, 0, 0, 0, 0), None);
    }

    #[test]
    fn fixed_phy_mode_decodes_speed_and_duplex_control_bits() {
        assert_eq!(
            parse_phy_link((1 << 6) | (1 << 8), 1 << 2, 0, 0, 0, 0),
            Some(PhyLink {
                speed: LinkSpeed::Mbps1000,
                full_duplex: true,
            })
        );
        assert_eq!(
            parse_phy_link(1 << 13, 1 << 2, 0, 0, 0, 0),
            Some(PhyLink {
                speed: LinkSpeed::Mbps100,
                full_duplex: false,
            })
        );
    }

    #[test]
    fn staged_allocation_drops_completed_resources_when_a_later_stage_fails() {
        struct DropProbe<'a>(&'a AtomicUsize);
        impl Drop for DropProbe<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = AtomicUsize::new(0);
        let mut calls = 0;
        let result = allocate_staged(
            |_| {
                calls += 1;
                if calls == 2 {
                    Err(())
                } else {
                    Ok(DropProbe(&drops))
                }
            },
            [1, 2, 3],
        );
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}
