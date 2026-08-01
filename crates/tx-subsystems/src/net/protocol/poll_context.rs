use alloc::vec::Vec;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::TcpControl;
use tx_substrate::index::IndexError;
use tx_substrate::zone::Cap;

use crate::execution::Guard;
use crate::net::packet::{NetworkPublish, NetworkPublishTarget};
use crate::net::protocol::{
    build_icmpv4_echo_reply, build_icmpv4_echo_request, icmpv4_echo_message_len,
    parse_icmpv4_loopback_packet, Icmpv4Event, LoopbackIface, UdpRxDatagram,
};
use crate::net::structure::registry;
use crate::net::structure::table::{SocketTable, SOCKET_TABLE};
use crate::net::structure::{
    AddressFamily, ConnectionKey, IpEndpoint, Ipv4Address, RecvWireSet, SocketIdentity, SocketKind,
    SocketProtocol, TcpBacklogEntry, TcpState, UdpInner, TCP_BACKLOG_TIMEOUT_STAGING_MILLIS,
};

use super::SmoltcpTcpSegment;

pub struct PollContext {
    timestamp: Instant,
    socket_table: &'static SocketTable,
    packets_seen: usize,
    tx_packets: usize,
    sockets_touched: usize,
}

pub struct PollContextOutcome {
    pub packets_seen: usize,
    pub tx_packets: usize,
    pub sockets_touched: usize,
    pub bytes_moved: usize,
    pub publishes: Vec<NetworkPublishTarget>,
    pub created_children: Vec<Cap<SocketIdentity>>,
}

impl PollContext {
    pub fn new(timestamp: Instant) -> Self {
        Self::new_with_table(timestamp, SOCKET_TABLE.as_table())
    }

    pub const fn new_with_table(timestamp: Instant, socket_table: &'static SocketTable) -> Self {
        Self {
            timestamp,
            socket_table,
            packets_seen: 0,
            tx_packets: 0,
            sockets_touched: 0,
        }
    }

    pub const fn timestamp(&self) -> Instant {
        self.timestamp
    }

    pub const fn sockets_touched(&self) -> usize {
        self.sockets_touched
    }

    pub const fn packets_seen(&self) -> usize {
        self.packets_seen
    }

    pub fn poll_egress_one(
        &mut self,
        source: &Cap<SocketIdentity>,
        iface: &LoopbackIface,
        _guard: &Guard<'_>,
    ) -> Option<NetworkPublishTarget> {
        let source_payload = source.acquire_operational()?;
        let source_raw = source_payload.raw_tcp_socket()?;
        let packet = source_raw.dispatch_segment()?.emit_ipv4_packet()?;
        if !iface.dispatch_ip(packet) {
            return None;
        }

        self.tx_packets += 1;
        self.sockets_touched += 1;
        Some(NetworkPublishTarget::new(
            source.clone(),
            NetworkPublish::none(),
        ))
    }

    pub fn poll_udp_egress_one(
        &mut self,
        source: &Cap<SocketIdentity>,
        iface: &LoopbackIface,
        _guard: &Guard<'_>,
    ) -> Option<NetworkPublishTarget> {
        // 同 poll_udp_ingress:检活后再取 payload,防并发 close 竞态。
        let source_ident = source.downgrade().observe(_guard)?;
        let source_payload = source_ident.acquire_operational()?;
        let (local, connected_remote) = udp_endpoints(&source_payload.protocol_snapshot())?;
        let mut drain = source_payload.take_udp_tx_datagram()?;
        if drain.datagram.dst.port == 0 {
            drain.datagram.dst = connected_remote?;
        }
        // P2-S6: prefer the dispatch-resolved source (bound address or
        // enqueue-time hint); fall back to the loopback selection rule for
        // datagrams that predate the hint.
        let packet_src = if !drain.src.is_unspecified() && drain.src.port != 0 {
            drain.src
        } else {
            select_udp_packet_source(local, drain.datagram.dst, iface)
        };
        let packet = drain.datagram.emit_ipv4_packet(packet_src)?;
        if !iface.dispatch_ip(packet) {
            return None;
        }

        self.tx_packets += 1;
        self.sockets_touched += 1;
        Some(NetworkPublishTarget::new(
            source.clone(),
            NetworkPublish {
                send_has_space: drain.became_available,
                ..NetworkPublish::none()
            },
        ))
    }

