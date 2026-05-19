//! Netfilter hook skeleton.
//!
//! This is intentionally only the hook surface and default-ACCEPT policy.
//! Rule storage, iptables translation, conntrack, and NAT land after the
//! bridge/route datapaths are stable.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::wire::{
    IpAddress, IpProtocol, Ipv4Address as SmoltcpIpv4Address, Ipv4Packet, TcpPacket, UdpPacket,
};

use crate::execution::Errno;
use crate::net::protocol::{parse_icmpv4_payload, Icmpv4Event};
use crate::net::structure::Ipv4Address;
use crate::net::NetNamespacePayload;
use crate::sync::SpinMutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterHook {
    Prerouting,
    Input,
    Forward,
    Output,
    Postrouting,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterVerdict {
    Accept,
    Drop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetfilterFrameContext {
    pub hook: NetfilterHook,
    pub bridge: Option<&'static str>,
    pub ingress: Option<&'static str>,
    pub egress: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterTable {
    Filter,
    Nat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterTarget {
    Accept,
    Drop,
    Masquerade,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetfilterIpv4Cidr {
    pub addr: Ipv4Address,
    pub prefix_len: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetfilterRule {
    pub table: NetfilterTable,
    pub hook: NetfilterHook,
    pub src: Option<NetfilterIpv4Cidr>,
    pub out_iface: Option<&'static str>,
    pub target: NetfilterTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterConntrackProtocol {
    Icmp,
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetfilterConntrackSnapshot {
    pub protocol: NetfilterConntrackProtocol,
    pub original_src: Ipv4Address,
    pub original_src_port: u16,
    pub masquerade_src: Ipv4Address,
    pub masquerade_src_port: u16,
    pub external_dst: Ipv4Address,
    pub external_dst_port: u16,
    pub icmp_ident: u16,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetfilterStatsSnapshot {
    pub prerouting: u64,
    pub input: u64,
    pub forward: u64,
    pub output: u64,
    pub postrouting: u64,
}

struct NetfilterStats {
    prerouting: AtomicU64,
    input: AtomicU64,
    forward: AtomicU64,
    output: AtomicU64,
    postrouting: AtomicU64,
}

static NETFILTER_STATS: NetfilterStats = NetfilterStats::new();
static NETFILTER_RULES: SpinMutex<Vec<NetfilterRule>> = SpinMutex::new(Vec::new());
static NETFILTER_CONNTRACK: SpinMutex<Vec<MasqueradeConntrack>> = SpinMutex::new(Vec::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MasqueradeConntrack {
    protocol: NetfilterConntrackProtocol,
    original_src: Ipv4Address,
    original_src_port: u16,
    masquerade_src: Ipv4Address,
    masquerade_src_port: u16,
    external_dst: Ipv4Address,
    external_dst_port: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct L4Tuple {
    protocol: NetfilterConntrackProtocol,
    src_port: u16,
    dst_port: u16,
}

impl NetfilterStats {
    const fn new() -> Self {
        Self {
            prerouting: AtomicU64::new(0),
            input: AtomicU64::new(0),
            forward: AtomicU64::new(0),
            output: AtomicU64::new(0),
            postrouting: AtomicU64::new(0),
        }
    }

    fn counter(&self, hook: NetfilterHook) -> &AtomicU64 {
        match hook {
            NetfilterHook::Prerouting => &self.prerouting,
            NetfilterHook::Input => &self.input,
            NetfilterHook::Forward => &self.forward,
            NetfilterHook::Output => &self.output,
            NetfilterHook::Postrouting => &self.postrouting,
        }
    }

    fn snapshot(&self) -> NetfilterStatsSnapshot {
        NetfilterStatsSnapshot {
            prerouting: self.prerouting.load(Ordering::Relaxed),
            input: self.input.load(Ordering::Relaxed),
            forward: self.forward.load(Ordering::Relaxed),
            output: self.output.load(Ordering::Relaxed),
            postrouting: self.postrouting.load(Ordering::Relaxed),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn reset(&self) {
        self.prerouting.store(0, Ordering::Relaxed);
        self.input.store(0, Ordering::Relaxed);
        self.forward.store(0, Ordering::Relaxed);
        self.output.store(0, Ordering::Relaxed);
        self.postrouting.store(0, Ordering::Relaxed);
    }
}

pub fn run_frame_hook(ctx: NetfilterFrameContext, _frame: &[u8]) -> NetfilterVerdict {
    NETFILTER_STATS
        .counter(ctx.hook)
        .fetch_add(1, Ordering::Relaxed);
    for rule in NETFILTER_RULES.lock().iter().copied() {
        if rule.table != NetfilterTable::Filter || rule.hook != ctx.hook {
            continue;
        }
        if !rule_matches_context(rule, ctx) {
            continue;
        }
        return match rule.target {
            NetfilterTarget::Drop => NetfilterVerdict::Drop,
            NetfilterTarget::Accept | NetfilterTarget::Masquerade => NetfilterVerdict::Accept,
        };
    }
    NetfilterVerdict::Accept
}

pub fn add_netfilter_rule_for_test_or_bootstrap(rule: NetfilterRule) -> Result<(), Errno> {
    if let Some(src) = rule.src {
        if src.prefix_len > 32 {
            return Err(Errno::EINVAL);
        }
    }
    NETFILTER_RULES.lock().push(rule);
    Ok(())
}

pub fn add_masquerade_rule_for_test_or_bootstrap(
    src: NetfilterIpv4Cidr,
    out_iface: &'static str,
) -> Result<(), Errno> {
    add_netfilter_rule_for_test_or_bootstrap(NetfilterRule {
        table: NetfilterTable::Nat,
        hook: NetfilterHook::Postrouting,
        src: Some(src),
        out_iface: Some(out_iface),
        target: NetfilterTarget::Masquerade,
    })
}

pub fn remove_netfilter_rule_for_test_or_bootstrap(index: usize) -> Result<(), Errno> {
    let mut rules = NETFILTER_RULES.lock();
    if index >= rules.len() {
        return Err(Errno::ENOENT);
    }
    rules.remove(index);
    Ok(())
}

pub fn flush_netfilter_rules_and_conntrack_for_test_or_bootstrap() {
    NETFILTER_RULES.lock().clear();
    NETFILTER_CONNTRACK.lock().clear();
}

pub fn netfilter_rules_snapshot() -> Vec<NetfilterRule> {
    NETFILTER_RULES.lock().clone()
}

pub fn netfilter_conntrack_snapshot() -> Vec<NetfilterConntrackSnapshot> {
    NETFILTER_CONNTRACK
        .lock()
        .iter()
        .copied()
        .map(|entry| NetfilterConntrackSnapshot {
            protocol: entry.protocol,
            original_src: entry.original_src,
            original_src_port: entry.original_src_port,
            masquerade_src: entry.masquerade_src,
            masquerade_src_port: entry.masquerade_src_port,
            external_dst: entry.external_dst,
            external_dst_port: entry.external_dst_port,
            icmp_ident: if entry.protocol == NetfilterConntrackProtocol::Icmp {
                entry.original_src_port
            } else {
                0
            },
        })
        .collect()
}

pub fn apply_netfilter_control_command(
    netns: &NetNamespacePayload,
    bytes: &[u8],
) -> Result<(), Errno> {
    let command = core::str::from_utf8(bytes)
        .map_err(|_| Errno::EINVAL)?
        .trim_matches(|ch: char| ch.is_ascii_whitespace());
    if command.is_empty() {
        return Ok(());
    }

    let mut parts = command.split_ascii_whitespace();
    match parts.next().ok_or(Errno::EINVAL)? {
        "flush" => {
            if parts.next().is_some() {
                return Err(Errno::EINVAL);
            }
            flush_netfilter_rules_and_conntrack_for_test_or_bootstrap();
            Ok(())
        }
        "delete" => {
            let index = parse_usize(parts.next().ok_or(Errno::EINVAL)?)?;
            if parts.next().is_some() {
                return Err(Errno::EINVAL);
            }
            remove_netfilter_rule_for_test_or_bootstrap(index)
        }
        "masquerade" => {
            let cidr = parse_ipv4_cidr(parts.next().ok_or(Errno::EINVAL)?)?;
            let out_iface = resolve_iface_name(netns, parts.next().ok_or(Errno::EINVAL)?)?;
            if parts.next().is_some() {
                return Err(Errno::EINVAL);
            }
            add_masquerade_rule_for_test_or_bootstrap(cidr, out_iface)
        }
        "filter" => {
            let hook = parse_hook(parts.next().ok_or(Errno::EINVAL)?)?;
            let target = parse_target(parts.next().ok_or(Errno::EINVAL)?)?;
            let mut out_iface = None;
            for part in parts {
                let Some(name) = part.strip_prefix("out=") else {
                    return Err(Errno::EINVAL);
                };
                out_iface = Some(resolve_iface_name(netns, name)?);
            }
            add_netfilter_rule_for_test_or_bootstrap(NetfilterRule {
                table: NetfilterTable::Filter,
                hook,
                src: None,
                out_iface,
                target,
            })
        }
        _ => Err(Errno::EINVAL),
    }
}

pub fn apply_postrouting_nat_ipv4(
    ctx: NetfilterFrameContext,
    packet: &[u8],
    masquerade_src: Ipv4Address,
) -> Option<Vec<u8>> {
    let ipv4 = Ipv4Packet::new_checked(packet).ok()?;
    let src = from_smoltcp_ipv4(ipv4.src_addr());
    let dst = from_smoltcp_ipv4(ipv4.dst_addr());
    let tuple = l4_tuple(ipv4.next_header(), src, dst, ipv4.payload())?;
    if !NETFILTER_RULES.lock().iter().copied().any(|rule| {
        rule.table == NetfilterTable::Nat
            && rule.hook == NetfilterHook::Postrouting
            && rule.target == NetfilterTarget::Masquerade
            && rule_matches_context(rule, ctx)
            && rule.src.map_or(true, |cidr| ipv4_in_cidr(src, cidr))
    }) {
        return None;
    }

    remember_masquerade(MasqueradeConntrack {
        protocol: tuple.protocol,
        original_src: src,
        original_src_port: tuple.src_port,
        masquerade_src,
        masquerade_src_port: tuple.src_port,
        external_dst: dst,
        external_dst_port: tuple.dst_port,
    });
    Some(rewrite_ipv4_nat(
        packet,
        Some(masquerade_src),
        None,
        None,
        None,
    )?)
}

pub fn apply_prerouting_nat_ipv4(ctx: NetfilterFrameContext, packet: &[u8]) -> Option<Vec<u8>> {
    let _ctx = ctx;
    let ipv4 = Ipv4Packet::new_checked(packet).ok()?;
    let src = from_smoltcp_ipv4(ipv4.src_addr());
    let dst = from_smoltcp_ipv4(ipv4.dst_addr());
    let tuple = l4_tuple(ipv4.next_header(), src, dst, ipv4.payload())?;
    let entry = NETFILTER_CONNTRACK
        .lock()
        .iter()
        .find(|entry| {
            entry.protocol == tuple.protocol
                && entry.external_dst == src
                && entry.masquerade_src == dst
                && entry_matches_reply_tuple(**entry, tuple)
        })
        .copied()?;
    let dst_port = match entry.protocol {
        NetfilterConntrackProtocol::Icmp => None,
        NetfilterConntrackProtocol::Tcp | NetfilterConntrackProtocol::Udp => {
            Some(entry.original_src_port)
        }
    };
    Some(rewrite_ipv4_nat(
        packet,
        None,
        Some(entry.original_src),
        None,
        dst_port,
    )?)
}

pub fn netfilter_stats_snapshot() -> NetfilterStatsSnapshot {
    NETFILTER_STATS.snapshot()
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_netfilter_for_test() {
    NETFILTER_STATS.reset();
    flush_netfilter_rules_and_conntrack_for_test_or_bootstrap();
}

fn rule_matches_context(rule: NetfilterRule, ctx: NetfilterFrameContext) -> bool {
    if let Some(out_iface) = rule.out_iface {
        if ctx.egress != Some(out_iface) {
            return false;
        }
    }
    true
}

fn remember_masquerade(entry: MasqueradeConntrack) {
    let mut conntrack = NETFILTER_CONNTRACK.lock();
    if let Some(existing) = conntrack.iter_mut().find(|existing| {
        existing.protocol == entry.protocol
            && existing.original_src == entry.original_src
            && existing.original_src_port == entry.original_src_port
            && existing.masquerade_src == entry.masquerade_src
            && existing.masquerade_src_port == entry.masquerade_src_port
            && existing.external_dst == entry.external_dst
            && existing.external_dst_port == entry.external_dst_port
    }) {
        *existing = entry;
        return;
    }
    conntrack.push(entry);
}

fn entry_matches_reply_tuple(entry: MasqueradeConntrack, tuple: L4Tuple) -> bool {
    match entry.protocol {
        NetfilterConntrackProtocol::Icmp => entry.masquerade_src_port == tuple.src_port,
        NetfilterConntrackProtocol::Tcp | NetfilterConntrackProtocol::Udp => {
            entry.external_dst_port == tuple.src_port && entry.masquerade_src_port == tuple.dst_port
        }
    }
}

fn l4_tuple(
    protocol: IpProtocol,
    src: Ipv4Address,
    dst: Ipv4Address,
    payload: &[u8],
) -> Option<L4Tuple> {
    match protocol {
        IpProtocol::Icmp => match parse_icmpv4_payload(src, dst, payload) {
            Icmpv4Event::EchoRequest(packet) | Icmpv4Event::EchoReply(packet) => Some(L4Tuple {
                protocol: NetfilterConntrackProtocol::Icmp,
                src_port: packet.ident,
                dst_port: 0,
            }),
            Icmpv4Event::Malformed | Icmpv4Event::Unsupported => None,
        },
        IpProtocol::Tcp => {
            let packet = TcpPacket::new_checked(payload).ok()?;
            Some(L4Tuple {
                protocol: NetfilterConntrackProtocol::Tcp,
                src_port: packet.src_port(),
                dst_port: packet.dst_port(),
            })
        }
        IpProtocol::Udp => {
            let packet = UdpPacket::new_checked(payload).ok()?;
            Some(L4Tuple {
                protocol: NetfilterConntrackProtocol::Udp,
                src_port: packet.src_port(),
                dst_port: packet.dst_port(),
            })
        }
        _ => None,
    }
}

fn rewrite_ipv4_nat(
    packet: &[u8],
    src: Option<Ipv4Address>,
    dst: Option<Ipv4Address>,
    src_port: Option<u16>,
    dst_port: Option<u16>,
) -> Option<Vec<u8>> {
    let mut out = Vec::from(packet);
    let (header_len, total_len, protocol, old_src, old_dst) = {
        let ipv4 = Ipv4Packet::new_checked(out.as_slice()).ok()?;
        (
            ipv4.header_len() as usize,
            ipv4.total_len() as usize,
            ipv4.next_header(),
            from_smoltcp_ipv4(ipv4.src_addr()),
            from_smoltcp_ipv4(ipv4.dst_addr()),
        )
    };
    if total_len > out.len() || header_len > total_len {
        return None;
    }
    let new_src = src.unwrap_or(old_src);
    let new_dst = dst.unwrap_or(old_dst);
    {
        let mut ipv4 = Ipv4Packet::new_checked(out.as_mut_slice()).ok()?;
        if src.is_some() {
            ipv4.set_src_addr(to_smoltcp_ipv4(new_src));
        }
        if dst.is_some() {
            ipv4.set_dst_addr(to_smoltcp_ipv4(new_dst));
        }
        ipv4.fill_checksum();
    }

    let src_addr = IpAddress::Ipv4(to_smoltcp_ipv4(new_src));
    let dst_addr = IpAddress::Ipv4(to_smoltcp_ipv4(new_dst));
    let payload = &mut out[header_len..total_len];
    match protocol {
        IpProtocol::Tcp => {
            let mut tcp = TcpPacket::new_checked(payload).ok()?;
            if let Some(port) = src_port {
                tcp.set_src_port(port);
            }
            if let Some(port) = dst_port {
                tcp.set_dst_port(port);
            }
            tcp.fill_checksum(&src_addr, &dst_addr);
        }
        IpProtocol::Udp => {
            let mut udp = UdpPacket::new_checked(payload).ok()?;
            if let Some(port) = src_port {
                udp.set_src_port(port);
            }
            if let Some(port) = dst_port {
                udp.set_dst_port(port);
            }
            udp.fill_checksum(&src_addr, &dst_addr);
        }
        IpProtocol::Icmp => {}
        _ => return None,
    }
    Some(out)
}

fn resolve_iface_name(netns: &NetNamespacePayload, name: &str) -> Result<&'static str, Errno> {
    netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == name)
        .map(|link| link.name)
        .ok_or(Errno::ENODEV)
}

fn parse_hook(name: &str) -> Result<NetfilterHook, Errno> {
    match name {
        "PREROUTING" | "prerouting" => Ok(NetfilterHook::Prerouting),
        "INPUT" | "input" => Ok(NetfilterHook::Input),
        "FORWARD" | "forward" => Ok(NetfilterHook::Forward),
        "OUTPUT" | "output" => Ok(NetfilterHook::Output),
        "POSTROUTING" | "postrouting" => Ok(NetfilterHook::Postrouting),
        _ => Err(Errno::EINVAL),
    }
}

fn parse_target(name: &str) -> Result<NetfilterTarget, Errno> {
    match name {
        "ACCEPT" | "accept" => Ok(NetfilterTarget::Accept),
        "DROP" | "drop" => Ok(NetfilterTarget::Drop),
        _ => Err(Errno::EINVAL),
    }
}

fn parse_ipv4_cidr(input: &str) -> Result<NetfilterIpv4Cidr, Errno> {
    let (addr, prefix_len) = match input.split_once('/') {
        Some((addr, prefix)) => (addr, parse_u8(prefix)?),
        None => (input, 32),
    };
    if prefix_len > 32 {
        return Err(Errno::EINVAL);
    }
    Ok(NetfilterIpv4Cidr {
        addr: parse_ipv4(addr)?,
        prefix_len,
    })
}

fn parse_ipv4(input: &str) -> Result<Ipv4Address, Errno> {
    let mut octets = [0u8; 4];
    let mut parts = input.split('.');
    for octet in &mut octets {
        *octet = parse_u8(parts.next().ok_or(Errno::EINVAL)?)?;
    }
    if parts.next().is_some() {
        return Err(Errno::EINVAL);
    }
    Ok(Ipv4Address::new(octets))
}

fn parse_u8(input: &str) -> Result<u8, Errno> {
    let value = parse_usize(input)?;
    if value > u8::MAX as usize {
        return Err(Errno::EINVAL);
    }
    Ok(value as u8)
}

fn parse_usize(input: &str) -> Result<usize, Errno> {
    if input.is_empty() {
        return Err(Errno::EINVAL);
    }
    let mut value = 0usize;
    for &byte in input.as_bytes() {
        if !byte.is_ascii_digit() {
            return Err(Errno::EINVAL);
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add((byte - b'0') as usize))
            .ok_or(Errno::EINVAL)?;
    }
    Ok(value)
}

fn ipv4_in_cidr(addr: Ipv4Address, cidr: NetfilterIpv4Cidr) -> bool {
    let mask = prefix_mask(cidr.prefix_len.min(32));
    (ipv4_to_u32(addr) & mask) == (ipv4_to_u32(cidr.addr) & mask)
}

fn prefix_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    }
}

fn ipv4_to_u32(addr: Ipv4Address) -> u32 {
    u32::from_be_bytes(addr.octets())
}

fn to_smoltcp_ipv4(addr: Ipv4Address) -> SmoltcpIpv4Address {
    let [a, b, c, d] = addr.octets();
    SmoltcpIpv4Address::new(a, b, c, d)
}

fn from_smoltcp_ipv4(addr: SmoltcpIpv4Address) -> Ipv4Address {
    Ipv4Address::new(addr.octets())
}
