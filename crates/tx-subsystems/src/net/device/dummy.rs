//! Minimal dummy netdevice for rtnetlink command tests.

use alloc::boxed::Box;

use crate::device::DevT;
use crate::execution::{Guard, StepOutcome};
use crate::net::packet::RxFrame;

use super::{EthernetAddress, NetDeviceKind, NetDeviceOps, NetDeviceRegistration};

pub const DUMMY_DEFAULT_MTU: u16 = 1500;

pub struct DummyDevice {
    mac: EthernetAddress,
    mtu: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DummyConfig {
    pub name: &'static str,
    pub devt: DevT,
    pub mac: EthernetAddress,
    pub mtu: u16,
}

#[derive(Clone, Copy)]
pub struct DummyInstance {
    pub registration: &'static NetDeviceRegistration,
    pub device: &'static DummyDevice,
}

impl DummyDevice {
    pub const fn new(mac: EthernetAddress, mtu: u16) -> Self {
        Self { mac, mtu }
    }
}

impl NetDeviceOps for DummyDevice {
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
        NetDeviceKind::Dummy
    }
}

pub fn create_dummy_for_test_or_bootstrap(config: DummyConfig) -> DummyInstance {
    let device = Box::leak(Box::new(DummyDevice::new(config.mac, config.mtu)));
    let registration = Box::leak(Box::new(NetDeviceRegistration {
        devt: config.devt,
        name: config.name,
        ops: device,
    }));

    DummyInstance {
        registration,
        device,
    }
}
