use alloc::vec::Vec;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};

use tx_hal::{PlatformInfoIf, TxPlatform};
use tx_substrate::step::{NoProgress, StepOutcome};
use tx_substrate::SpinMutex;
use tx_subsystems::execution::{Errno, Guard};
use tx_subsystems::net::delegate::net_delegate_wait_token;
use tx_subsystems::net::device::{
    EthernetAddress, NetDeviceIrqOutcome, NetDeviceOps, VirtioNetStats,
};
use tx_subsystems::net::packet::{PacketTxReadiness, RxFrame};
use virtio_drivers::device::net::{VirtIONetRaw, VirtioNetHdr};
use virtio_drivers::transport::mmio::{MmioError, MmioTransport, VirtIOHeader};
use virtio_drivers::transport::{DeviceType, Transport};
use virtio_drivers::Error as VirtioError;

use super::{pci, TxVirtioHal};

const DEFAULT_MTU: u16 = 1500;
const RX_BUFFER_LEN: usize = 2048;
const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976;
const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x08;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VirtioNetError {
    MissingMmioRegion(&'static str),
    WrongDeviceType(DeviceType),
    Mmio(MmioError),
    Pci(pci::VirtioPciError),
    Transport,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VirtioNetPollOutcome {
    pub claimed: bool,
    pub rx_ready: bool,
    pub tx_completed: usize,
    pub poll_wakes: usize,
}

pub struct VirtioMmioNet<P: TxPlatform, const QUEUE_SIZE: usize> {
    region_source: crate::virtio::mmio::RegionSource,
    inner: SpinMutex<Option<VirtioNetRawState<P, MmioTransport<'static>, QUEUE_SIZE>>>,
    initialized: AtomicBool,
    mac: AtomicU64,
    mtu: AtomicU16,
    stats: VirtioNetStats,
    _platform: PhantomData<fn() -> P>,
}

pub struct VirtioPciNet<P: TxPlatform, const QUEUE_SIZE: usize> {
    ecam_region_name: &'static str,
    mmio32_region_name: &'static str,
    inner: SpinMutex<
        Option<VirtioNetRawState<P, virtio_drivers::transport::pci::PciTransport, QUEUE_SIZE>>,
    >,
    initialized: AtomicBool,
    mac: AtomicU64,
    mtu: AtomicU16,
    stats: VirtioNetStats,
    _platform: PhantomData<fn() -> P>,
}

struct VirtioNetRawState<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize> {
    raw: VirtIONetRaw<TxVirtioHal<P>, T, QUEUE_SIZE>,
    rx_pending: Vec<PendingRxBuffer>,
    tx_pending: Vec<PendingTxBuffer>,
}

struct PendingRxBuffer {
    token: u16,
    bytes: Vec<u8>,
}

struct PendingTxBuffer {
    token: u16,
    bytes: Vec<u8>,
    frame_len: usize,
}

impl<P: TxPlatform, const QUEUE_SIZE: usize> VirtioMmioNet<P, QUEUE_SIZE> {
    pub const fn new(mmio_region_name: &'static str) -> Self {
        Self::with_source(crate::virtio::mmio::RegionSource::Name(mmio_region_name))
    }

    pub const fn from_region(region: tx_hal::MmioRegion) -> Self {
        Self::with_source(crate::virtio::mmio::RegionSource::Region(region))
    }

    const fn with_source(region_source: crate::virtio::mmio::RegionSource) -> Self {
        Self {
            region_source,
            inner: SpinMutex::new(None),
            initialized: AtomicBool::new(false),
            mac: AtomicU64::new(0),
            mtu: AtomicU16::new(DEFAULT_MTU),
            stats: VirtioNetStats::new(),
            _platform: PhantomData,
        }
    }

    pub fn init(&'static self) -> Result<(), VirtioNetError> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let region = match self.region_source {
            crate::virtio::mmio::RegionSource::Name(name) => <P as PlatformInfoIf>::platform_info()
                .mmio_regions
                .iter()
                .copied()
                .find(|region| region.name == name)
                .ok_or(VirtioNetError::MissingMmioRegion(name))?,
            crate::virtio::mmio::RegionSource::Region(region) => region,
        };
        // Do not construct MmioTransport for a non-net device: dropping it
        // resets the underlying virtio device, including the boot block disk.
        let device_type = peek_mmio_device_type(region)?;
        if device_type != DeviceType::Network {
            return Err(VirtioNetError::WrongDeviceType(device_type));
        }
        let header = NonNull::new(region.virt.start.0 as *mut VirtIOHeader)
            .ok_or(VirtioNetError::MissingMmioRegion(region.name))?;
        let transport = unsafe { MmioTransport::new(header, region.virt.size) }
            .map_err(VirtioNetError::Mmio)?;
        init_raw_device(self, transport)
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> &VirtioNetStats {
        &self.stats
    }

    pub fn poll_device_and_fire(&self) -> VirtioNetPollOutcome {
        poll_device_and_fire(&self.inner, &self.stats, &self.initialized, QUEUE_SIZE)
    }

    pub fn ack_interrupt_and_fire(&self) -> VirtioNetPollOutcome {
        let mut inner = self.inner.lock();
        let Some(state) = inner.as_mut() else {
            return VirtioNetPollOutcome::default();
        };
        let claimed = state.raw.ack_interrupt();
        drop(inner);
        let mut outcome =
            poll_device_and_fire(&self.inner, &self.stats, &self.initialized, QUEUE_SIZE);
        outcome.claimed = claimed;
        outcome
    }

    pub fn enable_interrupts(&self) {
        let mut inner = self.inner.lock();
        if let Some(state) = inner.as_mut() {
            state.raw.enable_interrupts();
        }
    }
}

impl<P: TxPlatform, const QUEUE_SIZE: usize> VirtioPciNet<P, QUEUE_SIZE> {
    pub const fn new(ecam_region_name: &'static str, mmio32_region_name: &'static str) -> Self {
        Self {
            ecam_region_name,
            mmio32_region_name,
            inner: SpinMutex::new(None),
            initialized: AtomicBool::new(false),
            mac: AtomicU64::new(0),
            mtu: AtomicU16::new(DEFAULT_MTU),
            stats: VirtioNetStats::new(),
            _platform: PhantomData,
        }
    }

    pub fn init(&'static self) -> Result<(), VirtioNetError> {
        if self.initialized.load(Ordering::Acquire) {
            return Ok(());
        }

        let ecam = pci::mmio_region::<P>(self.ecam_region_name).map_err(VirtioNetError::Pci)?;
        let mmio32 = pci::mmio_region::<P>(self.mmio32_region_name).map_err(VirtioNetError::Pci)?;
        let transport =
            pci::find_virtio_net_transport::<P>(ecam, mmio32).map_err(VirtioNetError::Pci)?;
        init_raw_device(self, transport)
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub fn stats(&self) -> &VirtioNetStats {
        &self.stats
    }

    pub fn poll_device_and_fire(&self) -> VirtioNetPollOutcome {
        poll_device_and_fire(&self.inner, &self.stats, &self.initialized, QUEUE_SIZE)
    }

    pub fn ack_interrupt_and_fire(&self) -> VirtioNetPollOutcome {
        let mut inner = self.inner.lock();
        let Some(state) = inner.as_mut() else {
            return VirtioNetPollOutcome::default();
        };
        let claimed = state.raw.ack_interrupt();
        drop(inner);
        let mut outcome =
            poll_device_and_fire(&self.inner, &self.stats, &self.initialized, QUEUE_SIZE);
        outcome.claimed = claimed;
        outcome
    }

    pub fn enable_interrupts(&self) {
        let mut inner = self.inner.lock();
        if let Some(state) = inner.as_mut() {
            state.raw.enable_interrupts();
        }
    }
}

trait RawDeviceSlot<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize> {
    fn inner(&self) -> &SpinMutex<Option<VirtioNetRawState<P, T, QUEUE_SIZE>>>;
    fn initialized(&self) -> &AtomicBool;
    fn mac(&self) -> &AtomicU64;
    fn mtu(&self) -> &AtomicU16;
}

impl<P: TxPlatform, const QUEUE_SIZE: usize> RawDeviceSlot<P, MmioTransport<'static>, QUEUE_SIZE>
    for VirtioMmioNet<P, QUEUE_SIZE>
{
    fn inner(
        &self,
    ) -> &SpinMutex<Option<VirtioNetRawState<P, MmioTransport<'static>, QUEUE_SIZE>>> {
        &self.inner
    }

    fn initialized(&self) -> &AtomicBool {
        &self.initialized
    }

    fn mac(&self) -> &AtomicU64 {
        &self.mac
    }

    fn mtu(&self) -> &AtomicU16 {
        &self.mtu
    }
}

impl<P: TxPlatform, const QUEUE_SIZE: usize>
    RawDeviceSlot<P, virtio_drivers::transport::pci::PciTransport, QUEUE_SIZE>
    for VirtioPciNet<P, QUEUE_SIZE>
{
    fn inner(
        &self,
    ) -> &SpinMutex<
        Option<VirtioNetRawState<P, virtio_drivers::transport::pci::PciTransport, QUEUE_SIZE>>,
    > {
        &self.inner
    }

    fn initialized(&self) -> &AtomicBool {
        &self.initialized
    }

    fn mac(&self) -> &AtomicU64 {
        &self.mac
    }

    fn mtu(&self) -> &AtomicU16 {
        &self.mtu
    }
}

fn peek_mmio_device_type(region: tx_hal::MmioRegion) -> Result<DeviceType, VirtioNetError> {
    let base = region.virt.start.0 as *const u8;
    let magic = unsafe { core::ptr::read_volatile(base.cast::<u32>()) };
    if magic != VIRTIO_MMIO_MAGIC {
        return Err(VirtioNetError::Mmio(MmioError::BadMagic(magic)));
    }

    let device_id =
        unsafe { core::ptr::read_volatile(base.add(VIRTIO_MMIO_DEVICE_ID_OFFSET).cast::<u32>()) };
    DeviceType::try_from(device_id)
        .map_err(|err| VirtioNetError::Mmio(MmioError::InvalidDeviceID(err)))
}

fn init_raw_device<P, T, S, const QUEUE_SIZE: usize>(
    slot: &'static S,
    transport: T,
) -> Result<(), VirtioNetError>
where
    P: TxPlatform,
    T: Transport + 'static,
    S: RawDeviceSlot<P, T, QUEUE_SIZE> + 'static,
{
    if transport.device_type() != DeviceType::Network {
        return Err(VirtioNetError::WrongDeviceType(transport.device_type()));
    }
    let mut raw = VirtIONetRaw::<TxVirtioHal<P>, T, QUEUE_SIZE>::new(transport)
        .map_err(|_| VirtioNetError::Transport)?;
    raw.disable_interrupts();
    let mac = EthernetAddress::new(raw.mac_address());
    slot.mac().store(pack_mac(mac), Ordering::Release);
    slot.mtu().store(DEFAULT_MTU, Ordering::Release);

    let mut state = VirtioNetRawState {
        raw,
        rx_pending: Vec::new(),
        tx_pending: Vec::new(),
    };
    prime_rx_buffers(&mut state, QUEUE_SIZE);
    *slot.inner().lock() = Some(state);
    slot.initialized().store(true, Ordering::Release);
    Ok(())
}

fn poll_device_and_fire<P, T, const QUEUE_SIZE: usize>(
    inner: &SpinMutex<Option<VirtioNetRawState<P, T, QUEUE_SIZE>>>,
    stats: &VirtioNetStats,
    initialized: &AtomicBool,
    _rx_budget: usize,
) -> VirtioNetPollOutcome
where
    P: TxPlatform,
    T: Transport,
{
    if !initialized.load(Ordering::Acquire) {
        return VirtioNetPollOutcome::default();
    }

    let mut inner = inner.lock();
    let Some(state) = inner.as_mut() else {
        return VirtioNetPollOutcome::default();
    };
    let tx_completed = complete_tx(state, stats, usize::MAX);
    let rx_ready = state.raw.poll_receive().is_some();
    // Do NOT suppress device interrupts when work is pending. An earlier
    // NAPI-style `disable_interrupts()` here had no matching re-enable
    // anywhere, so the FIRST net IRQ silenced the device forever (later
    // frames sat unnoticed until an unrelated delegate poll — root cause
    // of the P2 "server response never ACKed / read never wakes" stall).
    // IRQ-rate throttling is already provided one level up by the outstanding
    // controller claim: the PLIC gateway does not forward this source again
    // until the task-context bottom half ACKs/polls the device and completes
    // that claim. Device-level suppression is therefore unnecessary.

    let poll_wakes = if rx_ready || tx_completed != 0 {
        stats.irq_polls.fetch_add(1, Ordering::Relaxed);
        tx_subsystems::net::delegate::net_delegate_kick_poll()
    } else {
        0
    };

    VirtioNetPollOutcome {
        claimed: false,
        rx_ready,
        tx_completed,
        poll_wakes,
    }
}

fn prime_rx_buffers<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize>(
    state: &mut VirtioNetRawState<P, T, QUEUE_SIZE>,
    budget: usize,
) {
    for _ in 0..budget {
        if !queue_rx_buffer(state, alloc::vec![0; RX_BUFFER_LEN]) {
            break;
        }
    }
}

fn queue_rx_buffer<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize>(
    state: &mut VirtioNetRawState<P, T, QUEUE_SIZE>,
    mut bytes: Vec<u8>,
) -> bool {
    match unsafe { state.raw.receive_begin(&mut bytes) } {
        Ok(token) => {
            state.rx_pending.push(PendingRxBuffer { token, bytes });
            true
        }
        Err(_) => false,
    }
}

fn complete_tx<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize>(
    state: &mut VirtioNetRawState<P, T, QUEUE_SIZE>,
    stats: &VirtioNetStats,
    budget: usize,
) -> usize {
    let mut completed = 0usize;
    for _ in 0..budget {
        let Some(token) = state.raw.poll_transmit() else {
            break;
        };
        let Some(pos) = state
            .tx_pending
            .iter()
            .position(|pending| pending.token == token)
        else {
            stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            break;
        };
        let pending = state.tx_pending.swap_remove(pos);
        match unsafe { state.raw.transmit_complete(token, &pending.bytes) } {
            Ok(_) => {
                completed += 1;
                stats.tx_completed.fetch_add(1, Ordering::Relaxed);
                stats
                    .tx_completed_bytes
                    .fetch_add(pending.frame_len as u64, Ordering::Relaxed);
            }
            Err(_) => {
                stats.tx_errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }
    completed
}

fn receive_one<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize>(
    state: &mut VirtioNetRawState<P, T, QUEUE_SIZE>,
    stats: &VirtioNetStats,
) -> Option<RxFrame> {
    let token = state.raw.poll_receive()?;
    let pos = state
        .rx_pending
        .iter()
        .position(|pending| pending.token == token)?;
    let mut pending = state.rx_pending.swap_remove(pos);
    let frame = match unsafe { state.raw.receive_complete(token, &mut pending.bytes) } {
        Ok((header_len, packet_len)) => {
            let end = header_len.checked_add(packet_len)?;
            if end > pending.bytes.len() {
                stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
                None
            } else {
                let mut bytes = Vec::with_capacity(packet_len);
                bytes.extend_from_slice(&pending.bytes[header_len..end]);
                stats.rx_packets.fetch_add(1, Ordering::Relaxed);
                stats
                    .rx_bytes
                    .fetch_add(packet_len as u64, Ordering::Relaxed);
                Some(RxFrame::new(bytes))
            }
        }
        Err(_) => {
            stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            None
        }
    };
    queue_rx_buffer(state, pending.bytes);
    frame
}

fn transmit_frame<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize>(
    state: &mut VirtioNetRawState<P, T, QUEUE_SIZE>,
    stats: &VirtioNetStats,
    frame: &[u8],
) -> StepOutcome<(), NoProgress> {
    complete_tx(state, stats, usize::MAX);
    if !state.raw.can_send() {
        stats.tx_busy.fetch_add(1, Ordering::Relaxed);
        let token = net_delegate_wait_token();
        return StepOutcome::yield_on_wait_source(NoProgress, token.source_id(), token.interest());
    }

    let header_len = core::mem::size_of::<VirtioNetHdr>();
    let mut bytes = alloc::vec![0; header_len + frame.len()];
    let header_len = match state.raw.fill_buffer_header(&mut bytes) {
        Ok(len) => len,
        Err(_) => {
            stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            return StepOutcome::Err(Errno::EIO);
        }
    };
    bytes[header_len..header_len + frame.len()].copy_from_slice(frame);

    match unsafe { state.raw.transmit_begin(&bytes) } {
        Ok(token) => {
            state.tx_pending.push(PendingTxBuffer {
                token,
                bytes,
                frame_len: frame.len(),
            });
            stats.tx_submitted.fetch_add(1, Ordering::Relaxed);
            stats
                .tx_bytes
                .fetch_add(frame.len() as u64, Ordering::Relaxed);
            StepOutcome::Done(())
        }
        Err(VirtioError::QueueFull) => {
            stats.tx_busy.fetch_add(1, Ordering::Relaxed);
            let token = net_delegate_wait_token();
            StepOutcome::yield_on_wait_source(NoProgress, token.source_id(), token.interest())
        }
        Err(_) => {
            stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            StepOutcome::Err(Errno::EIO)
        }
    }
}

fn tx_readiness<P: TxPlatform, T: Transport, const QUEUE_SIZE: usize>(
    inner: &SpinMutex<Option<VirtioNetRawState<P, T, QUEUE_SIZE>>>,
    stats: &VirtioNetStats,
    initialized: &AtomicBool,
) -> PacketTxReadiness {
    if !initialized.load(Ordering::Acquire) {
        return PacketTxReadiness::Busy;
    }
    let mut inner = inner.lock();
    let Some(state) = inner.as_mut() else {
        return PacketTxReadiness::Busy;
    };
    complete_tx(state, stats, usize::MAX);
    if state.raw.can_send() {
        PacketTxReadiness::Ready
    } else {
        PacketTxReadiness::Busy
    }
}

fn pack_mac(mac: EthernetAddress) -> u64 {
    let octets = mac.octets();
    let mut raw = 0u64;
    let mut idx = 0usize;
    while idx < octets.len() {
        raw |= (octets[idx] as u64) << (idx * 8);
        idx += 1;
    }
    raw
}

fn unpack_mac(raw: u64) -> EthernetAddress {
    let mut octets = [0u8; 6];
    let mut idx = 0usize;
    while idx < octets.len() {
        octets[idx] = ((raw >> (idx * 8)) & 0xff) as u8;
        idx += 1;
    }
    EthernetAddress::new(octets)
}

impl<P: TxPlatform, const QUEUE_SIZE: usize> NetDeviceOps for VirtioMmioNet<P, QUEUE_SIZE> {
    fn receive(&self) -> Option<RxFrame> {
        let mut inner = self.inner.lock();
        let state = inner.as_mut()?;
        receive_one(state, &self.stats)
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        if !self.initialized.load(Ordering::Acquire) {
            return StepOutcome::Err(Errno::ENODEV);
        }
        let mut inner = self.inner.lock();
        let Some(state) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV);
        };
        transmit_frame(state, &self.stats, frame)
    }

    fn tx_readiness(&self, _guard: &Guard<'_>) -> PacketTxReadiness {
        tx_readiness(&self.inner, &self.stats, &self.initialized)
    }

    fn enable_interrupts(&self) {
        VirtioMmioNet::enable_interrupts(self);
    }

    fn ack_interrupt_and_fire(&self) -> NetDeviceIrqOutcome {
        let outcome = VirtioMmioNet::ack_interrupt_and_fire(self);
        NetDeviceIrqOutcome {
            rx_ready: outcome.rx_ready,
            tx_completed: outcome.tx_completed,
            tx_completed_bytes: 0,
            poll_wakes: outcome.poll_wakes,
        }
    }

    fn mac_addr(&self) -> EthernetAddress {
        unpack_mac(self.mac.load(Ordering::Acquire))
    }

    fn mtu(&self) -> u16 {
        self.mtu.load(Ordering::Acquire)
    }
}

impl<P: TxPlatform, const QUEUE_SIZE: usize> NetDeviceOps for VirtioPciNet<P, QUEUE_SIZE> {
    fn receive(&self) -> Option<RxFrame> {
        let mut inner = self.inner.lock();
        let state = inner.as_mut()?;
        receive_one(state, &self.stats)
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<(), NoProgress> {
        if !self.initialized.load(Ordering::Acquire) {
            return StepOutcome::Err(Errno::ENODEV);
        }
        let mut inner = self.inner.lock();
        let Some(state) = inner.as_mut() else {
            return StepOutcome::Err(Errno::ENODEV);
        };
        transmit_frame(state, &self.stats, frame)
    }

    fn tx_readiness(&self, _guard: &Guard<'_>) -> PacketTxReadiness {
        tx_readiness(&self.inner, &self.stats, &self.initialized)
    }

    fn enable_interrupts(&self) {
        VirtioPciNet::enable_interrupts(self);
    }

    fn ack_interrupt_and_fire(&self) -> NetDeviceIrqOutcome {
        let outcome = VirtioPciNet::ack_interrupt_and_fire(self);
        NetDeviceIrqOutcome {
            rx_ready: outcome.rx_ready,
            tx_completed: outcome.tx_completed,
            tx_completed_bytes: 0,
            poll_wakes: outcome.poll_wakes,
        }
    }

    fn mac_addr(&self) -> EthernetAddress {
        unpack_mac(self.mac.load(Ordering::Acquire))
    }

    fn mtu(&self) -> u16 {
        self.mtu.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_pack_round_trips_six_octets() {
        let mac = EthernetAddress::new([0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);

        assert_eq!(unpack_mac(pack_mac(mac)), mac);
    }
}
