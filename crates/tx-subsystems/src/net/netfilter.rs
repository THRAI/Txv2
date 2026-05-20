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
use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::protocol::{parse_icmpv4_payload, Icmpv4Event};
use crate::net::structure::Ipv4Address;

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
    Dnat,
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
    pub protocol: Option<NetfilterConntrackProtocol>,
    pub src: Option<NetfilterIpv4Cidr>,
    pub dst: Option<NetfilterIpv4Cidr>,
    pub dst_port: Option<u16>,
    pub in_iface: Option<&'static str>,
    pub out_iface: Option<&'static str>,
    pub target: NetfilterTarget,
    pub compat_target: Option<&'static str>,
    pub to_addr: Option<Ipv4Address>,
    pub to_port: Option<u16>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetfilterRuleCounters {
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetfilterRuleSnapshot {
    pub rule: NetfilterRule,
    pub counters: NetfilterRuleCounters,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterConntrackProtocol {
    Icmp,
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetfilterConntrackSnapshot {
    pub kind: NetfilterNatKind,
    pub protocol: NetfilterConntrackProtocol,
    pub original_src: Ipv4Address,
    pub original_src_port: u16,
    pub masquerade_src: Ipv4Address,
    pub masquerade_src_port: u16,
    pub external_dst: Ipv4Address,
    pub external_dst_port: u16,
    pub icmp_ident: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetfilterNatKind {
    Masquerade,
    Dnat,
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

#[derive(Debug, Eq, PartialEq)]
pub struct NetfilterState {
    rules: Vec<NetfilterRuleEntry>,
    masquerade_conntrack: Vec<MasqueradeConntrack>,
    dnat_conntrack: Vec<DnatConntrack>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NetfilterRuleEntry {
    rule: NetfilterRule,
    counters: NetfilterRuleCounters,
}

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
struct DnatConntrack {
    protocol: NetfilterConntrackProtocol,
    client_src: Ipv4Address,
    client_src_port: u16,
    public_dst: Ipv4Address,
    public_dst_port: u16,
    private_dst: Ipv4Address,
    private_dst_port: u16,
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

impl NetfilterState {
    pub const fn new() -> Self {
        Self {
            rules: Vec::new(),
            masquerade_conntrack: Vec::new(),
            dnat_conntrack: Vec::new(),
        }
    }

    fn rule_snapshots(&self) -> Vec<NetfilterRuleSnapshot> {
        self.rules
            .iter()
            .copied()
            .map(|entry| NetfilterRuleSnapshot {
                rule: entry.rule,
                counters: entry.counters,
            })
            .collect()
    }

    fn rules(&self) -> Vec<NetfilterRule> {
        self.rules.iter().copied().map(|entry| entry.rule).collect()
    }

    fn conntrack_snapshot(&self) -> Vec<NetfilterConntrackSnapshot> {
        let mut out: Vec<_> = self
            .masquerade_conntrack
            .iter()
            .copied()
            .map(|entry| NetfilterConntrackSnapshot {
                kind: NetfilterNatKind::Masquerade,
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
            .collect();
        out.extend(
            self.dnat_conntrack
                .iter()
                .copied()
                .map(|entry| NetfilterConntrackSnapshot {
                    kind: NetfilterNatKind::Dnat,
                    protocol: entry.protocol,
                    original_src: entry.private_dst,
                    original_src_port: entry.private_dst_port,
                    masquerade_src: entry.public_dst,
                    masquerade_src_port: entry.public_dst_port,
                    external_dst: entry.client_src,
                    external_dst_port: entry.client_src_port,
                    icmp_ident: 0,
                }),
        );
        out
    }

    fn push_rule(&mut self, rule: NetfilterRule) {
        self.rules.push(NetfilterRuleEntry {
            rule,
            counters: NetfilterRuleCounters::default(),
        });
    }

    fn remove_rule(&mut self, index: usize) -> Result<(), Errno> {
        if index >= self.rules.len() {
            return Err(Errno::ENOENT);
        }
        self.rules.remove(index);
        Ok(())
    }

    fn clear(&mut self) {
        self.rules.clear();
        self.masquerade_conntrack.clear();
        self.dnat_conntrack.clear();
    }

    fn retain_rules(&mut self, mut keep: impl FnMut(NetfilterRule) -> bool) {
        self.rules.retain(|entry| keep(entry.rule));
    }
}

impl Default for NetfilterState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run_frame_hook(ctx: NetfilterFrameContext, frame: &[u8]) -> NetfilterVerdict {
    let netns = initial_net_namespace_payload();
    run_frame_hook_in_namespace(&netns, ctx, frame)
}

pub fn run_frame_hook_in_namespace(
    netns: &NetNamespacePayload,
    ctx: NetfilterFrameContext,
    frame: &[u8],
) -> NetfilterVerdict {
    NETFILTER_STATS
        .counter(ctx.hook)
        .fetch_add(1, Ordering::Relaxed);
    let mut state = netns.netfilter_state().lock();
    for entry in state.rules.iter_mut() {
        let rule = entry.rule;
        if rule.table != NetfilterTable::Filter || rule.hook != ctx.hook {
            continue;
        }
        if !rule_matches_context(rule, ctx) {
            continue;
        }
        entry.counters.packets = entry.counters.packets.saturating_add(1);
        entry.counters.bytes = entry.counters.bytes.saturating_add(frame.len() as u64);
        return match rule.target {
            NetfilterTarget::Drop => NetfilterVerdict::Drop,
            NetfilterTarget::Accept | NetfilterTarget::Masquerade | NetfilterTarget::Dnat => {
                NetfilterVerdict::Accept
            }
        };
    }
    NetfilterVerdict::Accept
}

pub fn add_netfilter_rule_for_test_or_bootstrap(rule: NetfilterRule) -> Result<(), Errno> {
    let netns = initial_net_namespace_payload();
    add_netfilter_rule_in_namespace_for_test_or_bootstrap(&netns, rule)
}

pub fn add_netfilter_rule_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
    rule: NetfilterRule,
) -> Result<(), Errno> {
    if let Some(src) = rule.src {
        if src.prefix_len > 32 {
            return Err(Errno::EINVAL);
        }
    }
    if let Some(dst) = rule.dst {
        if dst.prefix_len > 32 {
            return Err(Errno::EINVAL);
        }
    }
    if rule.target == NetfilterTarget::Dnat && rule.to_addr.is_none() {
        return Err(Errno::EINVAL);
    }
    netns.netfilter_state().lock().push_rule(rule);
    Ok(())
}

pub fn add_masquerade_rule_for_test_or_bootstrap(
    src: NetfilterIpv4Cidr,
    out_iface: &'static str,
) -> Result<(), Errno> {
    let netns = initial_net_namespace_payload();
    add_masquerade_rule_in_namespace_for_test_or_bootstrap(&netns, src, out_iface)
}

pub fn add_masquerade_rule_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
    src: NetfilterIpv4Cidr,
    out_iface: &'static str,
) -> Result<(), Errno> {
    add_netfilter_rule_in_namespace_for_test_or_bootstrap(
        netns,
        NetfilterRule {
            table: NetfilterTable::Nat,
            hook: NetfilterHook::Postrouting,
            protocol: None,
            src: Some(src),
            dst: None,
            dst_port: None,
            in_iface: None,
            out_iface: Some(out_iface),
            target: NetfilterTarget::Masquerade,
            compat_target: None,
            to_addr: None,
            to_port: None,
        },
    )
}

pub fn add_dnat_rule_for_test_or_bootstrap(
    protocol: NetfilterConntrackProtocol,
    public_dst: Ipv4Address,
    public_port: u16,
    private_dst: Ipv4Address,
    private_port: u16,
) -> Result<(), Errno> {
    let netns = initial_net_namespace_payload();
    add_dnat_rule_in_namespace_for_test_or_bootstrap(
        &netns,
        protocol,
        public_dst,
        public_port,
        private_dst,
        private_port,
    )
}

pub fn add_dnat_rule_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
    protocol: NetfilterConntrackProtocol,
    public_dst: Ipv4Address,
    public_port: u16,
    private_dst: Ipv4Address,
    private_port: u16,
) -> Result<(), Errno> {
    add_netfilter_rule_in_namespace_for_test_or_bootstrap(
        netns,
        NetfilterRule {
            table: NetfilterTable::Nat,
            hook: NetfilterHook::Prerouting,
            protocol: Some(protocol),
            src: None,
            dst: Some(NetfilterIpv4Cidr {
                addr: public_dst,
                prefix_len: 32,
            }),
            dst_port: Some(public_port),
            in_iface: None,
            out_iface: None,
            target: NetfilterTarget::Dnat,
            compat_target: None,
            to_addr: Some(private_dst),
            to_port: Some(private_port),
        },
    )
}

pub fn remove_netfilter_rule_for_test_or_bootstrap(index: usize) -> Result<(), Errno> {
    let netns = initial_net_namespace_payload();
    remove_netfilter_rule_in_namespace_for_test_or_bootstrap(&netns, index)
}

pub fn remove_netfilter_rule_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
    index: usize,
) -> Result<(), Errno> {
    netns.netfilter_state().lock().remove_rule(index)
}

pub fn remove_netfilter_rules_for_table_for_test_or_bootstrap(table: NetfilterTable) {
    let netns = initial_net_namespace_payload();
    remove_netfilter_rules_for_table_in_namespace_for_test_or_bootstrap(&netns, table);
}

pub fn remove_netfilter_rules_for_table_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
    table: NetfilterTable,
) {
    netns
        .netfilter_state()
        .lock()
        .retain_rules(|rule| rule.table != table);
}

pub fn remove_netfilter_rules_for_chain_for_test_or_bootstrap(
    table: NetfilterTable,
    hook: NetfilterHook,
) {
    let netns = initial_net_namespace_payload();
    remove_netfilter_rules_for_chain_in_namespace_for_test_or_bootstrap(&netns, table, hook);
}

pub fn remove_netfilter_rules_for_chain_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
    table: NetfilterTable,
    hook: NetfilterHook,
) {
    netns
        .netfilter_state()
        .lock()
        .retain_rules(|rule| rule.table != table || rule.hook != hook);
}

pub fn flush_netfilter_rules_and_conntrack_for_test_or_bootstrap() {
    let netns = initial_net_namespace_payload();
    flush_netfilter_rules_and_conntrack_in_namespace_for_test_or_bootstrap(&netns);
}

pub fn flush_netfilter_rules_and_conntrack_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
) {
    netns.netfilter_state().lock().clear();
}

pub fn cleanup_netfilter_device_state_for_test_or_bootstrap(
    iface_name: &'static str,
    ipv4_addr: Option<Ipv4Address>,
) {
    let netns = initial_net_namespace_payload();
    cleanup_netfilter_device_state_in_namespace_for_test_or_bootstrap(
        &netns, iface_name, ipv4_addr,
    );
}

pub fn cleanup_netfilter_device_state_in_namespace_for_test_or_bootstrap(
    netns: &NetNamespacePayload,
    iface_name: &'static str,
    ipv4_addr: Option<Ipv4Address>,
) {
    let mut state = netns.netfilter_state().lock();
    state.retain_rules(|rule| {
        let touches_iface = rule.in_iface == Some(iface_name) || rule.out_iface == Some(iface_name);
        let touches_addr = ipv4_addr.is_some_and(|addr| {
            rule.to_addr == Some(addr)
                || rule
                    .src
                    .is_some_and(|cidr| cidr.prefix_len == 32 && cidr.addr == addr)
                || rule
                    .dst
                    .is_some_and(|cidr| cidr.prefix_len == 32 && cidr.addr == addr)
        });
        !touches_iface && !touches_addr
    });
    if let Some(addr) = ipv4_addr {
        state.masquerade_conntrack.retain(|entry| {
            entry.original_src != addr && entry.masquerade_src != addr && entry.external_dst != addr
        });
        state.dnat_conntrack.retain(|entry| {
            entry.client_src != addr && entry.public_dst != addr && entry.private_dst != addr
        });
    }
}

pub fn netfilter_rules_snapshot() -> Vec<NetfilterRule> {
    let netns = initial_net_namespace_payload();
    netfilter_rules_snapshot_for_namespace(&netns)
}

pub fn netfilter_rules_snapshot_for_namespace(netns: &NetNamespacePayload) -> Vec<NetfilterRule> {
    netns.netfilter_state().lock().rules()
}

pub fn netfilter_rule_snapshots() -> Vec<NetfilterRuleSnapshot> {
    let netns = initial_net_namespace_payload();
    netfilter_rule_snapshots_for_namespace(&netns)
}

pub fn netfilter_rule_snapshots_for_namespace(
    netns: &NetNamespacePayload,
) -> Vec<NetfilterRuleSnapshot> {
    netns.netfilter_state().lock().rule_snapshots()
}

pub fn netfilter_conntrack_snapshot() -> Vec<NetfilterConntrackSnapshot> {
    let netns = initial_net_namespace_payload();
    netfilter_conntrack_snapshot_for_namespace(&netns)
}

pub fn netfilter_conntrack_snapshot_for_namespace(
    netns: &NetNamespacePayload,
) -> Vec<NetfilterConntrackSnapshot> {
    netns.netfilter_state().lock().conntrack_snapshot()
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
            flush_netfilter_rules_and_conntrack_in_namespace_for_test_or_bootstrap(netns);
            Ok(())
        }
        "delete" => {
            let index = parse_usize(parts.next().ok_or(Errno::EINVAL)?)?;
            if parts.next().is_some() {
                return Err(Errno::EINVAL);
            }
            remove_netfilter_rule_in_namespace_for_test_or_bootstrap(netns, index)
        }
        "masquerade" => {
            let cidr = parse_ipv4_cidr(parts.next().ok_or(Errno::EINVAL)?)?;
            let out_iface = resolve_iface_name(netns, parts.next().ok_or(Errno::EINVAL)?)?;
            if parts.next().is_some() {
                return Err(Errno::EINVAL);
            }
            add_masquerade_rule_in_namespace_for_test_or_bootstrap(netns, cidr, out_iface)
        }
        "dnat" => {
            let protocol = parse_protocol(parts.next().ok_or(Errno::EINVAL)?)?;
            let public_dst = parse_ipv4(parts.next().ok_or(Errno::EINVAL)?)?;
            let public_port = parse_u16(parts.next().ok_or(Errno::EINVAL)?)?;
            let private_dst = parse_ipv4(parts.next().ok_or(Errno::EINVAL)?)?;
            let private_port = parse_u16(parts.next().ok_or(Errno::EINVAL)?)?;
            if parts.next().is_some() {
                return Err(Errno::EINVAL);
            }
            add_dnat_rule_in_namespace_for_test_or_bootstrap(
                netns,
                protocol,
                public_dst,
                public_port,
                private_dst,
                private_port,
            )
        }
        "filter" => {
            let hook = parse_hook(parts.next().ok_or(Errno::EINVAL)?)?;
            let target = parse_target(parts.next().ok_or(Errno::EINVAL)?)?;
            let mut out_iface = None;
            let mut in_iface = None;
            for part in parts {
                if let Some(name) = part.strip_prefix("out=") {
                    out_iface = Some(resolve_iface_name(netns, name)?);
                    continue;
                }
                if let Some(name) = part.strip_prefix("in=") {
                    in_iface = Some(resolve_iface_name(netns, name)?);
                    continue;
                }
                {
                    return Err(Errno::EINVAL);
                }
            }
            add_netfilter_rule_in_namespace_for_test_or_bootstrap(
                netns,
                NetfilterRule {
                    table: NetfilterTable::Filter,
                    hook,
                    protocol: None,
                    src: None,
                    dst: None,
                    dst_port: None,
                    in_iface,
                    out_iface,
                    target,
                    compat_target: None,
                    to_addr: None,
                    to_port: None,
                },
            )
        }
        _ => Err(Errno::EINVAL),
    }
}