    pub fn poll_icmp_egress_one(
        &mut self,
        source: &Cap<SocketIdentity>,
        iface: &LoopbackIface,
        _guard: &Guard<'_>,
    ) -> Option<NetworkPublishTarget> {
        let source_payload = source.acquire_operational()?;
        let drain = source_payload.take_icmp_tx_echo()?;
        let packet = build_icmpv4_echo_request(&drain.packet);
        if !iface.dispatch_ip(packet) {
            return None;
        }

        self.tx_packets += 1;
        self.sockets_touched += 1;
        Some(NetworkPublishTarget::new(
            source.clone(),
            NetworkPublish {
                send_has_space: drain.became_available,
                ..NetworkPublish::none()
            },
        ))
    }

    pub fn poll_ingress(
        &mut self,
        iface: &LoopbackIface,
        guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        let mut publishes = Vec::new();
        let mut created_children = Vec::new();
        let mut bytes_moved = 0;

        for _ in 0..budget {
            let Some(packet) = iface.pop_ingress() else {
                break;
            };
            self.packets_seen += 1;

            if let Some(segment) = SmoltcpTcpSegment::parse_ipv4_packet(&packet) {
                let Some(src) = segment.src_endpoint() else {
                    continue;
                };
                let Some(dst) = segment.dst_endpoint() else {
                    continue;
                };
                let key = ConnectionKey::new(dst, src);
                if let Some(target) = self.socket_table.lookup_tcp_connection(key, guard) {
                    let Some(target_payload) = target.acquire_operational() else {
                        continue;
                    };
                    if let Some(publish) =
                        self.process_segment_for_target(&target, &target_payload, &segment, guard)
                    {
                        bytes_moved += publish.bytes_moved;
                        publishes.extend(publish.publishes);
                    }
                } else if let Some(first_syn) = self.process_first_syn_for_listener(&segment, guard)
                {
                    bytes_moved += first_syn.bytes_moved;
                    publishes.extend(first_syn.publishes);
                    if let Some(child) = first_syn.created_child {
                        created_children.push(child);
                    }
                } else if let Some(backlog) = self.process_listener_backlog_segment(&segment, guard)
                {
                    bytes_moved += backlog.bytes_moved;
                    publishes.extend(backlog.publishes);
                }
                continue;
            }

            if let Some(datagram) = UdpRxDatagram::parse_ipv4_packet(&packet) {
                let payload_len = datagram.payload.len();
                let Some(target) =
                    self.socket_table
                        .lookup_udp_ingress(datagram.src, datagram.dst, guard)
                else {
                    continue;
                };
                let Some(target_ident) = target.downgrade().observe(guard) else {
                    continue;
                };
                let Some(target_payload) = target_ident.acquire_operational() else {
                    continue;
                };
                let mut publish = NetworkPublish::none();
                if target_payload.record_recv_payload(datagram.src, datagram.dst, datagram.payload)
                {
                    publish.recv_has_data = true;
                }
                self.sockets_touched += 1;
                bytes_moved += payload_len;
                if publish.has_any() {
                    publishes.push(NetworkPublishTarget::new(target, publish));
                }
                continue;
            }

            match parse_icmpv4_loopback_packet(&packet) {
                Icmpv4Event::EchoRequest(request)
                    if accepts_loopback_icmp_destination(iface, request.dst)
                        && iface.dispatch_ip(build_icmpv4_echo_reply(&request.reply_packet())) =>
                {
                    self.tx_packets += 1;
                }
                Icmpv4Event::EchoReply(reply) => {
                    let moved = icmpv4_echo_message_len(&reply);
                    for target in self.socket_table.snapshot_raw_icmp(guard) {
                        let Some(target_payload) = target.acquire_operational() else {
                            continue;
                        };
                        if !raw_icmp_accepts_reply(&target_payload.protocol_snapshot(), reply.dst) {
                            continue;
                        }
                        let mut publish = NetworkPublish::none();
                        if target_payload.record_icmp_recv_echo_reply(reply.clone()) {
                            publish.recv_has_data = true;
                        }
                        self.sockets_touched += 1;
                        bytes_moved += moved;
                        if publish.has_any() {
                            publishes.push(NetworkPublishTarget::new(target, publish));
                        }
                    }
                }
                _ => {}
            }
        }

        PollContextOutcome {
            packets_seen: self.packets_seen,
            tx_packets: self.tx_packets,
            sockets_touched: self.sockets_touched,
            bytes_moved,
            publishes,
            created_children,
        }
    }

