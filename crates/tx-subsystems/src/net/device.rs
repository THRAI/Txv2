//! Network device static registration shell.

use crate::device::DevT;
use crate::execution::{Guard, StepOutcome};
use crate::net::packet::{PacketTxReadiness, RxFrame};

mod virtio;

pub use virtio::{
    VirtioNetConfig, VirtioNetDevice, VirtioNetFeatureSet, VirtioNetIrqEvent, VirtioNetIrqOutcome,
    VirtioNetQueueConfig, VirtioNetRxInjectOutcome, VirtioNetStats, VirtioNetStatsSnapshot,
    VirtioNetTxCompleteOutcome, VIRTIO_NET0_DEVICE, VIRTIO_NET0_REGISTRATION,
    VIRTIO_NET_DEFAULT_MTU, VIRTIO_NET_STAGING_MAJOR,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EthernetAddress(pub [u8; 6]);

impl EthernetAddress {
    pub const BROADCAST: Self = Self([0xff; 6]);

    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

pub trait NetDeviceOps: Send + Sync + 'static {
    fn receive(&self) -> Option<RxFrame>;
    fn transmit(&self, frame: &[u8], guard: &Guard<'_>) -> StepOutcome<()>;
    fn tx_readiness(&self, guard: &Guard<'_>) -> PacketTxReadiness {
        let _guard = guard;
        PacketTxReadiness::Ready
    }
    fn mac_addr(&self) -> EthernetAddress;
    fn mtu(&self) -> u16;
}

#[derive(Clone, Copy)]
pub struct NetDeviceRegistration {
    pub devt: DevT,
    pub name: &'static str,
    pub ops: &'static dyn NetDeviceOps,
}
