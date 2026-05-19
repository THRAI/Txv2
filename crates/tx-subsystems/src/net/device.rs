//! Network device static registration shell.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::device::DevT;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::admin::NetAdminAuthority;
use crate::net::packet::{PacketTxReadiness, RxFrame};

mod bridge;
mod veth;
mod virtio;

pub use bridge::{
    create_bridge_for_test_or_bootstrap, BridgeConfig, BridgeDevice, BridgeForwardOutcome,
    BridgeInstance, BridgePortSnapshot, BridgeSnapshot, BRIDGE_FORWARD_BUDGET_DEFAULT,
};
pub use veth::{
    create_veth_pair_for_test_or_bootstrap, VethDevice, VethEndpointConfig, VethPair,
    VethPairConfig, VethStatsSnapshot, VETH_DEFAULT_MTU,
};
pub use virtio::{
    VirtioNetConfig, VirtioNetDevice, VirtioNetFeatureSet, VirtioNetIrqEvent, VirtioNetIrqOutcome,
    VirtioNetQueueConfig, VirtioNetRxInjectOutcome, VirtioNetStats, VirtioNetStatsSnapshot,
    VirtioNetTxCompleteOutcome, VIRTIO_NET0_DEVICE, VIRTIO_NET0_REGISTRATION,
    VIRTIO_NET_DEFAULT_MTU, VIRTIO_NET_STAGING_MAJOR,
};

pub type NetDeviceIrqOutcome = VirtioNetIrqOutcome;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EthernetAddress(pub [u8; 6]);

impl EthernetAddress {
    pub const BROADCAST: Self = Self([0xff; 6]);

    pub const fn new(octets: [u8; 6]) -> Self {
        Self(octets)
    }

    pub const fn octets(self) -> [u8; 6] {
        self.0
    }