    pub fn poll_ingress_to_socket(
        &mut self,
        iface: &LoopbackIface,
        target: &Cap<SocketIdentity>,
        guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        let mut publishes = Vec::new();
        let mut bytes_moved = 0;
        let mut matched = 0usize;

        while matched < budget {
            let Some(target_payload) = target.acquire_operational() else {
                break;
            };
            let target_protocol = target_payload.protocol_snapshot();
            let Some(packet) = iface.take_ingress_matching(|packet| {
                SmoltcpTcpSegment::packet_endpoints(packet).is_some_and(|(src, dst)| {
                    tcp_endpoints_match_socket(&target_protocol, src, dst)
                })
            }) else {
                break;
            };
            let Some(segment) = SmoltcpTcpSegment::parse_ipv4_packet(&packet) else {
                continue;
            };
            let Some(target_payload) = target.acquire_operational() else {
                continue;
            };
            self.packets_seen += 1;
            matched += 1;
            if let Some(publish) =
                self.process_segment_for_target(target, &target_payload, &segment, guard)
            {
                bytes_moved += publish.bytes_moved;
                publishes.extend(publish.publishes);
            }
        }

        PollContextOutcome {
            packets_seen: self.packets_seen,
            tx_packets: self.tx_packets,
            sockets_touched: self.sockets_touched,
            bytes_moved,
            publishes,
            created_children: Vec::new(),
        }
    }

    /// Process TCP packets for exactly one connection direction.
    ///
    /// This is the listener-side counterpart of `poll_ingress_to_socket`.
    /// Before the first SYN is consumed there is no child socket to target, so
    /// the endpoints themselves identify the handshake.  Selection is atomic
    /// in `LoopbackIface`; concurrent connect syscalls cannot drain or
    /// half-open another client's connection.
    pub fn poll_tcp_ingress_for_flow(
        &mut self,
        iface: &LoopbackIface,
        expected_src: IpEndpoint,
        expected_dst: IpEndpoint,
        guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        let mut publishes = Vec::new();
        let mut created_children = Vec::new();
        let mut bytes_moved = 0;
        let mut matched = 0usize;

        while matched < budget {
            let Some(packet) = iface.take_ingress_matching(|packet| {
                SmoltcpTcpSegment::packet_endpoints(packet) == Some((expected_src, expected_dst))
            }) else {
                break;
            };
            let Some(segment) = SmoltcpTcpSegment::parse_ipv4_packet(&packet) else {
                continue;
            };
            self.packets_seen += 1;
            matched += 1;

            let key = ConnectionKey::new(expected_dst, expected_src);
            if let Some(target) = self.socket_table.lookup_tcp_connection(key, guard) {
                let Some(target_payload) = target.acquire_operational() else {
                    continue;
                };
                if let Some(publish) =
                    self.process_segment_for_target(&target, &target_payload, &segment, guard)
                {
                    bytes_moved += publish.bytes_moved;
                    publishes.extend(publish.publishes);
                }
            } else if let Some(first_syn) = self.process_first_syn_for_listener(&segment, guard) {
                bytes_moved += first_syn.bytes_moved;
                publishes.extend(first_syn.publishes);
                if let Some(child) = first_syn.created_child {
                    created_children.push(child);
                }
            } else if let Some(backlog) = self.process_listener_backlog_segment(&segment, guard) {
                bytes_moved += backlog.bytes_moved;
                publishes.extend(backlog.publishes);
            }
        }

        PollContextOutcome {
            packets_seen: self.packets_seen,
            tx_packets: self.tx_packets,
            sockets_touched: self.sockets_touched,
            bytes_moved,
            publishes,
            created_children,
        }
    }

    pub fn poll_udp_ingress(
        &mut self,
        iface: &LoopbackIface,
        guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        self.poll_ingress(iface, guard, budget)
    }

    pub fn poll_icmp_ingress(
        &mut self,
        iface: &LoopbackIface,
        guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        self.poll_ingress(iface, guard, budget)
    }

    fn process_first_syn_for_listener(
        &mut self,
        segment: &SmoltcpTcpSegment,
        guard: &Guard<'_>,
    ) -> Option<FirstSynTarget> {
        if !is_first_syn(segment) {
            return None;
        }

        let src = segment.src_endpoint()?;
        let dst = segment.dst_endpoint()?;
        let listener = self
            .socket_table
            .lookup_tcp_listener_dual_stack_endpoint(dst, guard)?;
        let listener_payload = listener.acquire_operational()?;
        if !listener_accepts_incoming(&listener_payload, dst) {
            return None;
        }

        let options = listener_payload.with_options(Clone::clone);
        let child = registry::create_socket_in_namespace_with_family(
            SocketKind::Tcp,
            listener_payload.family(),
            options,
            listener_payload.net_namespace(),
        )
        .ok()?;
        let child_payload = child.acquire_operational()?;
        child_payload.with_protocol_mut(|protocol| {
            *protocol = SocketProtocol::Tcp(TcpState::Connecting {
                local: dst,
                remote: src,
            });
        });
        child_payload.raw_tcp_socket()?.listen_endpoint(dst).ok()?;
        let created_at = self.timestamp();
        listener_payload.enqueue_connecting_entry(TcpBacklogEntry {
            child: child.clone(),
            local: dst,
            peer: src,
            created_at,
            deadline: created_at + Duration::from_millis(TCP_BACKLOG_TIMEOUT_STAGING_MILLIS as u64),
            attempts: 1,
        })?;

        let processed = self.process_segment_for_target(&child, &child_payload, segment, guard)?;
        Some(FirstSynTarget {
            bytes_moved: processed.bytes_moved,
            publishes: processed.publishes,
            created_child: Some(child),
        })
    }

