use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress as SmoltcpEthernetAddress, EthernetFrame,
    EthernetProtocol, EthernetRepr, Ipv4Address as SmoltcpIpv4Address, Ipv4Packet,
};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::device::{EthernetAddress, NetDeviceRegistration};
use crate::net::packet::{
    demux_rx_frame_with_smoltcp, PacketDispatch, PacketSource, PacketTxReadiness, PacketTxResult,
    PacketTxSink, RxFrame,
};
use crate::net::structure::{Ipv4Address, Ipv6Address};
use crate::sync::SpinMutex;

use super::{build_icmpv4_echo_reply, Icmpv4Event, IfaceCommon};
mod l3;
mod link;

pub const ARP_CACHE_TTL: Duration = Duration::from_secs(300);
pub const ARP_REQUEST_RETRY_LIMIT: u8 = 3;
pub const ARP_REQUEST_RETRY_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ipv4RouteDecision {
    Direct { next_hop: Ipv4Address },
    Gateway { next_hop: Ipv4Address },
    Broadcast { next_hop: Ipv4Address },
    Unreachable { dst: Ipv4Address },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpEntry {
    pub mac: EthernetAddress,
    pub expires_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpPendingEntry {
    pub ip: Ipv4Address,
    pub attempts: u8,
    pub next_probe_at: Instant,
    pub last_error: Option<Errno>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NdiscEntry {
    pub mac: EthernetAddress,
    pub expires_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArpResolution {
    Resolved { mac: EthernetAddress },
    Pending { next_hop: Ipv4Address },
    Failed { next_hop: Ipv4Address, errno: Errno },
}

#[derive(Debug, Default)]
pub struct ArpStats {
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    pub resolved: AtomicU64,
    pub requests_tx: AtomicU64,
    pub replies_tx: AtomicU64,
    pub retry_limit_exceeded: AtomicU64,
}

#[derive(Debug, Default)]
pub struct NetStats {
    pub rx_packets: AtomicU64,
    pub tx_packets: AtomicU64,
    pub rx_bytes: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub rx_errors: AtomicU64,
    pub tx_errors: AtomicU64,
    pub rx_dropped: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArpSnapshotState {
    Resolved,
    Pending,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArpSnapshotEntry {
    pub iface_name: &'static str,
    pub ip: Ipv4Address,
    pub mac: Option<EthernetAddress>,
    pub expires_at: Option<Instant>,
    pub state: ArpSnapshotState,
    pub attempts: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NdiscSnapshotEntry {
    pub iface_name: &'static str,
    pub ip: Ipv6Address,
    pub mac: Option<EthernetAddress>,
    pub expires_at: Option<Instant>,
    pub state: ArpSnapshotState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetStatsSnapshot {
    pub iface_name: &'static str,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
}

pub struct EtherIface {
    pub netdev: &'static NetDeviceRegistration,
    pub common: IfaceCommon,
    pub ether_addr: EthernetAddress,
    arp_table: SpinMutex<BTreeMap<Ipv4Address, ArpEntry>>,
    pending_arp: SpinMutex<BTreeMap<Ipv4Address, ArpPendingEntry>>,
    ndisc_table: SpinMutex<BTreeMap<Ipv6Address, NdiscEntry>>,
    ipv4_fragments: SpinMutex<BTreeMap<Ipv4FragmentKey, Ipv4ReassemblyEntry>>,
    next_ipv4_ident: AtomicU64,
    pub name: &'static str,
    pub stats: NetStats,
    pub arp_stats: ArpStats,
}

pub struct EtherPacketSource<'a> {
    pub iface: &'a EtherIface,
}

pub struct EtherPacketTxSink<'a> {
    pub iface: &'a EtherIface,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Ipv4FragmentKey {
    src: Ipv4Address,
    dst: Ipv4Address,
    ident: u16,
    protocol: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ipv4PacketMeta {
    src: Ipv4Address,
    dst: Ipv4Address,
    ident: u16,
    protocol: u8,
    header_len: usize,
    total_len: usize,
    flags_fragment: u16,
    fragment_offset: usize,
    more_fragments: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Ipv4FragmentRange {
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct Ipv4ReassemblyEntry {
    header: Option<Vec<u8>>,
    payload: Vec<u8>,
    ranges: Vec<Ipv4FragmentRange>,
    total_payload_len: Option<usize>,
    /// R3b: wall-clock stamp of this flow's most recent fragment. Refreshed on
    /// every insert/hit; drives LRU eviction + TTL expiry so a forged-source
    /// fragment flood cannot wipe legitimate in-flight reassemblies.
    last_seen: Instant,
}

impl Default for Ipv4ReassemblyEntry {
    fn default() -> Self {
        Self {
            header: None,
            payload: Vec::new(),
            ranges: Vec::new(),
            total_payload_len: None,
            last_seen: Instant::ZERO,
        }
    }
}

enum Ipv4IngressPacket<'a> {
    Borrowed(&'a [u8]),
    Owned(Vec<u8>),
}

enum Ipv4IngressOutcome<'a> {
    Complete(Ipv4IngressPacket<'a>),
    Pending,
    Malformed,
}

const IPV4_MIN_HEADER_LEN: usize = 20;
const IPV4_MAX_PACKET_LEN: usize = 65_535;
const IPV4_FLAG_RESERVED: u16 = 0x8000;
const IPV4_FLAG_DONT_FRAGMENT: u16 = 0x4000;
const IPV4_FLAG_MORE_FRAGMENTS: u16 = 0x2000;
const IPV4_FRAGMENT_OFFSET_MASK: u16 = 0x1fff;
const IPV4_REASSEMBLY_FLOW_LIMIT: usize = 64;
/// R3b: abandon a partial reassembly this long after its last fragment. Matches
/// Linux `net.ipv4.ipfrag_time` (30 s); `now` is the P0-unfrozen wall clock.
const IPV4_REASSEMBLY_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArpFlushOutcome {
    pub attempted: usize,
    pub sent: usize,
    pub busy: usize,
    pub failed: usize,
    pub tx_bytes: usize,
    pub remaining: usize,
}

impl EtherIface {
    pub fn new(
        netdev: &'static NetDeviceRegistration,
        common: IfaceCommon,
        ether_addr: EthernetAddress,
        name: &'static str,
    ) -> Self {
        Self {
            netdev,
            common,
            ether_addr,
            arp_table: SpinMutex::new(BTreeMap::new()),
            pending_arp: SpinMutex::new(BTreeMap::new()),
            ndisc_table: SpinMutex::new(BTreeMap::new()),
            ipv4_fragments: SpinMutex::new(BTreeMap::new()),
            next_ipv4_ident: AtomicU64::new(1),
            name,
            stats: NetStats::new(),
            arp_stats: ArpStats::new(),
        }
    }

    pub fn process_frame_at(
        &self,
        frame: RxFrame,
        now: Instant,
        guard: Option<&Guard<'_>>,
    ) -> PacketDispatch {
        self.stats
            .rx_bytes
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        self.stats.rx_packets.fetch_add(1, Ordering::Relaxed);

        let ethernet = match EthernetFrame::new_checked(frame.as_bytes()) {
            Ok(ethernet) => ethernet,
            Err(_) => {
                self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketDispatch::Malformed;
            }
        };

        if !self.accepts_ethernet_destination(ethernet.dst_addr()) {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return PacketDispatch::Unsupported;
        }

        match ethernet.ethertype() {
            EthernetProtocol::Ipv4 => {
                let packet = match self.prepare_ipv4_ingress(ethernet.payload()) {
                    Ipv4IngressOutcome::Complete(packet) => packet,
                    Ipv4IngressOutcome::Pending => return PacketDispatch::Unsupported,
                    Ipv4IngressOutcome::Malformed => {
                        self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                        return PacketDispatch::Malformed;
                    }
                };
                let frame = match packet.as_slice() {
                    Some(packet) => build_ipv4_ethernet_frame(
                        from_smoltcp_ether(ethernet.src_addr()),
                        from_smoltcp_ether(ethernet.dst_addr()),
                        packet,
                    ),
                    None => {
                        self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                        return PacketDispatch::Malformed;
                    }
                };
                let dispatch = demux_rx_frame_with_smoltcp(&RxFrame::new(frame));
                self.maybe_reply_icmpv4(&dispatch, now, guard);
                dispatch
            }
            EthernetProtocol::Arp => self.process_arp(ethernet.payload(), now, guard),
            // P2-S7 (§6-2-A): v6 TCP/UDP frames go through the same demux
            // (its v6 arm parses them); no v4-style reassembly staging yet
            // (v6 fragmentation is a P4 concern).
            EthernetProtocol::Ipv6 => demux_rx_frame_with_smoltcp(&frame),
            EthernetProtocol::Unknown(_) => PacketDispatch::Unsupported,
        }
    }

    pub fn dispatch_ip_at(&self, packet: &[u8], now: Instant, guard: &Guard<'_>) -> PacketTxResult {
        if packet.len() > IPV4_MAX_PACKET_LEN {
            self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
            return PacketTxResult::Failed {
                errno: Errno::EINVAL,
            };
        }

        let ipv4 = match Ipv4Packet::new_checked(packet) {
            Ok(ipv4) => ipv4,
            Err(_) => {
                self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketTxResult::Failed {
                    errno: Errno::EINVAL,
                };
            }
        };
        let route = decide_ipv4_route(self.common, from_smoltcp_ipv4(ipv4.dst_addr()));
        let next_hop = match route.next_hop() {
            Some(next_hop) => next_hop,
            None => {
                self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketTxResult::Failed {
                    errno: Errno::EADDRNOTAVAIL,
                };
            }
        };

        let dst_mac = match self.resolve_or_request(next_hop, now) {
            ArpResolution::Resolved { mac } => mac,
            ArpResolution::Pending { next_hop } => {
                return PacketTxResult::PendingResolution { next_hop };
            }
            ArpResolution::Failed { errno, .. } => {
                self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketTxResult::Failed { errno };
            }
        };

        self.transmit_ipv4_packet(dst_mac, packet, guard)
    }

    pub fn flush_pending_arp_at(
        &self,
        now: Instant,
        budget: usize,
        guard: &Guard<'_>,
    ) -> ArpFlushOutcome {
        let mut outcome = ArpFlushOutcome::default();

        let ready = self.ready_pending_arp(now, budget);
        for target_ip in ready {
            let Some(entry) = self.pending_entry_for_probe(target_ip, now) else {
                if self
                    .pending_arp_entry(target_ip)
                    .is_some_and(|entry| entry.last_error.is_some())
                {
                    outcome.failed += 1;
                }
                continue;
            };
            outcome.attempted += 1;

            let frame = self.build_arp_request(entry.ip);
            match self.transmit_frame(&frame, guard) {
                PacketTxResult::Accepted { frame_len } => {
                    outcome.sent += 1;
                    outcome.tx_bytes += frame_len;
                    self.mark_arp_probe_sent(target_ip, now);
                    self.arp_stats.requests_tx.fetch_add(1, Ordering::Relaxed);
                }
                PacketTxResult::Busy => {
                    outcome.busy += 1;
                    break;
                }
                PacketTxResult::PendingResolution { .. } => {
                    outcome.failed += 1;
                }
                PacketTxResult::Failed { .. } => {
                    outcome.failed += 1;
                }
            }
        }

        outcome.remaining = self.pending_arp.lock().len();
        outcome
    }

    pub fn arp_entry(&self, ip: Ipv4Address, now: Instant) -> Option<ArpEntry> {
        self.lookup_arp_entry(ip, now)
    }

    pub fn install_arp_for_test_or_bootstrap(
        &self,
        ip: Ipv4Address,
        mac: EthernetAddress,
        expires_at: Instant,
    ) {
        self.arp_table
            .lock()
            .insert(ip, ArpEntry { mac, expires_at });
        self.pending_arp.lock().remove(&ip);
    }

    pub fn install_static_arp(&self, ip: Ipv4Address, mac: EthernetAddress) {
        self.arp_table.lock().insert(
            ip,
            ArpEntry {
                mac,
                expires_at: Instant::from_secs(10 * 365 * 24 * 60 * 60),
            },
        );
        self.pending_arp.lock().remove(&ip);
    }

    pub fn remove_static_arp(&self, ip: Ipv4Address) -> bool {
        self.pending_arp.lock().remove(&ip);
        self.arp_table.lock().remove(&ip).is_some()
    }

    pub fn install_static_ndisc(&self, ip: Ipv6Address, mac: EthernetAddress) {
        self.ndisc_table.lock().insert(
            ip,
            NdiscEntry {
                mac,
                expires_at: Instant::from_secs(10 * 365 * 24 * 60 * 60),
            },
        );
    }

    pub fn remove_static_ndisc(&self, ip: Ipv6Address) -> bool {
        self.ndisc_table.lock().remove(&ip).is_some()
    }

    pub fn copy_arp_cache_from(&self, other: &EtherIface) {
        let arp_entries = other.arp_table.lock().clone();
        let pending_entries = other.pending_arp.lock().clone();
        let ndisc_entries = other.ndisc_table.lock().clone();
        self.arp_table.lock().extend(arp_entries);
        self.pending_arp.lock().extend(pending_entries);
        self.ndisc_table.lock().extend(ndisc_entries);
    }

    pub fn pending_arp_len(&self) -> usize {
        self.pending_arp.lock().len()
    }

    pub fn pending_arp_entry(&self, ip: Ipv4Address) -> Option<ArpPendingEntry> {
        self.pending_arp.lock().get(&ip).copied()
    }

    pub fn arp_snapshot(&self, now: Instant) -> Vec<ArpSnapshotEntry> {
        let mut entries = Vec::new();

        for (ip, entry) in self.arp_table.lock().iter() {
            if entry.expires_at > now {
                entries.push(ArpSnapshotEntry {
                    iface_name: self.name,
                    ip: *ip,
                    mac: Some(entry.mac),
                    expires_at: Some(entry.expires_at),
                    state: ArpSnapshotState::Resolved,
                    attempts: 0,
                });
            }
        }

        for (ip, pending) in self.pending_arp.lock().iter() {
            entries.push(ArpSnapshotEntry {
                iface_name: self.name,
                ip: *ip,
                mac: None,
                expires_at: None,
                state: if pending.last_error.is_some() {
                    ArpSnapshotState::Failed
                } else {
                    ArpSnapshotState::Pending
                },
                attempts: pending.attempts,
            });
        }

        entries.sort_by_key(|entry| (entry.ip, entry.state.sort_key()));
        entries
    }

    pub fn ndisc_snapshot(&self, now: Instant) -> Vec<NdiscSnapshotEntry> {
        let mut entries = Vec::new();

        for (ip, entry) in self.ndisc_table.lock().iter() {
            if entry.expires_at > now {
                entries.push(NdiscSnapshotEntry {
                    iface_name: self.name,
                    ip: *ip,
                    mac: Some(entry.mac),
                    expires_at: Some(entry.expires_at),
                    state: ArpSnapshotState::Resolved,
                });
            }
        }

        entries.sort_by_key(|entry| entry.ip);
        entries
    }

    pub fn net_stats_snapshot(&self) -> NetStatsSnapshot {
        NetStatsSnapshot {
            iface_name: self.name,
            rx_packets: self.stats.rx_packets.load(Ordering::Relaxed),
            tx_packets: self.stats.tx_packets.load(Ordering::Relaxed),
            rx_bytes: self.stats.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: self.stats.tx_bytes.load(Ordering::Relaxed),
            rx_errors: self.stats.rx_errors.load(Ordering::Relaxed),
            tx_errors: self.stats.tx_errors.load(Ordering::Relaxed),
            rx_dropped: self.stats.rx_dropped.load(Ordering::Relaxed),
        }
    }

    pub fn clear_for_test_or_bootstrap(&self) {
        self.arp_table.lock().clear();
        self.pending_arp.lock().clear();
        self.ipv4_fragments.lock().clear();
    }

    pub fn accepts_ethernet_destination_addr(&self, dst: EthernetAddress) -> bool {
        self.accepts_ethernet_destination(to_smoltcp_ether(dst))
    }

    pub fn accepts_ipv4_destination_addr(&self, dst: Ipv4Address) -> bool {
        self.accepts_ipv4_destination(dst)
    }

    fn maybe_reply_icmpv4(
        &self,
        dispatch: &PacketDispatch,
        now: Instant,
        guard: Option<&Guard<'_>>,
    ) {
        let PacketDispatch::Icmp(Icmpv4Event::EchoRequest(request)) = dispatch else {
            return;
        };
        if !self.accepts_ipv4_destination(request.dst) {
            self.stats.rx_dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(guard) = guard else {
            return;
        };
        let reply = build_icmpv4_echo_reply(&request.reply_packet());
        match self.dispatch_ip_at(reply.as_bytes(), now, guard) {
            PacketTxResult::Accepted { .. } => {}
            PacketTxResult::Busy | PacketTxResult::PendingResolution { .. } => {}
            PacketTxResult::Failed { .. } => {}
        }
    }

    fn transmit_ipv4_packet(
        &self,
        dst_mac: EthernetAddress,
        packet: &[u8],
        guard: &Guard<'_>,
    ) -> PacketTxResult {
        if packet.len() <= usize::from(self.common.mtu()) {
            let frame = build_ipv4_ethernet_frame(self.ether_addr, dst_mac, packet);
            return self.transmit_frame(&frame, guard);
        }
        self.transmit_ipv4_fragments(dst_mac, packet, guard)
    }
}

impl Ipv4RouteDecision {
    pub const fn next_hop(self) -> Option<Ipv4Address> {
        match self {
            Self::Direct { next_hop }
            | Self::Gateway { next_hop }
            | Self::Broadcast { next_hop } => Some(next_hop),
            Self::Unreachable { .. } => None,
        }
    }
}

impl NetStats {
    pub const fn new() -> Self {
        Self {
            rx_packets: AtomicU64::new(0),
            tx_packets: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_errors: AtomicU64::new(0),
            tx_errors: AtomicU64::new(0),
            rx_dropped: AtomicU64::new(0),
        }
    }
}

impl ArpStats {
    pub const fn new() -> Self {
        Self {
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            resolved: AtomicU64::new(0),
            requests_tx: AtomicU64::new(0),
            replies_tx: AtomicU64::new(0),
            retry_limit_exceeded: AtomicU64::new(0),
        }
    }
}

impl ArpSnapshotState {
    const fn sort_key(self) -> u8 {
        match self {
            Self::Resolved => 0,
            Self::Pending => 1,
            Self::Failed => 2,
        }
    }
}

impl PacketSource for EtherPacketSource<'_> {
    fn next_packet(&self) -> Option<PacketDispatch> {
        let frame = self.iface.netdev.ops.receive()?;
        Some(
            self.iface
                .process_frame_at(frame, Instant::ZERO, Option::<&Guard<'_>>::None),
        )
    }

    fn next_packet_at(&self, now: Instant, guard: &Guard<'_>) -> Option<PacketDispatch> {
        let frame = self.iface.netdev.ops.receive()?;
        Some(self.iface.process_frame_at(frame, now, Some(guard)))
    }
}

impl PacketTxSink for EtherPacketTxSink<'_> {
    fn readiness(&self, guard: &Guard<'_>) -> PacketTxReadiness {
        self.iface.netdev.ops.tx_readiness(guard)
    }

    fn readiness_at(&self, now: Instant, guard: &Guard<'_>) -> PacketTxReadiness {
        let _now = now;
        self.readiness(guard)
    }

    fn source_ipv4(&self) -> Option<Ipv4Address> {
        Some(self.iface.common.ipv4_addr())
    }

    fn transmit(&self, frame: &[u8], guard: &Guard<'_>) -> PacketTxResult {
        self.iface.dispatch_ip_at(frame, Instant::ZERO, guard)
    }

    fn transmit_at(&self, frame: &[u8], now: Instant, guard: &Guard<'_>) -> PacketTxResult {
        self.iface.dispatch_ip_at(frame, now, guard)
    }
}

fn build_ipv4_ethernet_frame(
    src: EthernetAddress,
    dst: EthernetAddress,
    ipv4_packet: &[u8],
) -> Vec<u8> {
    let repr = EthernetRepr {
        src_addr: to_smoltcp_ether(src),
        dst_addr: to_smoltcp_ether(dst),
        ethertype: EthernetProtocol::Ipv4,
    };
    let mut frame = vec![0; repr.buffer_len() + ipv4_packet.len()];
    let mut ethernet = EthernetFrame::new_unchecked(frame.as_mut_slice());
    repr.emit(&mut ethernet);
    ethernet.payload_mut().copy_from_slice(ipv4_packet);
    frame
}

fn build_arp_frame(repr: &ArpRepr) -> Vec<u8> {
    let (src_addr, dst_addr) = match repr {
        ArpRepr::EthernetIpv4 {
            source_hardware_addr,
            target_hardware_addr,
            ..
        } => (*source_hardware_addr, *target_hardware_addr),
        _ => return Vec::new(),
    };
    let ether = EthernetRepr {
        src_addr,
        dst_addr,
        ethertype: EthernetProtocol::Arp,
    };
    let mut frame = vec![0; ether.buffer_len() + repr.buffer_len()];
    let mut ethernet = EthernetFrame::new_unchecked(frame.as_mut_slice());
    ether.emit(&mut ethernet);
    let mut arp = ArpPacket::new_unchecked(ethernet.payload_mut());
    repr.emit(&mut arp);
    frame
}

impl Ipv4PacketMeta {
    const fn is_fragmented(self) -> bool {
        self.more_fragments || self.fragment_offset != 0
    }

    const fn payload_len(self) -> usize {
        self.total_len - self.header_len
    }

    const fn fragment_end(self) -> usize {
        self.fragment_offset + self.payload_len()
    }
}

impl Ipv4IngressPacket<'_> {
    fn as_slice(&self) -> Option<&[u8]> {
        match self {
            Self::Borrowed(bytes) => Some(bytes),
            Self::Owned(bytes) => Some(bytes.as_slice()),
        }
    }
}

impl Ipv4ReassemblyEntry {
    fn record_range(&mut self, start: usize, end: usize) {
        if start == end {
            return;
        }
        self.ranges.push(Ipv4FragmentRange { start, end });
        self.ranges.sort_by_key(|range| range.start);

        let mut merged: Vec<Ipv4FragmentRange> = Vec::new();
        for range in self.ranges.drain(..) {
            if let Some(last) = merged.last_mut() {
                if range.start <= last.end {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        self.ranges = merged;
    }

    fn is_complete(&self) -> bool {
        if self.header.is_none() {
            return false;
        }
        let Some(total) = self.total_payload_len else {
            return false;
        };

        let mut covered = 0usize;
        for range in &self.ranges {
            if range.start > covered {
                return false;
            }
            covered = covered.max(range.end);
            if covered >= total {
                return true;
            }
        }
        false
    }
}

fn parse_ipv4_meta(packet: &[u8]) -> Option<Ipv4PacketMeta> {
    if packet.len() < IPV4_MIN_HEADER_LEN || packet.len() > IPV4_MAX_PACKET_LEN {
        return None;
    }
    if packet[0] >> 4 != 4 {
        return None;
    }
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if header_len < IPV4_MIN_HEADER_LEN || packet.len() < header_len {
        return None;
    }
    let total_len = usize::from(read_u16(packet, 2));
    if total_len < header_len || total_len > packet.len() {
        return None;
    }
    let flags_fragment = read_u16(packet, 6);
    let fragment_offset = usize::from(flags_fragment & IPV4_FRAGMENT_OFFSET_MASK) * 8;
    let more_fragments = flags_fragment & IPV4_FLAG_MORE_FRAGMENTS != 0;
    let payload_len = total_len - header_len;
    if more_fragments && payload_len % 8 != 0 {
        return None;
    }
    let fragment_end = fragment_offset.checked_add(payload_len)?;
    if fragment_end > IPV4_MAX_PACKET_LEN {
        return None;
    }

    Some(Ipv4PacketMeta {
        src: Ipv4Address::new([packet[12], packet[13], packet[14], packet[15]]),
        dst: Ipv4Address::new([packet[16], packet[17], packet[18], packet[19]]),
        ident: read_u16(packet, 4),
        protocol: packet[9],
        header_len,
        total_len,
        flags_fragment,
        fragment_offset,
        more_fragments,
    })
}

/// R3b: keep the reassembly table bounded. First drop every flow whose
/// `last_seen + TTL` has passed, then — only when `incoming` is a genuinely new
/// flow and the table is still at capacity — evict the single least-recently-seen
/// flow. This replaces the previous wholesale `clear()`, which let 65 forged
/// first-fragments wipe all 64 legitimate in-flight reassemblies (a low-severity
/// DoS). Mirrors the P3-C conntrack `expire_and_cap_*` pattern; `now` is the
/// P0-unfrozen `net_now_instant()`.
fn expire_and_cap_ipv4_fragments(
    fragments: &mut BTreeMap<Ipv4FragmentKey, Ipv4ReassemblyEntry>,
    incoming: &Ipv4FragmentKey,
    now: Instant,
) {
    fragments.retain(|_, entry| entry.last_seen + IPV4_REASSEMBLY_TTL > now);
    if fragments.contains_key(incoming) {
        return;
    }
    while fragments.len() >= IPV4_REASSEMBLY_FLOW_LIMIT {
        let Some(oldest) = fragments
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen.total_micros())
            .map(|(key, _)| *key)
        else {
            break;
        };
        fragments.remove(&oldest);
    }
}

fn assemble_ipv4_packet(entry: Ipv4ReassemblyEntry) -> Option<Vec<u8>> {
    let mut packet = entry.header?;
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    let total_payload_len = entry.total_payload_len?;
    let total_len = header_len.checked_add(total_payload_len)?;
    if total_len > IPV4_MAX_PACKET_LEN || entry.payload.len() < total_payload_len {
        return None;
    }
    packet.resize(total_len, 0);
    packet[header_len..].copy_from_slice(&entry.payload[..total_payload_len]);
    write_u16(&mut packet, 2, total_len as u16);
    write_u16(&mut packet, 6, 0);
    fill_ipv4_header_checksum(&mut packet);
    Some(packet)
}

fn fill_ipv4_header_checksum(packet: &mut [u8]) {
    let header_len = usize::from(packet[0] & 0x0f) * 4;
    if packet.len() < header_len || header_len < IPV4_MIN_HEADER_LEN {
        return;
    }
    packet[10] = 0;
    packet[11] = 0;
    let checksum = ipv4_header_checksum(&packet[..header_len]);
    write_u16(packet, 10, checksum);
}

fn ipv4_header_checksum(header: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut chunks = header.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u32::from(u16::from_be_bytes([chunk[0], chunk[1]]));
    }
    if let Some(&last) = chunks.remainder().first() {
        sum += u32::from(last) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([bytes[offset], bytes[offset + 1]])
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    let [hi, lo] = value.to_be_bytes();
    bytes[offset] = hi;
    bytes[offset + 1] = lo;
}

fn to_smoltcp_ether(addr: EthernetAddress) -> SmoltcpEthernetAddress {
    SmoltcpEthernetAddress(addr.octets())
}

fn from_smoltcp_ether(addr: SmoltcpEthernetAddress) -> EthernetAddress {
    EthernetAddress::new(addr.0)
}

fn to_smoltcp_ipv4(addr: Ipv4Address) -> SmoltcpIpv4Address {
    let [a, b, c, d] = addr.octets();
    SmoltcpIpv4Address::new(a, b, c, d)
}

fn from_smoltcp_ipv4(addr: SmoltcpIpv4Address) -> Ipv4Address {
    Ipv4Address::new(addr.octets())
}

pub fn decide_ipv4_route(common: IfaceCommon, dst: Ipv4Address) -> Ipv4RouteDecision {
    if dst == Ipv4Address::BROADCAST {
        Ipv4RouteDecision::Broadcast { next_hop: dst }
    } else if same_ipv4_subnet(common, dst) {
        Ipv4RouteDecision::Direct { next_hop: dst }
    } else if let Some(next_hop) = common.gateway() {
        Ipv4RouteDecision::Gateway { next_hop }
    } else {
        Ipv4RouteDecision::Unreachable { dst }
    }
}

fn same_ipv4_subnet(common: IfaceCommon, dst: Ipv4Address) -> bool {
    let local = ipv4_to_u32(common.ipv4_addr());
    let dst = ipv4_to_u32(dst);
    let mask = ipv4_to_u32(common.netmask());
    (local & mask) == (dst & mask)
}

fn ipv4_to_u32(addr: Ipv4Address) -> u32 {
    u32::from_be_bytes(addr.octets())
}

#[cfg(test)]
mod fragment_reassembly_bound_tests {
    use super::*;

    fn frag_key(ident: u16) -> Ipv4FragmentKey {
        Ipv4FragmentKey {
            src: Ipv4Address::new([10, 0, 0, 1]),
            dst: Ipv4Address::new([10, 0, 0, 2]),
            ident,
            protocol: 17,
        }
    }

    fn entry_at(micros: i64) -> Ipv4ReassemblyEntry {
        Ipv4ReassemblyEntry {
            last_seen: Instant::from_micros(micros),
            ..Ipv4ReassemblyEntry::default()
        }
    }

    // R3b: a full table + a brand-new flow must evict exactly the single
    // least-recently-seen flow — NOT clear the whole table. The old code did
    // `fragments.clear()`, so 65 forged first-fragments wiped all 64 legitimate
    // in-flight reassemblies (a low-severity DoS).
    #[test]
    fn overflow_evicts_only_oldest_flow_not_whole_table() {
        let mut fragments = BTreeMap::new();
        for i in 0..IPV4_REASSEMBLY_FLOW_LIMIT {
            // Distinct, increasing last_seen so "oldest" is deterministic.
            fragments.insert(frag_key(i as u16), entry_at(1_000 + i as i64));
        }
        assert_eq!(fragments.len(), IPV4_REASSEMBLY_FLOW_LIMIT);

        let incoming = frag_key(IPV4_REASSEMBLY_FLOW_LIMIT as u16);
        let now = Instant::from_micros(1_000 + IPV4_REASSEMBLY_FLOW_LIMIT as i64);
        expire_and_cap_ipv4_fragments(&mut fragments, &incoming, now);

        // Exactly one slot freed for the newcomer; every other flow survives.
        assert_eq!(fragments.len(), IPV4_REASSEMBLY_FLOW_LIMIT - 1);
        assert!(
            !fragments.contains_key(&frag_key(0)),
            "the oldest flow must be the one evicted"
        );
        for i in 1..IPV4_REASSEMBLY_FLOW_LIMIT {
            assert!(
                fragments.contains_key(&frag_key(i as u16)),
                "legitimate flow {i} must not be evicted"
            );
        }
    }

    // R3b: flows past their TTL are swept once `now` advances, independent of
    // table pressure.
    #[test]
    fn ttl_sweep_drops_stale_flows() {
        let mut fragments = BTreeMap::new();
        fragments.insert(frag_key(1), entry_at(0));
        fragments.insert(frag_key(2), entry_at(0));

        let now = Instant::ZERO + IPV4_REASSEMBLY_TTL + Duration::from_micros(1);
        expire_and_cap_ipv4_fragments(&mut fragments, &frag_key(3), now);

        assert!(fragments.is_empty(), "flows past their TTL must be swept");
    }

    // R3b: a later fragment of an ALREADY-present flow never evicts anyone,
    // even at capacity — reassembly in progress is left untouched.
    #[test]
    fn later_fragment_of_existing_flow_never_evicts() {
        let mut fragments = BTreeMap::new();
        for i in 0..IPV4_REASSEMBLY_FLOW_LIMIT {
            fragments.insert(frag_key(i as u16), entry_at(1_000 + i as i64));
        }
        let existing = frag_key(5);
        let now = Instant::from_micros(2_000);
        expire_and_cap_ipv4_fragments(&mut fragments, &existing, now);
        assert_eq!(
            fragments.len(),
            IPV4_REASSEMBLY_FLOW_LIMIT,
            "an in-progress flow must not trigger eviction"
        );
    }
}
