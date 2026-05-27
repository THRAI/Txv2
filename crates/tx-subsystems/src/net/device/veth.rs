//! Virtual Ethernet pair staging device.
//!
//! A veth endpoint is a `NetDeviceOps`: transmitting on one endpoint enqueues
//! the Ethernet frame into its peer endpoint's receive queue. Namespace
//! ownership, routing, bridge membership, and netfilter hooks live above this
//! device layer.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::device::DevT;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::packet::{PacketTxReadiness, RxFrame};
use crate::sync::SpinMutex;

use super::{EthernetAddress, NetDeviceKind, NetDeviceOps, NetDeviceRegistration};

pub const VETH_DEFAULT_MTU: u16 = 1500;

pub struct VethDevice {
    mac: EthernetAddress,
    mtu: u16,
    rx: SpinMutex<VecDeque<RxFrame>>,
    peer: SpinMutex<Option<&'static VethDevice>>,
    rx_packets: AtomicU64,
    tx_packets: AtomicU64,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    tx_errors: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VethEndpointConfig {
    pub name: &'static str,
    pub devt: DevT,
    pub mac: EthernetAddress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VethPairConfig {
    pub left: VethEndpointConfig,
    pub right: VethEndpointConfig,
    pub mtu: u16,
}

#[derive(Clone, Copy)]
pub struct VethPair {
    pub left: &'static NetDeviceRegistration,
    pub right: &'static NetDeviceRegistration,
    pub left_device: &'static VethDevice,
    pub right_device: &'static VethDevice,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VethStatsSnapshot {
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub tx_errors: u64,
    pub rx_pending: usize,
}

impl VethDevice {
    pub fn new(mac: EthernetAddress, mtu: u16) -> Self {
        Self {
            mac,
            mtu,
            rx: SpinMutex::new(VecDeque::new()),
            peer: SpinMutex::new(None),
            rx_packets: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            tx_errors: AtomicU64::new(0),
        }
    }

    pub fn set_peer_for_test_or_bootstrap(&'static self, peer: &'static VethDevice) {
        *self.peer.lock() = Some(peer);
    }

    pub fn pending_rx(&self) -> usize {
        self.rx.lock().len()
    }

    pub fn stats_snapshot(&self) -> VethStatsSnapshot {
        VethStatsSnapshot {
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            tx_errors: self.tx_errors.load(Ordering::Relaxed),
            rx_pending: self.pending_rx(),
        }
    }

    fn push_rx(&self, frame: &[u8]) {
        let mut bytes = Vec::with_capacity(frame.len());
        bytes.extend_from_slice(frame);
        self.rx.lock().push_back(RxFrame::new(bytes));
        self.rx_packets.fetch_add(1, Ordering::Relaxed);
        self.rx_bytes
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
    }

    fn frame_limit(&self) -> usize {
        usize::from(self.mtu) + 14
    }
}

impl NetDeviceOps for VethDevice {
    fn receive(&self) -> Option<RxFrame> {
        self.rx.lock().pop_front()
    }

    fn transmit(&self, frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<()> {
        if frame.is_empty() || frame.len() > self.frame_limit() {
            self.tx_errors.fetch_add(1, Ordering::Relaxed);
            return StepOutcome::Err(Errno::EINVAL);
        }

        let Some(peer) = *self.peer.lock() else {
            self.tx_errors.fetch_add(1, Ordering::Relaxed);
            return StepOutcome::Err(Errno::ENODEV);
        };

        peer.push_rx(frame);
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
        self.tx_bytes
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        StepOutcome::Done(())
    }

    fn tx_readiness(&self, _guard: &Guard<'_>) -> PacketTxReadiness {
        PacketTxReadiness::Ready
    }

    fn mac_addr(&self) -> EthernetAddress {
        self.mac
    }

    fn mtu(&self) -> u16 {
        self.mtu
    }

    fn device_kind(&self) -> NetDeviceKind {
        NetDeviceKind::Veth
    }
}

pub fn create_veth_pair_for_test_or_bootstrap(config: VethPairConfig) -> VethPair {
    let left_device = Box::leak(Box::new(VethDevice::new(config.left.mac, config.mtu)));
    let right_device = Box::leak(Box::new(VethDevice::new(config.right.mac, config.mtu)));
    left_device.set_peer_for_test_or_bootstrap(right_device);
    right_device.set_peer_for_test_or_bootstrap(left_device);

    let left = Box::leak(Box::new(NetDeviceRegistration {
        devt: config.left.devt,
        name: config.left.name,
        ops: left_device,
    }));
    let right = Box::leak(Box::new(NetDeviceRegistration {
        devt: config.right.devt,
        name: config.right.name,
        ops: right_device,
    }));

    VethPair {
        left,
        right,
        left_device,
        right_device,
    }
}