pub fn apply_postrouting_nat_ipv4(
    ctx: NetfilterFrameContext,
    packet: &[u8],
    masquerade_src: Ipv4Address,
) -> Option<Vec<u8>> {
    let netns = initial_net_namespace_payload();
    apply_postrouting_nat_ipv4_in_namespace(&netns, ctx, packet, masquerade_src)
}

pub fn apply_postrouting_nat_ipv4_in_namespace(
    netns: &NetNamespacePayload,
    ctx: NetfilterFrameContext,
    packet: &[u8],
    masquerade_src: Ipv4Address,
) -> Option<Vec<u8>> {
    let ipv4 = Ipv4Packet::new_checked(packet).ok()?;
    let src = from_smoltcp_ipv4(ipv4.src_addr());
    let dst = from_smoltcp_ipv4(ipv4.dst_addr());
    let tuple = l4_tuple(ipv4.next_header(), src, dst, ipv4.payload())?;
    if let Some(entry) = find_dnat_reply_in_namespace(netns, src, dst, tuple) {
        return rewrite_ipv4_nat(
            packet,
            Some(entry.public_dst),
            None,
            Some(entry.public_dst_port),
            None,
        );
    }
    let matched = {
        let mut state = netns.netfilter_state().lock();
        if let Some(entry) = state.rules.iter_mut().find(|entry| {
            let rule = entry.rule;
            rule.table == NetfilterTable::Nat
                && rule.hook == NetfilterHook::Postrouting
                && rule.target == NetfilterTarget::Masquerade
                && rule_matches_context(rule, ctx)
                && rule_matches_l4(rule, tuple)
                && rule.src.map_or(true, |cidr| ipv4_in_cidr(src, cidr))
                && rule.dst.map_or(true, |cidr| ipv4_in_cidr(dst, cidr))
                && rule.dst_port.map_or(true, |port| tuple.dst_port == port)
        }) {
            entry.counters.packets = entry.counters.packets.saturating_add(1);
            entry.counters.bytes = entry.counters.bytes.saturating_add(packet.len() as u64);
            true
        } else {
            false
        }
    };
    if !matched {
        return None;
    }

    remember_masquerade_in_namespace(
        netns,
        MasqueradeConntrack {
            protocol: tuple.protocol,
            original_src: src,
            original_src_port: tuple.src_port,
            masquerade_src,
            masquerade_src_port: tuple.src_port,
            external_dst: dst,
            external_dst_port: tuple.dst_port,
        },
    );
    Some(rewrite_ipv4_nat(
        packet,
        Some(masquerade_src),
        None,
        None,
        None,
    )?)
}

