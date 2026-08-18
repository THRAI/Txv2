//! Network device static registration shell.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::device::DevT;
use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::admin::NetAdminAuthority;
use crate::net::packet::{PacketTxReadiness, RxFrame};

mod bridge;
mod dummy;
mod veth;
mod virtio;
mod vlan;

pub use bridge::{
    create_bridge_for_test_or_bootstrap, BridgeConfig, BridgeDevice, BridgeForwardOutcome,
    BridgeInstance, BridgePortSnapshot, BridgeSnapshot, BRIDGE_FORWARD_BUDGET_DEFAULT,
};
pub use dummy::{
    create_dummy_for_test_or_bootstrap, DummyConfig, DummyDevice, DummyInstance, DUMMY_DEFAULT_MTU,
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
pub use vlan::{
    create_vlan_for_test_or_bootstrap, VlanConfig, VlanDevice, VlanInstance, VLAN_DEFAULT_MTU,
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
    Dummy,
    Veth,
    Bridge,
    Vlan,
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

const NET_REGISTRY_VACANT: u8 = 0;
const NET_REGISTRY_RESERVED: u8 = 1;
const NET_REGISTRY_PUBLISHED: u8 = 2;

static NET_REGISTRY_STATE: AtomicU8 = AtomicU8::new(NET_REGISTRY_VACANT);
static NET_REGISTRY_LEN: AtomicUsize = AtomicUsize::new(0);
static mut NET_REGISTRY: [Option<&'static NetDeviceRegistration>; MAX_STATIC_NET_DEVICES] =
    [None; MAX_STATIC_NET_DEVICES];

/// Rollback-safe reservation for one boot-time network-device publication.
///
/// Preparation validates the entire proposal and acquires the registry's
/// private one-shot reservation without making any entry visible. Dropping an
/// uncommitted value releases that reservation, so a larger boot transaction
/// can abandon its prepared registries without consuming the publication slot.
pub struct PreparedNetDeviceRegistry<'a> {
    regs: &'a [&'static NetDeviceRegistration],
    committed: bool,
}

impl<'a> PreparedNetDeviceRegistry<'a> {
    /// Publish the prepared registrations exactly once.
    ///
    /// Every fallible check completed in [`prepare_net_devices`]. This method
    /// performs no allocation and has no failure path; the final length store
    /// is the Release publication observed by registry readers.
    pub fn commit(mut self) {
        for (idx, reg) in self.regs.iter().copied().enumerate() {
            unsafe {
                NET_REGISTRY[idx] = Some(reg);
            }
        }
        NET_REGISTRY_LEN.store(self.regs.len(), Ordering::Release);
        NET_REGISTRY_STATE.store(NET_REGISTRY_PUBLISHED, Ordering::Release);
        self.committed = true;
    }
}

impl<'a> Drop for PreparedNetDeviceRegistry<'a> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = NET_REGISTRY_STATE.compare_exchange(
                NET_REGISTRY_RESERVED,
                NET_REGISTRY_VACANT,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }
}

/// Validate and reserve one boot-time network-device registry publication.
///
/// This function does not mutate the visible registry. Capacity and identity
/// validation happen before the private one-shot reservation is acquired, so
/// every rejected proposal leaves the registry empty and retryable.
pub fn prepare_net_devices<'a>(
    regs: &'a [&'static NetDeviceRegistration],
) -> Result<PreparedNetDeviceRegistry<'a>, Errno> {
    if regs.len() > MAX_STATIC_NET_DEVICES {
        return Err(Errno::ENOMEM);
    }
    for (idx, reg) in regs.iter().copied().enumerate() {
        if regs[..idx]
            .iter()
            .copied()
            .any(|seen| seen.devt == reg.devt || seen.name == reg.name)
        {
            return Err(Errno::EEXIST);
        }
    }

    if NET_REGISTRY_STATE
        .compare_exchange(
            NET_REGISTRY_VACANT,
            NET_REGISTRY_RESERVED,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return Err(Errno::EEXIST);
    }

    Ok(PreparedNetDeviceRegistry {
        regs,
        committed: false,
    })
}

