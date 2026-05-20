use alloc::vec::Vec;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::TcpControl;
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
    ConnectionKey, IpEndpoint, Ipv4Address, RawIcmpState, RecvWireSet, SocketIdentity, SocketKind,
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
    pub const fn new(timestamp: Instant) -> Self {
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
        let source_payload = source.acquire_operational()?;
        let (local, connected_remote) = udp_endpoints(&source_payload.protocol_snapshot())?;
        let mut drain = source_payload.take_udp_tx_datagram()?;
        if drain.datagram.dst.port == 0 {
            drain.datagram.dst = connected_remote?;
        }
        let packet_src = select_udp_packet_source(local, drain.datagram.dst, iface);
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

    pub fn poll_udp_loopback_direct_one(
        &mut self,
        source: &Cap<SocketIdentity>,
        iface: &LoopbackIface,
        guard: &Guard<'_>,
    ) -> Option<PollContextOutcome> {
        let source_payload = source.acquire_operational()?;
        let (local, connected_remote) = udp_endpoints(&source_payload.protocol_snapshot())?;
        let mut drain = source_payload.take_udp_tx_datagram()?;
        if drain.datagram.dst.port == 0 {
            drain.datagram.dst = connected_remote?;
        }
        let src = select_udp_packet_source(local, drain.datagram.dst, iface);
        if src.port == 0 || drain.datagram.dst.port == 0 || drain.datagram.payload.is_empty() {
            return None;
        }
        if udp_ipv4_packet_len(drain.datagram.payload.len()) > usize::from(iface.mtu()) {
            return None;
        }

        self.tx_packets += 1;
        self.packets_seen += 1;
        self.sockets_touched += 1;

        let payload_len = drain.datagram.payload.len();
        let mut publishes = Vec::new();
        if drain.became_available {
            publishes.push(NetworkPublishTarget::new(
                source.clone(),
                NetworkPublish {
                    send_has_space: true,
                    ..NetworkPublish::none()
                },
            ));
        }

        let Some(target) = self
            .socket_table
            .lookup_udp_ingress(src, drain.datagram.dst, guard)
        else {
            return Some(PollContextOutcome {
                packets_seen: self.packets_seen,
                tx_packets: self.tx_packets,
                sockets_touched: self.sockets_touched,
                bytes_moved: 0,
                publishes,
                created_children: Vec::new(),
            });
        };
        let Some(target_payload) = target.acquire_operational() else {
            return Some(PollContextOutcome {
                packets_seen: self.packets_seen,
                tx_packets: self.tx_packets,
                sockets_touched: self.sockets_touched,
                bytes_moved: 0,
                publishes,
                created_children: Vec::new(),
            });
        };

        let mut peer_publish = NetworkPublish::none();
        if target_payload.record_recv_payload(src, drain.datagram.dst, drain.datagram.payload) {
            peer_publish.recv_has_data = true;
        }
        self.sockets_touched += 1;
        if peer_publish.has_any() {
            publishes.push(NetworkPublishTarget::new(target, peer_publish));
        }

        Some(PollContextOutcome {
            packets_seen: self.packets_seen,
            tx_packets: self.tx_packets,
            sockets_touched: self.sockets_touched,
            bytes_moved: payload_len,
            publishes,
            created_children: Vec::new(),
        })
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

            let Some(segment) = SmoltcpTcpSegment::parse_ipv4_packet(&packet) else {
                continue;
            };
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

    pub fn poll_ingress_to_socket(
        &mut self,
        iface: &LoopbackIface,
        target: &Cap<SocketIdentity>,
        _guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        let mut publishes = Vec::new();
        let mut bytes_moved = 0;

        for _ in 0..budget {
            let Some(packet) = iface.pop_ingress() else {
                break;
            };
            self.packets_seen += 1;

            let Some(segment) = SmoltcpTcpSegment::parse_ipv4_packet(&packet) else {
                continue;
            };
            let Some(target_payload) = target.acquire_operational() else {
                continue;
            };
            if let Some(publish) =
                self.process_segment_for_target(target, &target_payload, &segment, _guard)
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

    pub fn poll_udp_ingress(
        &mut self,
        iface: &LoopbackIface,
        guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        let mut publishes = Vec::new();
        let mut bytes_moved = 0;

        for _ in 0..budget {
            let Some(packet) = iface.pop_ingress() else {
                break;
            };
            self.packets_seen += 1;

            let Some(datagram) = UdpRxDatagram::parse_ipv4_packet(&packet) else {
                continue;
            };
            let payload_len = datagram.payload.len();
            let Some(target) =
                self.socket_table
                    .lookup_udp_ingress(datagram.src, datagram.dst, guard)
            else {
                continue;
            };
            let Some(target_payload) = target.acquire_operational() else {
                continue;
            };

            let mut publish = NetworkPublish::none();
            if target_payload.record_recv_payload(datagram.src, datagram.dst, datagram.payload) {
                publish.recv_has_data = true;
            }
            self.sockets_touched += 1;
            bytes_moved += payload_len;
            if publish.has_any() {
                publishes.push(NetworkPublishTarget::new(target, publish));
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

    pub fn poll_icmp_ingress(
        &mut self,
        iface: &LoopbackIface,
        guard: &Guard<'_>,
        budget: usize,
    ) -> PollContextOutcome {
        let mut publishes = Vec::new();
        let mut bytes_moved = 0;

        for _ in 0..budget {
            let Some(packet) = iface.pop_ingress() else {
                break;
            };
            self.packets_seen += 1;

            match parse_icmpv4_loopback_packet(&packet) {
                Icmpv4Event::EchoRequest(request)
                    if accepts_loopback_icmp_destination(iface, request.dst) =>
                {
                    if iface.dispatch_ip(build_icmpv4_echo_reply(&request.reply_packet())) {
                        self.tx_packets += 1;
                    }
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
            created_children: Vec::new(),
        }
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
            .lookup_tcp_listener_addr(dst.addr, dst.port, guard)?;
        let listener_payload = listener.acquire_operational()?;
        if !listener_matches_incoming(&listener_payload.protocol_snapshot(), dst) {
            return None;
        }

        let options = listener_payload.with_options(Clone::clone);
        let child = registry::create_socket_in_namespace(
            SocketKind::Tcp,
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
            .lookup_tcp_listener_addr(dst.addr, dst.port, guard)?;
        let listener_payload = listener.acquire_operational()?;
        if !listener_matches_incoming(&listener_payload.protocol_snapshot(), dst) {
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

        let protocol_publish = target_raw.process_segment(segment);
        let became_readable = target_raw.drain_protocol_recv_to_staging();
        target_payload.refresh_io_from_raw();
        self.sockets_touched += 1;

        let mut publishes = Vec::new();
        if protocol_publish.connected {
            if let Some(accept_publish) = promote_connected_stream_and_publish_accept(
                self.socket_table,
                target,
                target_payload,
                guard,
            ) {
                publishes.push(accept_publish);
            }
        }

        let publish = NetworkPublish {
            recv_has_data: became_readable
                || protocol_publish.recv_readable
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
            bytes_moved: segment.payload_len(),
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

fn is_first_syn(segment: &SmoltcpTcpSegment) -> bool {
    segment.tcp.control == TcpControl::Syn && segment.tcp.ack_number.is_none()
}

fn listener_matches_incoming(protocol: &SocketProtocol, dst: IpEndpoint) -> bool {
    matches!(
        protocol,
        SocketProtocol::Tcp(TcpState::Listening { local, .. })
            if local.port == dst.port
                && (local.addr == dst.addr || local.addr == Ipv4Address::UNSPECIFIED)
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
    if local.addr == Ipv4Address::UNSPECIFIED && dst.addr == iface.local_ipv4() {
        IpEndpoint::new(iface.local_ipv4(), local.port)
    } else {
        local
    }
}

fn udp_ipv4_packet_len(payload_len: usize) -> usize {
    const IPV4_HEADER_LEN: usize = 20;
    const UDP_HEADER_LEN: usize = 8;
    IPV4_HEADER_LEN + UDP_HEADER_LEN + payload_len
}

fn accepts_loopback_icmp_destination(iface: &LoopbackIface, dst: Ipv4Address) -> bool {
    dst == iface.local_ipv4() || dst == Ipv4Address::BROADCAST
}

fn raw_icmp_accepts_reply(protocol: &SocketProtocol, dst: Ipv4Address) -> bool {
    match protocol {
        SocketProtocol::RawIcmp(RawIcmpState { bound_local, .. }) => {
            bound_local.is_none_or(|local| local == dst)
        }
        _ => false,
    }
}

fn promote_connected_stream_and_publish_accept(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    payload: &crate::net::structure::SocketOperationalEvidence,
    guard: &Guard<'_>,
) -> Option<NetworkPublishTarget> {
    let mut connected = None;
    payload.with_protocol_mut(|protocol| {
        if let SocketProtocol::Tcp(TcpState::Connecting { local, remote }) = protocol {
            let state_local = *local;
            let state_remote = *remote;
            connected = Some((state_local, state_remote));
            *protocol = SocketProtocol::Tcp(TcpState::Connected {
                local: state_local,
                remote: state_remote,
            });
        }
    });
    let (local, remote) = connected?;

    let listener = table.lookup_tcp_listener_addr(local.addr, local.port, guard)?;
    let listener_payload = listener.acquire_operational()?;
    if !listener_matches_incoming(&listener_payload.protocol_snapshot(), local) {
        return None;
    }
    table
        .insert_tcp_connection(ConnectionKey::new(local, remote), socket.clone())
        .ok()?;
    let became_ready = listener_payload.promote_connecting_to_accept(local, remote)?;
    became_ready.then(|| {
        NetworkPublishTarget::new(
            listener,
            NetworkPublish {
                accept_has_pending: true,
                ..NetworkPublish::none()
            },
        )
    })
}