    fn process_listener_backlog_segment(
        &mut self,
        segment: &SmoltcpTcpSegment,
        guard: &Guard<'_>,
    ) -> Option<SegmentProcessTarget> {
        let src = segment.src_endpoint()?;
        let dst = segment.dst_endpoint()?;
        let listener = self
            .socket_table
            .lookup_tcp_listener_dual_stack_endpoint(dst, guard)?;
        let listener_payload = listener.acquire_operational()?;
        if !listener_accepts_incoming(&listener_payload, dst) {
            return None;
        }
        let child = listener_payload.connecting_child(dst, src)?;
        let child_payload = child.acquire_operational()?;
        self.process_segment_for_target(&child, &child_payload, segment, guard)
    }

    fn process_segment_for_target(
        &mut self,
        target: &Cap<SocketIdentity>,
        target_payload: &crate::net::structure::SocketOperationalEvidence,
        segment: &SmoltcpTcpSegment,
        guard: &Guard<'_>,
    ) -> Option<SegmentProcessTarget> {
        let target_raw = target_payload.raw_tcp_socket()?;

        // Data now lands directly in the smoltcp rx ring inside
        // `process_segment`; readability is derived from the ring below.
        let protocol_publish = target_raw.process_segment(segment);
        self.sockets_touched += 1;

        // PollContext is the loopback protocol lane.  Reaching smoltcp's
        // Established state is only the protocol half of connect: the client
        // connection index, server backlog and both public SocketProtocol
        // states are committed together by step_tcp_loopback_handshake.
        // Publishing Connected here used to expose a half-committed stream
        // between SYN-ACK and final ACK.
        let mut publishes = Vec::new();

        let publish = NetworkPublish {
            recv_has_data: protocol_publish.recv_readable
                || target.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() == 0
                    && target_raw.recv_available() > 0,
            send_has_space: protocol_publish.send_writable,
            recv_broken: protocol_publish.broken || protocol_publish.recv_closed,
            send_broken: protocol_publish.broken || protocol_publish.send_closed,
            ..NetworkPublish::none()
        };
        if publish.has_any() {
            publishes.push(NetworkPublishTarget::new(target.clone(), publish));
        }
        Some(SegmentProcessTarget {
            bytes_moved: protocol_publish.received_bytes,
            publishes,
        })
    }
}

struct FirstSynTarget {
    bytes_moved: usize,
    publishes: Vec<NetworkPublishTarget>,
    created_child: Option<Cap<SocketIdentity>>,
}

struct SegmentProcessTarget {
    bytes_moved: usize,
    publishes: Vec<NetworkPublishTarget>,
}

pub(crate) fn is_first_syn(segment: &SmoltcpTcpSegment) -> bool {
    segment.tcp.control == TcpControl::Syn && segment.tcp.ack_number.is_none()
}

pub(crate) fn listener_accepts_incoming(
    listener_payload: &crate::net::structure::SocketOperationalEvidence,
    dst: IpEndpoint,
) -> bool {
    let protocol = listener_payload.protocol_snapshot();
    let v6only = listener_payload.with_options(|options| options.ip.ipv6_v6only);
    matches!(
        protocol,
        SocketProtocol::Tcp(TcpState::Listening { local, .. }) if local.port == dst.port
            && ((local.same_family(dst)
                && (local.ip_addr() == dst.ip_addr() || local.is_unspecified()))
                || (!v6only
                    && local.family == AddressFamily::Inet6
                    && local.is_unspecified()
                    && dst.family == AddressFamily::Inet
                    && dst.is_loopback()))
    )
}