pub fn apply_prerouting_nat_ipv4(ctx: NetfilterFrameContext, packet: &[u8]) -> Option<Vec<u8>> {
    let netns = initial_net_namespace_payload();
    apply_prerouting_nat_ipv4_in_namespace(&netns, ctx, packet)
}

pub fn apply_prerouting_nat_ipv4_in_namespace(
    netns: &NetNamespacePayload,
    ctx: NetfilterFrameContext,
    packet: &[u8],
) -> Option<Vec<u8>> {
    let ipv4 = Ipv4Packet::new_checked(packet).ok()?;
    let src = from_smoltcp_ipv4(ipv4.src_addr());
    let dst = from_smoltcp_ipv4(ipv4.dst_addr());
    let tuple = l4_tuple(ipv4.next_header(), src, dst, ipv4.payload())?;

    if let Some(rule) = find_dnat_rule_in_namespace(netns, ctx, src, dst, tuple, packet.len()) {
        let private_dst = rule.to_addr?;
        let private_port = rule.to_port.unwrap_or(tuple.dst_port);
        remember_dnat_in_namespace(
            netns,
            DnatConntrack {
                protocol: tuple.protocol,
                client_src: src,
                client_src_port: tuple.src_port,
                public_dst: dst,
                public_dst_port: tuple.dst_port,
                private_dst,
                private_dst_port: private_port,
            },
        );
        return rewrite_ipv4_nat(packet, None, Some(private_dst), None, Some(private_port));
    }

    let entry = netns
        .netfilter_state()
        .lock()
        .masquerade_conntrack
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
    if let Some(in_iface) = rule.in_iface {
        if ctx.ingress != Some(in_iface) {
            return false;
        }
    }
    if let Some(out_iface) = rule.out_iface {
        if ctx.egress != Some(out_iface) {
            return false;
        }
    }
    true
}

