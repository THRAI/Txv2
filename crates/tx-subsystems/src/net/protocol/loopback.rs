use alloc::collections::VecDeque;

use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::Ipv4Address;
use crate::sync::SpinMutex;

use super::{build_icmpv4_echo_reply, parse_icmpv4_loopback_packet, Icmpv4Event};

pub static LOOPBACK_IFACE: LoopbackIface = LoopbackIface::new_loopback();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IfaceCommon {
    ipv4_addr: Ipv4Address,
    netmask: Ipv4Address,
    gateway: Option<Ipv4Address>,
    mtu: u16,
}

pub struct LoopbackIface {
    loopback_queue: SpinMutex<VecDeque<LoopbackIpPacket>>,
    common: IfaceCommon,
}

impl IfaceCommon {
    pub const fn new(ipv4_addr: Ipv4Address, netmask: Ipv4Address, mtu: u16) -> Self {
        Self {
            ipv4_addr,
            netmask,
            gateway: None,
            mtu,
        }
    }

    pub const fn with_gateway(
        ipv4_addr: Ipv4Address,
        netmask: Ipv4Address,
        gateway: Option<Ipv4Address>,
        mtu: u16,
    ) -> Self {
        Self {
            ipv4_addr,
            netmask,
            gateway,
            mtu,
        }
    }

    pub const fn loopback() -> Self {
        Self::new(
            Ipv4Address::LOOPBACK,
            Ipv4Address::new([255, 0, 0, 0]),
            65_535,
        )
    }

    pub const fn ipv4_addr(self) -> Ipv4Address {
        self.ipv4_addr
    }

    pub const fn netmask(self) -> Ipv4Address {
        self.netmask
    }

    pub const fn gateway(self) -> Option<Ipv4Address> {
        self.gateway
    }

    pub const fn mtu(self) -> u16 {
        self.mtu
    }
}

impl LoopbackIface {
    pub const fn new(common: IfaceCommon) -> Self {
        Self {
            loopback_queue: SpinMutex::new(VecDeque::new()),
            common,
        }
    }

    pub const fn new_loopback() -> Self {
        Self::new(IfaceCommon::loopback())
    }

    pub fn dispatch_ip(&self, packet: LoopbackIpPacket) -> bool {
        if packet.is_empty() || packet.len() > usize::from(self.common.mtu()) {
            return false;
        }
        self.loopback_queue.lock().push_back(packet);
        true
    }

    pub fn pop_ingress(&self) -> Option<LoopbackIpPacket> {
        self.loopback_queue.lock().pop_front()
    }

    /// Remove the first packet accepted by `predicate` without disturbing the
    /// relative order of any other packet.
    ///
    /// A loopback TCP handshake may run inline on several CPUs at once.  A
    /// pop/inspect/requeue loop is not sufficient there: another CPU can
    /// observe the temporarily removed queue head and both handshakes can
    /// consume each other's packets.  Selection therefore has to be one
    /// atomic queue operation.  The queue lock is held only while locating and
    /// removing one packet; protocol processing remains outside the lock.
    pub fn take_ingress_matching(
        &self,
        mut predicate: impl FnMut(&LoopbackIpPacket) -> bool,
    ) -> Option<LoopbackIpPacket> {
        let mut queue = self.loopback_queue.lock();
        let index = queue.iter().position(&mut predicate)?;
        queue.remove(index)
    }

    pub fn has_ingress_matching(
        &self,
        mut predicate: impl FnMut(&LoopbackIpPacket) -> bool,
    ) -> bool {
        self.loopback_queue.lock().iter().any(&mut predicate)
    }

    pub fn pending_packets(&self) -> usize {
        self.loopback_queue.lock().len()
    }

    pub fn clear_for_test_or_bootstrap(&self) {
        self.loopback_queue.lock().clear();
    }

    pub fn process_icmpv4_echo_once(&self) -> Option<Icmpv4Event> {
        let packet = self.pop_ingress()?;
        let event = parse_icmpv4_loopback_packet(&packet);
        if let Icmpv4Event::EchoRequest(request) = &event {
            if request.dst == self.local_ipv4() || request.dst == Ipv4Address::BROADCAST {
                let reply = build_icmpv4_echo_reply(&request.reply_packet());
                let _ = self.dispatch_ip(reply);
            }
        }
        Some(event)
    }

    pub const fn local_ipv4(&self) -> Ipv4Address {
        self.common.ipv4_addr()
    }

    pub const fn netmask(&self) -> Ipv4Address {
        self.common.netmask()
    }

    pub const fn gateway(&self) -> Option<Ipv4Address> {
        self.common.gateway()
    }

    pub const fn mtu(&self) -> u16 {
        self.common.mtu()
    }
}

pub const fn loopback_iface() -> &'static LoopbackIface {
    &LOOPBACK_IFACE
}
