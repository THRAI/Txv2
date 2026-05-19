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
use crate::net::structure::Ipv4Address;
use crate::sync::SpinMutex;

use super::{build_icmpv4_echo_reply, Icmpv4Event, IfaceCommon};

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
                let dispatch = demux_rx_frame_with_smoltcp(&frame);
                self.maybe_reply_icmpv4(&dispatch, now, guard);
                dispatch
            }
            EthernetProtocol::Arp => self.process_arp(ethernet.payload(), now, guard),
            EthernetProtocol::Ipv6 | EthernetProtocol::Unknown(_) => PacketDispatch::Unsupported,
        }
    }

    pub fn dispatch_ip_at(&self, packet: &[u8], now: Instant, guard: &Guard<'_>) -> PacketTxResult {
        if packet.len() > usize::from(self.common.mtu()) {
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

        let frame = build_ipv4_ethernet_frame(self.ether_addr, dst_mac, packet);
        self.transmit_frame(&frame, guard)
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
    }

    pub fn accepts_ethernet_destination_addr(&self, dst: EthernetAddress) -> bool {
        self.accepts_ethernet_destination(to_smoltcp_ether(dst))
    }

    pub fn accepts_ipv4_destination_addr(&self, dst: Ipv4Address) -> bool {
        self.accepts_ipv4_destination(dst)
    }

    fn process_arp(
        &self,
        payload: &[u8],
        now: Instant,
        guard: Option<&Guard<'_>>,
    ) -> PacketDispatch {
        let packet = match ArpPacket::new_checked(payload) {
            Ok(packet) => packet,
            Err(_) => {
                self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketDispatch::Malformed;
            }
        };
        let repr = match ArpRepr::parse(&packet) {
            Ok(repr) => repr,
            Err(_) => {
                self.stats.rx_errors.fetch_add(1, Ordering::Relaxed);
                return PacketDispatch::Malformed;
            }
        };

        match repr {
            ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Reply,
                source_hardware_addr,
                source_protocol_addr,
                ..
            } => {
                if source_hardware_addr.is_unicast() {
                    self.learn_arp(
                        from_smoltcp_ipv4(source_protocol_addr),
                        from_smoltcp_ether(source_hardware_addr),
                        now,
                    );
                }
                PacketDispatch::Unsupported
            }
            ArpRepr::EthernetIpv4 {
                operation: ArpOperation::Request,
                source_hardware_addr,
                source_protocol_addr,
                target_protocol_addr,
                ..
            } => {
                if source_hardware_addr.is_unicast() {
                    self.learn_arp(
                        from_smoltcp_ipv4(source_protocol_addr),
                        from_smoltcp_ether(source_hardware_addr),
                        now,
                    );
                }
                if from_smoltcp_ipv4(target_protocol_addr) == self.common.ipv4_addr() {
                    if let Some(guard) = guard {
                        let reply = self.build_arp_reply(
                            from_smoltcp_ipv4(source_protocol_addr),
                            from_smoltcp_ether(source_hardware_addr),
                        );
                        if matches!(
                            self.transmit_frame(&reply, guard),
                            PacketTxResult::Accepted { .. }
                        ) {
                            self.arp_stats.replies_tx.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                PacketDispatch::Unsupported
            }
            _ => PacketDispatch::Unsupported,
        }
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

    fn learn_arp(&self, ip: Ipv4Address, mac: EthernetAddress, now: Instant) {
        self.arp_table.lock().insert(
            ip,
            ArpEntry {
                mac,
                expires_at: now + ARP_CACHE_TTL,
            },
        );
        self.pending_arp.lock().remove(&ip);
        self.arp_stats.resolved.fetch_add(1, Ordering::Relaxed);
    }

    fn lookup_arp_entry(&self, ip: Ipv4Address, now: Instant) -> Option<ArpEntry> {
        let mut table = self.arp_table.lock();
        match table.get(&ip).copied() {
            Some(entry) if entry.expires_at > now => Some(entry),
            Some(_) => {
                table.remove(&ip);
                None
            }
            None => None,
        }
    }

    fn resolve_or_request(&self, next_hop: Ipv4Address, now: Instant) -> ArpResolution {
        if next_hop == Ipv4Address::BROADCAST {
            return ArpResolution::Resolved {
                mac: EthernetAddress::BROADCAST,
            };
        }

        if let Some(entry) = self.lookup_arp_entry(next_hop, now) {
            self.arp_stats.cache_hits.fetch_add(1, Ordering::Relaxed);
            return ArpResolution::Resolved { mac: entry.mac };
        }

        self.arp_stats.cache_misses.fetch_add(1, Ordering::Relaxed);
        self.queue_pending_arp(next_hop, now)
    }

    fn queue_pending_arp(&self, ip: Ipv4Address, now: Instant) -> ArpResolution {
        let mut pending = self.pending_arp.lock();
        match pending.get(&ip).copied() {
            Some(ArpPendingEntry {
                last_error: Some(errno),
                ..
            }) => ArpResolution::Failed {
                next_hop: ip,
                errno,
            },
            Some(_) => ArpResolution::Pending { next_hop: ip },
            None => {
                pending.insert(
                    ip,
                    ArpPendingEntry {
                        ip,
                        attempts: 0,
                        next_probe_at: now,
                        last_error: None,
                    },
                );
                ArpResolution::Pending { next_hop: ip }
            }
        }
    }

    fn ready_pending_arp(&self, now: Instant, budget: usize) -> Vec<Ipv4Address> {
        self.pending_arp
            .lock()
            .iter()
            .filter_map(|(ip, entry)| {
                (entry.last_error.is_none() && entry.next_probe_at <= now).then_some(*ip)
            })
            .take(budget)
            .collect()
    }

    fn pending_entry_for_probe(&self, ip: Ipv4Address, now: Instant) -> Option<ArpPendingEntry> {
        let mut pending = self.pending_arp.lock();
        let entry = pending.get_mut(&ip)?;
        if entry.last_error.is_some() || entry.next_probe_at > now {
            return None;
        }
        if entry.attempts >= ARP_REQUEST_RETRY_LIMIT {
            entry.last_error = Some(Errno::EADDRNOTAVAIL);
            self.arp_stats
                .retry_limit_exceeded
                .fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(*entry)
    }

    fn mark_arp_probe_sent(&self, ip: Ipv4Address, now: Instant) {
        if let Some(entry) = self.pending_arp.lock().get_mut(&ip) {
            entry.attempts = entry.attempts.saturating_add(1);
            entry.next_probe_at = now + ARP_REQUEST_RETRY_DELAY;
        }
    }

    fn accepts_ethernet_destination(&self, dst: SmoltcpEthernetAddress) -> bool {
        dst.is_broadcast() || dst == to_smoltcp_ether(self.ether_addr)
    }

    fn accepts_ipv4_destination(&self, dst: Ipv4Address) -> bool {
        dst == self.common.ipv4_addr() || dst == Ipv4Address::BROADCAST
    }

    fn build_arp_request(&self, target_ip: Ipv4Address) -> Vec<u8> {
        let repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: to_smoltcp_ether(self.ether_addr),
            source_protocol_addr: to_smoltcp_ipv4(self.common.ipv4_addr()),
            target_hardware_addr: SmoltcpEthernetAddress::BROADCAST,
            target_protocol_addr: to_smoltcp_ipv4(target_ip),
        };
        build_arp_frame(&repr)
    }

    fn build_arp_reply(&self, target_ip: Ipv4Address, target_mac: EthernetAddress) -> Vec<u8> {
        let repr = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: to_smoltcp_ether(self.ether_addr),
            source_protocol_addr: to_smoltcp_ipv4(self.common.ipv4_addr()),
            target_hardware_addr: to_smoltcp_ether(target_mac),
            target_protocol_addr: to_smoltcp_ipv4(target_ip),
        };
        build_arp_frame(&repr)
    }

    fn transmit_frame(&self, frame: &[u8], guard: &Guard<'_>) -> PacketTxResult {
        match self.netdev.ops.transmit(frame, guard) {
            StepOutcome::Done(()) | StepOutcome::Continue { .. } => {
                self.stats
                    .tx_bytes
                    .fetch_add(frame.len() as u64, Ordering::Relaxed);
                self.stats.tx_packets.fetch_add(1, Ordering::Relaxed);
                PacketTxResult::Accepted {
                    frame_len: frame.len(),
                }
            }
            StepOutcome::Yield { .. } => PacketTxResult::Busy,
            StepOutcome::Err(errno) => {
                self.stats.tx_errors.fetch_add(1, Ordering::Relaxed);
                PacketTxResult::Failed { errno }
            }
        }
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