fn rule_matches_l4(rule: NetfilterRule, tuple: L4Tuple) -> bool {
    if let Some(protocol) = rule.protocol {
        if protocol != tuple.protocol {
            return false;
        }
    }
    true
}

fn find_dnat_rule_in_namespace(
    netns: &NetNamespacePayload,
    ctx: NetfilterFrameContext,
    src: Ipv4Address,
    dst: Ipv4Address,
    tuple: L4Tuple,
    packet_len: usize,
) -> Option<NetfilterRule> {
    let mut state = netns.netfilter_state().lock();
    state.rules.iter_mut().find_map(|entry| {
        let rule = entry.rule;
        if rule.table == NetfilterTable::Nat
            && rule.hook == NetfilterHook::Prerouting
            && rule.target == NetfilterTarget::Dnat
            && rule_matches_context(rule, ctx)
            && rule_matches_l4(rule, tuple)
            && rule.src.map_or(true, |cidr| ipv4_in_cidr(src, cidr))
            && rule.dst.map_or(true, |cidr| ipv4_in_cidr(dst, cidr))
            && rule.dst_port.map_or(true, |port| tuple.dst_port == port)
        {
            entry.counters.packets = entry.counters.packets.saturating_add(1);
            entry.counters.bytes = entry.counters.bytes.saturating_add(packet_len as u64);
            Some(rule)
        } else {
            None
        }
    })
}

