//! Minimal 802.1Q VLAN netdevice for rtnetlink command tests.
//!
//! This models VLAN link *metadata* only: a VLAN interface can be created on
//! top of a parent link, brought up/down, listed, and deleted. It deliberately
//! does NOT implement a VLAN tagging data path — `transmit` is a no-op like the
//! dummy device — so tests that exercise tagged traffic must still fail or
//! TCONF until a real data path exists.

use alloc::boxed::Box;

use crate::device::DevT;
use crate::execution::{Guard, StepOutcome};
use crate::net::packet::RxFrame;

use super::{EthernetAddress, NetDeviceKind, NetDeviceOps, NetDeviceRegistration};

pub const VLAN_DEFAULT_MTU: u16 = 1500;

pub struct VlanDevice {
    mac: EthernetAddress,
    mtu: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VlanConfig {
    pub name: &'static str,
    pub devt: DevT,
    pub mac: EthernetAddress,
    pub mtu: u16,
}

#[derive(Clone, Copy)]
pub struct VlanInstance {
    pub registration: &'static NetDeviceRegistration,
    pub device: &'static VlanDevice,
}

impl VlanDevice {
    pub const fn new(mac: EthernetAddress, mtu: u16) -> Self {
        Self { mac, mtu }
    }
}

impl NetDeviceOps for VlanDevice {
    fn receive(&self) -> Option<RxFrame> {
        None
    }

    fn transmit(&self, _frame: &[u8], _guard: &Guard<'_>) -> StepOutcome<()> {
        StepOutcome::Done(())
    }

    fn mac_addr(&self) -> EthernetAddress {
        self.mac
    }

    fn mtu(&self) -> u16 {
        self.mtu
    }

    fn device_kind(&self) -> NetDeviceKind {
        NetDeviceKind::Vlan
    }
}

pub fn create_vlan_for_test_or_bootstrap(config: VlanConfig) -> VlanInstance {
    let device = Box::leak(Box::new(VlanDevice::new(config.mac, config.mtu)));
    let registration = Box::leak(Box::new(NetDeviceRegistration {
        devt: config.devt,
        name: config.name,
        ops: device,
    }));

    VlanInstance {
        registration,
        device,
    }
}