    pub const fn is_broadcast(self) -> bool {
        matches!(self.0, [0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
    }

    pub const fn is_multicast(self) -> bool {
        (self.0[0] & 1) != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetDeviceKind {
    Loopback,
    Ethernet,
    Veth,
    Bridge,
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
    fn device_kind(&self) -> NetDeviceKind {
        NetDeviceKind::Ethernet
    }

    fn bridge_snapshot(&self) -> Option<BridgeSnapshot> {
        None
    }

    fn bridge_add_port(
        &self,
        _authority: NetAdminAuthority,
        _registration: &'static NetDeviceRegistration,
    ) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    fn bridge_remove_port(
        &self,
        _authority: NetAdminAuthority,
        _registration: &'static NetDeviceRegistration,
    ) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    fn bridge_poll(&self, _guard: &Guard<'_>) -> Option<BridgeForwardOutcome> {
        None
    }

    fn enable_interrupts(&self) {}

    fn ack_interrupt_and_fire(&self) -> NetDeviceIrqOutcome {
        NetDeviceIrqOutcome::default()
    }
}

#[derive(Clone, Copy)]
pub struct NetDeviceRegistration {
    pub devt: DevT,
    pub name: &'static str,
    pub ops: &'static dyn NetDeviceOps,
}

const MAX_STATIC_NET_DEVICES: usize = 8;

static NET_REGISTRY_INITIALIZED: AtomicBool = AtomicBool::new(false);
static NET_REGISTRY_LEN: AtomicUsize = AtomicUsize::new(0);
static mut NET_REGISTRY: [Option<&'static NetDeviceRegistration>; MAX_STATIC_NET_DEVICES] =
    [None; MAX_STATIC_NET_DEVICES];

pub fn register_net_devices(regs: &'static [&'static NetDeviceRegistration]) -> StepOutcome<()> {
    if NET_REGISTRY_INITIALIZED.swap(true, Ordering::AcqRel) {
        return StepOutcome::Err(Errno::EEXIST.into());
    }
    if regs.len() > MAX_STATIC_NET_DEVICES {
        return StepOutcome::Err(Errno::ENOMEM.into());
    }

    for (idx, reg) in regs.iter().copied().enumerate() {
        if regs[..idx]
            .iter()
            .copied()
            .any(|seen| seen.devt == reg.devt || seen.name == reg.name)
        {
            return StepOutcome::Err(Errno::EEXIST.into());
        }
        unsafe {
            NET_REGISTRY[idx] = Some(reg);
        }
    }
    NET_REGISTRY_LEN.store(regs.len(), Ordering::Release);
    StepOutcome::Done(())
}

pub fn net_device_by_name(name: &[u8]) -> Option<&'static NetDeviceRegistration> {
    net_device_snapshot()
        .into_iter()
        .find(|reg| reg.name.as_bytes() == name)
}

pub fn net_device_by_devt(devt: DevT) -> Option<&'static NetDeviceRegistration> {
    net_device_snapshot()
        .into_iter()
        .find(|reg| reg.devt == devt)
}

pub fn net_device_snapshot() -> Vec<&'static NetDeviceRegistration> {
    let len = NET_REGISTRY_LEN.load(Ordering::Acquire);
    let mut out = Vec::with_capacity(len);
    let mut idx = 0;
    while idx < len {
        let entry = unsafe { NET_REGISTRY[idx] };
        if let Some(reg) = entry {
            out.push(reg);
        }
        idx += 1;
    }
    out
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_net_registry_for_test() {
    let mut idx = 0;
    while idx < MAX_STATIC_NET_DEVICES {
        unsafe {
            NET_REGISTRY[idx] = None;
        }
        idx += 1;
    }
    NET_REGISTRY_LEN.store(0, Ordering::Release);
    NET_REGISTRY_INITIALIZED.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    static NET_REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct NullNetDevice;

    impl NetDeviceOps for NullNetDevice {
        fn receive(&self) -> Option<RxFrame> {
            None
        }

        fn transmit(&self, _frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<()> {
            StepOutcome::Done(())
        }

        fn mac_addr(&self) -> EthernetAddress {
            EthernetAddress::new([0x02, 0, 0, 0, 0, 0x42])
        }

        fn mtu(&self) -> u16 {
            VIRTIO_NET_DEFAULT_MTU
        }
    }

    static NULL_NET: NullNetDevice = NullNetDevice;
    static ETH0: NetDeviceRegistration = NetDeviceRegistration {
        devt: DevT::new(97, 0),
        name: "eth0",
        ops: &NULL_NET,
    };

    #[test]
    fn static_net_registry_indexes_by_name_and_devt_once() {
        let _lock = NET_REGISTRY_TEST_LOCK
            .lock()
            .expect("net registry test lock");
        reset_net_registry_for_test();
        static REGS: &[&NetDeviceRegistration] = &[&ETH0];

        assert_eq!(register_net_devices(REGS), StepOutcome::Done(()));
        assert!(core::ptr::eq(
            net_device_by_name(b"eth0").expect("eth0"),
            &ETH0
        ));
        assert!(core::ptr::eq(
            net_device_by_devt(DevT::new(97, 0)).expect("devt"),
            &ETH0
        ));
        let snapshot = net_device_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(core::ptr::eq(snapshot[0], &ETH0));
        assert_eq!(
            register_net_devices(REGS),
            StepOutcome::Err(Errno::EEXIST.into())
        );
        reset_net_registry_for_test();
    }

    #[test]
    fn static_net_registry_rejects_duplicate_names_or_devts() {
        let _lock = NET_REGISTRY_TEST_LOCK
            .lock()
            .expect("net registry test lock");
        reset_net_registry_for_test();
        static DUP_NAME: NetDeviceRegistration = NetDeviceRegistration {
            devt: DevT::new(97, 1),
            name: "eth0",
            ops: &NULL_NET,
        };
        static REGS: &[&NetDeviceRegistration] = &[&ETH0, &DUP_NAME];

        assert_eq!(
            register_net_devices(REGS),
            StepOutcome::Err(Errno::EEXIST.into())
        );
        assert!(net_device_snapshot().is_empty());
        reset_net_registry_for_test();
    }
}