/// Validate and publish the boot network-device registry in one compatibility
/// call. New global boot transactions should retain the prepared value until
/// every registry is ready, then call [`PreparedNetDeviceRegistry::commit`].
pub fn register_net_devices(regs: &'static [&'static NetDeviceRegistration]) -> StepOutcome<()> {
    match prepare_net_devices(regs) {
        Ok(prepared) => {
            prepared.commit();
            StepOutcome::Done(())
        }
        Err(errno) => StepOutcome::Err(errno),
    }
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

pub fn net_device_registry_len() -> usize {
    NET_REGISTRY_LEN.load(Ordering::Acquire)
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
    NET_REGISTRY_STATE.store(NET_REGISTRY_VACANT, Ordering::Release);
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
        assert_eq!(register_net_devices(REGS), StepOutcome::Err(Errno::EEXIST));
        reset_net_registry_for_test();
    }

    #[test]
    fn prepared_net_registry_stays_private_until_commit() {
        let _lock = NET_REGISTRY_TEST_LOCK
            .lock()
            .expect("net registry test lock");
        reset_net_registry_for_test();
        static REGS: &[&NetDeviceRegistration] = &[&ETH0];

        let prepared = prepare_net_devices(REGS).expect("prepare registry");
        assert!(net_device_snapshot().is_empty());
        assert_eq!(net_device_registry_len(), 0);
        assert!(matches!(prepare_net_devices(REGS), Err(Errno::EEXIST)));

        prepared.commit();
        assert!(core::ptr::eq(
            net_device_by_name(b"eth0").expect("eth0"),
            &ETH0
        ));
        assert!(matches!(prepare_net_devices(REGS), Err(Errno::EEXIST)));
        reset_net_registry_for_test();
    }

    #[test]
    fn dropping_prepared_net_registry_rolls_back_reservation() {
        let _lock = NET_REGISTRY_TEST_LOCK
            .lock()
            .expect("net registry test lock");
        reset_net_registry_for_test();
        static REGS: &[&NetDeviceRegistration] = &[&ETH0];

        let prepared = prepare_net_devices(REGS).expect("prepare registry");
        drop(prepared);
        assert!(net_device_snapshot().is_empty());

        let retry = prepare_net_devices(REGS).expect("retry prepare registry");
        retry.commit();
        assert!(core::ptr::eq(
            net_device_by_devt(DevT::new(97, 0)).expect("devt"),
            &ETH0
        ));
        reset_net_registry_for_test();
    }

    #[test]
    fn prepare_net_registry_accepts_a_local_registration_slice() {
        let _lock = NET_REGISTRY_TEST_LOCK
            .lock()
            .expect("net registry test lock");
        reset_net_registry_for_test();
        let local_regs = [&ETH0];

        let prepared = prepare_net_devices(&local_regs).expect("prepare local slice");
        assert!(net_device_snapshot().is_empty());
        prepared.commit();

        assert!(core::ptr::eq(
            net_device_by_name(b"eth0").expect("eth0"),
            &ETH0
        ));
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
        static DUP_DEVT: NetDeviceRegistration = NetDeviceRegistration {
            devt: DevT::new(97, 0),
            name: "eth1",
            ops: &NULL_NET,
        };
        static DUP_NAME_REGS: &[&NetDeviceRegistration] = &[&ETH0, &DUP_NAME];
        static DUP_DEVT_REGS: &[&NetDeviceRegistration] = &[&ETH0, &DUP_DEVT];

        assert_eq!(
            register_net_devices(DUP_NAME_REGS),
            StepOutcome::Err(Errno::EEXIST)
        );
        assert!(net_device_snapshot().is_empty());
        assert_eq!(
            register_net_devices(DUP_DEVT_REGS),
            StepOutcome::Err(Errno::EEXIST)
        );
        assert!(net_device_snapshot().is_empty());

        // Validation failure must not consume the one-shot publication slot or
        // leave a partially visible first entry behind.
        static VALID: &[&NetDeviceRegistration] = &[&ETH0];
        assert_eq!(register_net_devices(VALID), StepOutcome::Done(()));
        let snapshot = net_device_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(core::ptr::eq(snapshot[0], &ETH0));
        reset_net_registry_for_test();
    }

    #[test]
    fn static_net_registry_capacity_failure_is_retryable() {
        let _lock = NET_REGISTRY_TEST_LOCK
            .lock()
            .expect("net registry test lock");
        reset_net_registry_for_test();
        static TOO_MANY: [&NetDeviceRegistration; MAX_STATIC_NET_DEVICES + 1] =
            [&ETH0; MAX_STATIC_NET_DEVICES + 1];

        assert_eq!(
            register_net_devices(&TOO_MANY),
            StepOutcome::Err(Errno::ENOMEM)
        );
        assert!(net_device_snapshot().is_empty());

        static VALID: &[&NetDeviceRegistration] = &[&ETH0];
        assert_eq!(register_net_devices(VALID), StepOutcome::Done(()));
        assert!(core::ptr::eq(
            net_device_snapshot().first().copied().expect("eth0"),
            &ETH0
        ));
        reset_net_registry_for_test();
    }
}