fn udp_endpoints(protocol: &SocketProtocol) -> Option<(IpEndpoint, Option<IpEndpoint>)> {
    match protocol {
        SocketProtocol::Udp(UdpInner::Bound { local }) => Some((*local, None)),
        SocketProtocol::Udp(UdpInner::Connected { local, remote }) => Some((*local, Some(*remote))),
        _ => None,
    }
}

fn select_udp_packet_source(
    local: IpEndpoint,
    dst: IpEndpoint,
    iface: &LoopbackIface,
) -> IpEndpoint {
    if local.is_unspecified() && dst.is_loopback() {
        let _ = iface;
        IpEndpoint::loopback_for_family(dst.family, local.port)
    } else {
        local
    }
}

fn accepts_loopback_icmp_destination(iface: &LoopbackIface, dst: Ipv4Address) -> bool {
    dst == iface.local_ipv4() || dst == Ipv4Address::BROADCAST
}

fn raw_icmp_accepts_reply(protocol: &SocketProtocol, dst: Ipv4Address) -> bool {
    match protocol {
        SocketProtocol::RawIcmp(state) => state.accepts_ipv4_reply_to(dst),
        _ => false,
    }
}

fn tcp_endpoints_match_socket(protocol: &SocketProtocol, src: IpEndpoint, dst: IpEndpoint) -> bool {
    matches!(
        protocol,
        SocketProtocol::Tcp(
            TcpState::Connecting { local, remote } | TcpState::Connected { local, remote }
        ) if *local == dst && *remote == src
    )
}

pub(crate) fn promote_connected_stream_and_publish_accept(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    payload: &crate::net::structure::SocketOperationalEvidence,
    guard: &Guard<'_>,
) -> Option<NetworkPublishTarget> {
    let (local, remote) = match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => (local, remote),
        SocketProtocol::Tcp(TcpState::Connected { .. }) => return None,
        _ => return None,
    };
    let key = ConnectionKey::new(local, remote);

    // A matching half-open backlog entry identifies the server side.  An
    // outbound client has already reserved its 4-tuple before packets are
    // driven and therefore takes the client branch below.
    if let Some(listener) = table.lookup_tcp_listener_dual_stack_endpoint(local, guard) {
        if let Some(listener_payload) = listener.acquire_operational() {
            let is_server_child = listener_payload
                .connecting_child(local, remote)
                .is_some_and(|child| child.raw() == socket.raw());
            if is_server_child && listener_accepts_incoming(&listener_payload, local) {
                let inserted = ensure_tcp_connection_entry(table, key, socket, guard)?;
                let Some(became_ready) =
                    listener_payload.promote_connecting_to_accept(local, remote)
                else {
                    if inserted {
                        let _ = table.withdraw_tcp_connection(key);
                    }
                    return None;
                };
                mark_tcp_protocol_connected(payload, local, remote)?;
                return became_ready.then(|| {
                    NetworkPublishTarget::new(
                        listener,
                        NetworkPublish {
                            accept_has_pending: true,
                            ..NetworkPublish::none()
                        },
                    )
                });
            }
        }
    }

    // Outbound side (loopback or real device): never expose Connected unless
    // its routing-table entry is already committed and still names this
    // socket.  This makes retransmitted/duplicate protocol events idempotent.
    let indexed = table.lookup_tcp_connection(key, guard)?;
    if indexed.raw() != socket.raw() {
        return None;
    }
    mark_tcp_protocol_connected(payload, local, remote)?;
    None
}

fn ensure_tcp_connection_entry(
    table: &SocketTable,
    key: ConnectionKey,
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> Option<bool> {
    match table.insert_tcp_connection(key, socket.clone()) {
        Ok(()) => Some(true),
        Err(IndexError::Duplicate) => table
            .lookup_tcp_connection(key, guard)
            .filter(|existing| existing.raw() == socket.raw())
            .map(|_| false),
        Err(IndexError::Full | IndexError::Missing | IndexError::Busy) => None,
    }
}

fn mark_tcp_protocol_connected(
    payload: &crate::net::structure::SocketOperationalEvidence,
    local: IpEndpoint,
    remote: IpEndpoint,
) -> Option<()> {
    payload.with_protocol_mut(|protocol| match protocol {
        SocketProtocol::Tcp(TcpState::Connecting {
            local: current_local,
            remote: current_remote,
        }) if *current_local == local && *current_remote == remote => {
            *protocol = SocketProtocol::Tcp(TcpState::Connected { local, remote });
            Some(())
        }
        SocketProtocol::Tcp(TcpState::Connected {
            local: current_local,
            remote: current_remote,
        }) if *current_local == local && *current_remote == remote => Some(()),
        _ => None,
    })
}