fn remember_masquerade_in_namespace(netns: &NetNamespacePayload, entry: MasqueradeConntrack) {
    let mut state = netns.netfilter_state().lock();
    if let Some(existing) = state.masquerade_conntrack.iter_mut().find(|existing| {
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
    state.masquerade_conntrack.push(entry);
}

fn remember_dnat_in_namespace(netns: &NetNamespacePayload, entry: DnatConntrack) {
    let mut state = netns.netfilter_state().lock();
    if let Some(existing) = state.dnat_conntrack.iter_mut().find(|existing| {
        existing.protocol == entry.protocol
            && existing.client_src == entry.client_src
            && existing.client_src_port == entry.client_src_port
            && existing.public_dst == entry.public_dst
            && existing.public_dst_port == entry.public_dst_port
            && existing.private_dst == entry.private_dst
            && existing.private_dst_port == entry.private_dst_port
    }) {
        *existing = entry;
        return;
    }
    state.dnat_conntrack.push(entry);
}

fn find_dnat_reply_in_namespace(
    netns: &NetNamespacePayload,
    src: Ipv4Address,
    dst: Ipv4Address,
    tuple: L4Tuple,
) -> Option<DnatConntrack> {
    netns
        .netfilter_state()
        .lock()
        .dnat_conntrack
        .iter()
        .copied()
        .find(|entry| {
            entry.protocol == tuple.protocol
                && entry.private_dst == src
                && entry.private_dst_port == tuple.src_port
                && entry.client_src == dst
                && entry.client_src_port == tuple.dst_port
        })
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
        "DNAT" | "dnat" => Ok(NetfilterTarget::Dnat),
        "MASQUERADE" | "masquerade" => Ok(NetfilterTarget::Masquerade),
        _ => Err(Errno::EINVAL),
    }
}

fn parse_protocol(name: &str) -> Result<NetfilterConntrackProtocol, Errno> {
    match name {
        "icmp" | "ICMP" => Ok(NetfilterConntrackProtocol::Icmp),
        "tcp" | "TCP" => Ok(NetfilterConntrackProtocol::Tcp),
        "udp" | "UDP" => Ok(NetfilterConntrackProtocol::Udp),
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

fn parse_u16(input: &str) -> Result<u16, Errno> {
    let value = parse_usize(input)?;
    if value > u16::MAX as usize {
        return Err(Errno::EINVAL);
    }
    Ok(value as u16)
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
