use alloc::collections::VecDeque;

use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::{Ipv4Address, Ipv6Address};
use crate::sync::SpinMutex;

use super::{build_icmpv4_echo_reply, parse_icmpv4_loopback_packet, Icmpv4Event};

pub static LOOPBACK_IFACE: LoopbackIface = LoopbackIface::new_loopback();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IfaceCommon {
    ipv4_addr: Ipv4Address,
    netmask: Ipv4Address,
    gateway: Option<Ipv4Address>,
    mtu: u16,
    // IPv6 V1: on-link v6 config. IPv6 V3b added `ipv6_gateway` so
    // decide_ipv6_route forwards off-link v6 via the default route's gateway.
    ipv6_addr: Option<Ipv6Address>,
    ipv6_prefix_len: Option<u8>,
    ipv6_gateway: Option<Ipv6Address>,
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
            ipv6_addr: None,
            ipv6_prefix_len: None,
            ipv6_gateway: None,
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
            ipv6_addr: None,
            ipv6_prefix_len: None,
            ipv6_gateway: None,
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

    /// IPv6 V1: attach on-link v6 config (address + prefix). Chained after
    /// `with_gateway` at the namespace iface-build site.
    pub fn with_ipv6(self, ipv6_addr: Option<Ipv6Address>, ipv6_prefix_len: Option<u8>) -> Self {
        Self {
            ipv6_addr,
            ipv6_prefix_len,
            ..self
        }
    }

    pub const fn ipv6_addr(self) -> Option<Ipv6Address> {
        self.ipv6_addr
    }

    pub const fn ipv6_prefix_len(self) -> Option<u8> {
        self.ipv6_prefix_len
    }

    /// IPv6 V3b: attach the off-link v6 next-hop (default route's gateway).
    /// Chained after `with_ipv6` at the namespace iface-build site.
    pub fn with_ipv6_gateway(self, ipv6_gateway: Option<Ipv6Address>) -> Self {
        Self {
            ipv6_gateway,
            ..self
        }
    }

    pub const fn ipv6_gateway(self) -> Option<Ipv6Address> {
        self.ipv6_gateway
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
