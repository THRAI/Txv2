//! Netfilter hook skeleton.
//!
//! This is intentionally only the hook surface and default-ACCEPT policy.
//! Rule storage, iptables translation, conntrack, and NAT land after the
//! bridge/route datapaths are stable.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::wire::{IpProtocol, Ipv4Address as SmoltcpIpv4Address, Ipv4Packet};

use crate::execution::Errno;
use crate::net::protocol::{parse_icmpv4_payload, Icmpv4Event};
use crate::net::structure::Ipv4Address;
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
pub struct NetfilterConntrackSnapshot {
    pub original_src: Ipv4Address,
    pub masquerade_src: Ipv4Address,
    pub external_dst: Ipv4Address,
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
static NETFILTER_CONNTRACK: SpinMutex<Vec<IcmpMasqueradeConntrack>> = SpinMutex::new(Vec::new());

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IcmpMasqueradeConntrack {
    original_src: Ipv4Address,
    masquerade_src: Ipv4Address,
    external_dst: Ipv4Address,
    icmp_ident: u16,
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

pub fn netfilter_rules_snapshot() -> Vec<NetfilterRule> {
    NETFILTER_RULES.lock().clone()
}

pub fn netfilter_conntrack_snapshot() -> Vec<NetfilterConntrackSnapshot> {
    NETFILTER_CONNTRACK
        .lock()
        .iter()
        .copied()
        .map(|entry| NetfilterConntrackSnapshot {
            original_src: entry.original_src,
            masquerade_src: entry.masquerade_src,
            external_dst: entry.external_dst,
            icmp_ident: entry.icmp_ident,
        })
        .collect()
}

pub fn apply_postrouting_nat_ipv4(
    ctx: NetfilterFrameContext,
    packet: &[u8],
    masquerade_src: Ipv4Address,
) -> Option<Vec<u8>> {
    let ipv4 = Ipv4Packet::new_checked(packet).ok()?;
    let src = from_smoltcp_ipv4(ipv4.src_addr());
    let dst = from_smoltcp_ipv4(ipv4.dst_addr());
    let ident = icmp_echo_ident(ipv4.next_header(), src, dst, ipv4.payload())?;
    if !NETFILTER_RULES.lock().iter().copied().any(|rule| {
        rule.table == NetfilterTable::Nat
            && rule.hook == NetfilterHook::Postrouting
            && rule.target == NetfilterTarget::Masquerade
            && rule_matches_context(rule, ctx)
            && rule.src.map_or(true, |cidr| ipv4_in_cidr(src, cidr))
    }) {
        return None;
    }

    remember_icmp_masquerade(IcmpMasqueradeConntrack {
        original_src: src,
        masquerade_src,
        external_dst: dst,
        icmp_ident: ident,
    });
    Some(rewrite_ipv4_addr(packet, Some(masquerade_src), None)?)
}

pub fn apply_prerouting_nat_ipv4(ctx: NetfilterFrameContext, packet: &[u8]) -> Option<Vec<u8>> {
    let _ctx = ctx;
    let ipv4 = Ipv4Packet::new_checked(packet).ok()?;
    let src = from_smoltcp_ipv4(ipv4.src_addr());
    let dst = from_smoltcp_ipv4(ipv4.dst_addr());
    let ident = icmp_echo_ident(ipv4.next_header(), src, dst, ipv4.payload())?;
    let original_dst = NETFILTER_CONNTRACK
        .lock()
        .iter()
        .find(|entry| {
            entry.external_dst == src && entry.masquerade_src == dst && entry.icmp_ident == ident
        })
        .map(|entry| entry.original_src)?;
    Some(rewrite_ipv4_addr(packet, None, Some(original_dst))?)
}

pub fn netfilter_stats_snapshot() -> NetfilterStatsSnapshot {
    NETFILTER_STATS.snapshot()
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_netfilter_for_test() {
    NETFILTER_STATS.reset();
    NETFILTER_RULES.lock().clear();
    NETFILTER_CONNTRACK.lock().clear();
}

fn rule_matches_context(rule: NetfilterRule, ctx: NetfilterFrameContext) -> bool {
    if let Some(out_iface) = rule.out_iface {
        if ctx.egress != Some(out_iface) {
            return false;
        }
    }
    true
}

fn remember_icmp_masquerade(entry: IcmpMasqueradeConntrack) {
    let mut conntrack = NETFILTER_CONNTRACK.lock();
    if let Some(existing) = conntrack.iter_mut().find(|existing| {
        existing.original_src == entry.original_src
            && existing.masquerade_src == entry.masquerade_src
            && existing.external_dst == entry.external_dst
            && existing.icmp_ident == entry.icmp_ident
    }) {
        *existing = entry;
        return;
    }
    conntrack.push(entry);
}

fn icmp_echo_ident(
    protocol: IpProtocol,
    src: Ipv4Address,
    dst: Ipv4Address,
    payload: &[u8],
) -> Option<u16> {
    if protocol != IpProtocol::Icmp {
        return None;
    }
    match parse_icmpv4_payload(src, dst, payload) {
        Icmpv4Event::EchoRequest(packet) | Icmpv4Event::EchoReply(packet) => Some(packet.ident),
        Icmpv4Event::Malformed | Icmpv4Event::Unsupported => None,
    }
}

fn rewrite_ipv4_addr(
    packet: &[u8],
    src: Option<Ipv4Address>,
    dst: Option<Ipv4Address>,
) -> Option<Vec<u8>> {
    let mut out = Vec::from(packet);
    let mut ipv4 = Ipv4Packet::new_checked(out.as_mut_slice()).ok()?;
    if let Some(src) = src {
        ipv4.set_src_addr(to_smoltcp_ipv4(src));
    }
    if let Some(dst) = dst {
        ipv4.set_dst_addr(to_smoltcp_ipv4(dst));
    }
    ipv4.fill_checksum();
    Some(out)
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
