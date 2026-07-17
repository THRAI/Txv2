use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::vec::Vec;
use smoltcp::socket::tcp;
use smoltcp::time::{Duration, Instant};
use tx_substrate::zone::Cap;
use tx_substrate::zone::PayloadCap;

use crate::net::namespace::{initial_net_namespace_payload, NetNamespacePayload};
use crate::net::nfnetlink::{NetlinkNetfilterState, RawNetlinkNetfilterSocket};
use crate::net::rtnetlink::{NetlinkRouteState, RawNetlinkRouteSocket};
use crate::net::structure::SocketTable;
use crate::sync::SpinMutex;

use super::super::protocol::{
    parse_icmpv4_echo_payload_unchecked, parse_icmpv4_payload,
    parse_raw_icmpv4_echo_payload_unchecked, Icmpv4EchoPacket, Icmpv4Event, RawIcmpSocket,
    RawIpAddress, RawIpv6Packet, RawTcpSocket, RawUdpSocket, UdpTxDatagram,
};
use super::identity::SocketIdentity;
use super::multicast::{Ipv4MulticastGroup, Ipv4MulticastMemberships};
use super::types::{
    AddressFamily, IpEndpoint, Ipv4Address, Ipv6Address, PacketSocketState, ProtocolNumber,
    RawIcmpState, RdsState, SockAddrLl, SockShutdownCmd, SocketKind, SocketOptionSet, SocketType,
    TcpState, UdpInner, UnixSocketPath,
};

pub type SocketOperationalEvidence = PayloadCap<SocketPayload>;
pub const TCP_BACKLOG_TIMEOUT_STAGING_MILLIS: i64 = 30_000;
pub const TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING: u8 = 3;
pub const TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS: i64 = 1_000;

/// P3-B S1 (audit ⑨): exactly ONE protocol engine per socket. Replaces
/// the nine parallel `Option<RawX>` slots — invalid states (two engines,
/// or none) are unrepresentable, and the former 2–8-tuple matches
/// collapse to single-arm matches. The `SocketProtocol` FSM stays
/// separate on purpose: it is the syscall-level intent state (shared by
/// Sctp via `TcpState`), not the engine discriminator.
pub(crate) enum SocketImpl {
    Tcp(RawTcpSocket),
    Udp(RawUdpSocket),
    Icmp(RawIcmpSocket),
    Unix(RawUnixSocket),
    Rds(RawRdsSocket),
    Sctp(RawSctpSocket),
    Packet(RawPacketSocket),
    NetlinkRoute(RawNetlinkRouteSocket),
    NetlinkNetfilter(RawNetlinkNetfilterSocket),
}

impl SocketImpl {
    pub(crate) fn tcp(&self) -> Option<&RawTcpSocket> {
        match self {
            Self::Tcp(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn udp(&self) -> Option<&RawUdpSocket> {
        match self {
            Self::Udp(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn icmp(&self) -> Option<&RawIcmpSocket> {
        match self {
            Self::Icmp(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn unix(&self) -> Option<&RawUnixSocket> {
        match self {
            Self::Unix(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn rds(&self) -> Option<&RawRdsSocket> {
        match self {
            Self::Rds(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn sctp(&self) -> Option<&RawSctpSocket> {
        match self {
            Self::Sctp(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn packet(&self) -> Option<&RawPacketSocket> {
        match self {
            Self::Packet(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn netlink_route(&self) -> Option<&RawNetlinkRouteSocket> {
        match self {
            Self::NetlinkRoute(raw) => Some(raw),
            _ => None,
        }
    }

    pub(crate) fn netlink_netfilter(&self) -> Option<&RawNetlinkNetfilterSocket> {
        match self {
            Self::NetlinkNetfilter(raw) => Some(raw),
            _ => None,
        }
    }
}

pub struct SocketPayload {
    pub(crate) family: SpinMutex<AddressFamily>,
    pub(crate) net_namespace: PayloadCap<NetNamespacePayload>,
    pub(crate) protocol: SpinMutex<SocketProtocol>,
    pub(crate) options: SpinMutex<SocketOptionSet>,
    pub(crate) ip_multicast: SpinMutex<Ipv4MulticastMemberships>,
    pub(crate) imp: SocketImpl,
    pub(crate) unix_peer_cred: SpinMutex<Option<UnixPeerCred>>,
    pub(crate) tcp_backlog: SpinMutex<TcpBacklog>,
    pub shutdown_rd: AtomicBool,
    pub shutdown_wr: AtomicBool,
}

impl SocketPayload {
    pub fn new(kind: SocketKind, options: SocketOptionSet) -> Self {
        Self::new_in_namespace(kind, options, initial_net_namespace_payload())
    }

    pub fn new_in_namespace(
        kind: SocketKind,
        options: SocketOptionSet,
        net_namespace: PayloadCap<NetNamespacePayload>,
    ) -> Self {
        Self::new_in_namespace_with_family(
            kind,
            default_family_for_kind(kind),
            options,
            net_namespace,
        )
    }

    pub fn new_in_namespace_with_family(
        kind: SocketKind,
        family: AddressFamily,
        options: SocketOptionSet,
        net_namespace: PayloadCap<NetNamespacePayload>,
    ) -> Self {
        // kind → (intent FSM, engine) is a clean surjection: every kind
        // activates exactly one engine (UnixDatagram/UnixStream share
        // Unix; NetlinkXfrm/NetlinkNetfilter share NetlinkNetfilter).
        // The former out-of-band `raw_packet` fill is normalised here.
        let (protocol, imp) = match kind {
            SocketKind::UnixDatagram => (
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound),
                SocketImpl::Unix(RawUnixSocket::new(&options)),
            ),
            SocketKind::UnixStream => (
                SocketProtocol::UnixStream(UnixStreamState::Init),
                SocketImpl::Unix(RawUnixSocket::new(&options)),
            ),
            SocketKind::Tcp => (
                SocketProtocol::Tcp(TcpState::Init),
                SocketImpl::Tcp(RawTcpSocket::new(&options)),
            ),
            SocketKind::Udp => (
                SocketProtocol::Udp(UdpInner::Unbound),
                SocketImpl::Udp(RawUdpSocket::new(&options)),
            ),
            SocketKind::Sctp => (
                SocketProtocol::Sctp(TcpState::Init),
                SocketImpl::Sctp(RawSctpSocket::new(&options)),
            ),
            SocketKind::RdsSeqPacket => (
                SocketProtocol::Rds(RdsState::Unbound),
                SocketImpl::Rds(RawRdsSocket::new(&options)),
            ),
            SocketKind::RawIcmp => (
                SocketProtocol::RawIcmp(RawIcmpState::new(ProtocolNumber(1))),
                SocketImpl::Icmp(RawIcmpSocket::new(&options)),
            ),
            SocketKind::NetlinkRoute => (
                SocketProtocol::NetlinkRoute(NetlinkRouteState),
                SocketImpl::NetlinkRoute(RawNetlinkRouteSocket::new()),
            ),
            SocketKind::NetlinkXfrm | SocketKind::NetlinkNetfilter => (
                SocketProtocol::NetlinkNetfilter(NetlinkNetfilterState),
                SocketImpl::NetlinkNetfilter(RawNetlinkNetfilterSocket::new()),
            ),
            SocketKind::Packet => (
                SocketProtocol::Packet(PacketSocketState::new(0)),
                SocketImpl::Packet(RawPacketSocket::new(&options)),
            ),
        };
        let payload = Self {
            family: SpinMutex::new(family),
            net_namespace,
            protocol: SpinMutex::new(protocol),
            options: SpinMutex::new(options),
            ip_multicast: SpinMutex::new(Ipv4MulticastMemberships::empty()),
            imp,
            unix_peer_cred: SpinMutex::new(None),
            tcp_backlog: SpinMutex::new(TcpBacklog::new()),
            shutdown_rd: AtomicBool::new(false),
            shutdown_wr: AtomicBool::new(false),
        };
        payload
    }

    pub fn family(&self) -> AddressFamily {
        *self.family.lock()
    }

    pub fn set_family(&self, family: AddressFamily) {
        *self.family.lock() = family;
    }

    pub fn net_namespace(&self) -> PayloadCap<NetNamespacePayload> {
        self.net_namespace.clone()
    }

    pub fn socket_table(&self) -> &'static SocketTable {
        self.net_namespace.socket_table()
    }

    pub fn shutdown_rd(&self) -> bool {
        self.shutdown_rd.load(Ordering::Acquire)
    }

    pub fn shutdown_wr(&self) -> bool {
        self.shutdown_wr.load(Ordering::Acquire)
    }

    pub fn protocol_snapshot(&self) -> SocketProtocol {
        self.protocol.lock().clone()
    }

    pub fn bind_packet_socket(&self, sockaddr: SockAddrLl) -> Result<(), crate::execution::Errno> {
        if sockaddr.ifindex < 0 {
            return Err(crate::execution::Errno::ENODEV);
        }
        if sockaddr.ifindex != 0 {
            let ifindex = sockaddr.ifindex as u32;
            if !self
                .net_namespace
                .link_snapshot()
                .into_iter()
                .any(|link| link.ifindex == ifindex)
            {
                return Err(crate::execution::Errno::ENODEV);
            }
        }

        self.with_protocol_mut(|protocol| match protocol {
            SocketProtocol::Packet(state) => {
                state.protocol = sockaddr.protocol;
                state.ifindex = Some(sockaddr.ifindex);
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub fn packet_sockaddr(&self) -> Option<SockAddrLl> {
        match self.protocol_snapshot() {
            SocketProtocol::Packet(state) => Some(state.sockaddr()),
            _ => None,
        }
    }

    pub fn set_packet_protocol(&self, protocol: u16) -> Result<(), crate::execution::Errno> {
        self.with_protocol_mut(|socket_protocol| match socket_protocol {
            SocketProtocol::Packet(state) => {
                state.protocol = protocol;
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub fn set_packet_version(&self, version: i32) -> Result<(), crate::execution::Errno> {
        self.with_protocol_mut(|socket_protocol| match socket_protocol {
            SocketProtocol::Packet(state) => {
                state.packet_version = version;
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub fn set_packet_reserve(&self, reserve: u32) -> Result<(), crate::execution::Errno> {
        self.with_protocol_mut(|socket_protocol| match socket_protocol {
            SocketProtocol::Packet(state) => {
                if state
                    .packet_rx_ring_block_size
                    .is_some_and(|block_size| reserve > block_size)
                {
                    return false;
                }
                state.packet_reserve = reserve;
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub fn set_packet_rx_ring_block_size(
        &self,
        block_size: Option<u32>,
    ) -> Result<(), crate::execution::Errno> {
        self.with_protocol_mut(|socket_protocol| match socket_protocol {
            SocketProtocol::Packet(state) => {
                state.packet_rx_ring_block_size = block_size;
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub fn set_packet_vnet_hdr(&self, enabled: bool) -> Result<(), crate::execution::Errno> {
        self.with_protocol_mut(|socket_protocol| match socket_protocol {
            SocketProtocol::Packet(state) => {
                state.packet_vnet_hdr = enabled;
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub fn packet_vnet_hdr(&self) -> Result<bool, crate::execution::Errno> {
        match self.protocol_snapshot() {
            SocketProtocol::Packet(state) => Ok(state.packet_vnet_hdr),
            _ => Err(crate::execution::Errno::EINVAL),
        }
    }

    pub fn packet_reserve(&self) -> Result<u32, crate::execution::Errno> {
        match self.protocol_snapshot() {
            SocketProtocol::Packet(state) => Ok(state.packet_reserve),
            _ => Err(crate::execution::Errno::EINVAL),
        }
    }

    pub(crate) fn with_protocol<R>(&self, f: impl FnOnce(&SocketProtocol) -> R) -> R {
        f(&self.protocol.lock())
    }

    pub(crate) fn with_protocol_mut<R>(&self, f: impl FnOnce(&mut SocketProtocol) -> R) -> R {
        f(&mut self.protocol.lock())
    }

    pub(crate) fn mark_shutdown(&self, how: SockShutdownCmd) -> ShutdownMark {
        match how {
            SockShutdownCmd::Recv => ShutdownMark {
                recv: !self.shutdown_rd.swap(true, Ordering::AcqRel),
                send: false,
            },
            SockShutdownCmd::Send => ShutdownMark {
                recv: false,
                send: !self.shutdown_wr.swap(true, Ordering::AcqRel),
            },
            SockShutdownCmd::Both => ShutdownMark {
                recv: !self.shutdown_rd.swap(true, Ordering::AcqRel),
                send: !self.shutdown_wr.swap(true, Ordering::AcqRel),
            },
        }
    }

    pub fn with_options<R>(&self, f: impl FnOnce(&SocketOptionSet) -> R) -> R {
        f(&self.options.lock())
    }

    pub fn with_options_mut<R>(&self, f: impl FnOnce(&mut SocketOptionSet) -> R) -> R {
        f(&mut self.options.lock())
    }

    pub fn join_ipv4_multicast_group(
        &self,
        group: Ipv4MulticastGroup,
    ) -> Result<(), crate::execution::Errno> {
        self.ip_multicast.lock().join(group)
    }

    pub fn leave_ipv4_multicast_group(
        &self,
        group: Ipv4MulticastGroup,
    ) -> Result<(), crate::execution::Errno> {
        self.ip_multicast.lock().leave(group)
    }

    /// P3-B S3 (D7): readiness derived live from the engine + backlog on
    /// every call — there is no cached `io` field to tear (R1b gone). The
    /// per-engine single lock (S2) makes each field's read atomic; this
    /// snapshot is not atomic across the three fields, which is fine —
    /// poll re-reads each independently anyway.
    pub fn io_snapshot(&self) -> SocketIoState {
        SocketIoState {
            recv_len: self.raw_recv_available(),
            send_space: self.raw_send_available(),
            accept_pending: self.tcp_backlog.lock().connected_len(),
        }
    }

    pub fn unix_peer_cred(&self) -> Option<UnixPeerCred> {
        *self.unix_peer_cred.lock()
    }

    pub fn set_unix_peer_cred(&self, cred: UnixPeerCred) {
        *self.unix_peer_cred.lock() = Some(cred);
    }

    pub fn raw_tcp_socket(&self) -> Option<&RawTcpSocket> {
        self.imp.tcp()
    }

    pub fn reset_raw_tcp_socket(&self) -> Result<(), crate::execution::Errno> {
        let Some(raw_tcp) = self.imp.tcp() else {
            return Err(crate::execution::Errno::EOPNOTSUPP);
        };
        self.with_options(|options| raw_tcp.reset(options));
        Ok(())
    }

    pub fn tcp_recv_closed_by_peer(&self) -> bool {
        self.imp.tcp()
            .is_some_and(RawTcpSocket::is_recv_closed)
    }

    pub fn raw_udp_socket(&self) -> Option<&RawUdpSocket> {
        self.imp.udp()
    }

    pub fn raw_icmp_socket(&self) -> Option<&RawIcmpSocket> {
        self.imp.icmp()
    }

    pub(crate) fn raw_netlink_route_socket(&self) -> Option<&RawNetlinkRouteSocket> {
        self.imp.netlink_route()
    }

    pub(crate) fn raw_netlink_netfilter_socket(&self) -> Option<&RawNetlinkNetfilterSocket> {
        self.imp.netlink_netfilter()
    }

    pub(crate) fn record_unix_datagram(
        &self,
        source: Option<UnixSocketPath>,
        payload: Vec<u8>,
    ) -> Option<bool> {
        let raw_unix = self.imp.unix()?;
        let became_readable = raw_unix.ingest_datagram(source, payload)?;
        Some(became_readable)
    }

    pub(crate) fn record_unix_stream_bytes(&self, payload: Vec<u8>) -> Option<bool> {
        let raw_unix = self.imp.unix()?;
        let became_readable = raw_unix.ingest_stream_bytes(payload)?;
        Some(became_readable)
    }

    pub(crate) fn record_rds_packet(
        &self,
        source: IpEndpoint,
        destination: IpEndpoint,
        payload: Vec<u8>,
    ) -> Option<bool> {
        let raw_rds = self.imp.rds()?;
        let became_readable = raw_rds.ingest_packet(source, destination, payload)?;
        Some(became_readable)
    }

    pub(crate) fn record_sctp_message(
        &self,
        payload: Vec<u8>,
        notification: bool,
        stream: u16,
        ppid: u32,
        source: Option<IpEndpoint>,
    ) -> Option<bool> {
        let raw_sctp = self.imp.sctp()?;
        let became_readable =
            raw_sctp.ingest_message(payload, notification, stream, ppid, source)?;
        Some(became_readable)
    }

    /// 1-to-many: find or create the association to `peer`; returns (id, is_new).
    pub(crate) fn sctp_ensure_assoc(&self, peer: IpEndpoint) -> Option<(u32, bool)> {
        Some(self.imp.sctp()?.ensure_assoc(peer))
    }

    pub(crate) fn sctp_peers(&self) -> Vec<SctpAssoc> {
        self.imp.sctp()
            .map_or_else(Vec::new, RawSctpSocket::peers_snapshot)
    }

    /// Remove the 1-to-many association named by `assoc_id`, returning its peer
    /// endpoint if it existed.
    pub fn sctp_remove_assoc(&self, assoc_id: u32) -> Option<IpEndpoint> {
        self.imp.sctp()?.remove_assoc(assoc_id)
    }

    /// Peer endpoint of the 1-to-many (SEQPACKET) association named by
    /// `assoc_id`, for SCTP_GET_PEER_ADDRS (sctp_getpaddrs). None if no such
    /// association exists.
    pub fn sctp_peer_addr_by_assoc(&self, assoc_id: u32) -> Option<IpEndpoint> {
        self.sctp_peers()
            .into_iter()
            .find(|assoc| assoc.assoc_id == assoc_id)
            .map(|assoc| assoc.peer)
    }

    /// 1-to-many (SEQPACKET): the association id whose peer endpoint is `peer`,
    /// if such an association exists (used to report sctp_connectx's assoc id).
    pub fn sctp_assoc_id_for_peer(&self, peer: IpEndpoint) -> Option<u32> {
        self.sctp_peers()
            .into_iter()
            .find(|assoc| assoc.peer == peer)
            .map(|assoc| assoc.assoc_id)
    }

    /// All local addresses this socket is bound to (primary bind + sctp_bindx).
    pub fn sctp_local_addrs(&self) -> Vec<IpEndpoint> {
        self.imp.sctp()
            .map_or_else(Vec::new, RawSctpSocket::local_addrs)
    }

    /// Record an additional bound local address (bind primary / sctp_bindx).
    pub fn sctp_add_local_addr(&self, endpoint: IpEndpoint) {
        if let Some(raw) = self.imp.sctp() {
            raw.add_local_addr(endpoint);
        }
    }

    /// The full multi-homed address set of the peer reachable at `peer`, for
    /// SCTP_GET_PEER_ADDRS: resolve the peer socket and return its bound address
    /// set. Falls back to just `peer` if the peer socket can't be resolved or
    /// reports no addresses.
    pub fn sctp_peer_local_addrs(&self, peer: IpEndpoint) -> Vec<IpEndpoint> {
        let guard = tx_substrate::epoch::guard();
        let table = self.socket_table();
        let addrs = table
            .lookup_sctp_listener_dual_stack_endpoint(peer, &guard)
            .or_else(|| table.lookup_sctp_bound(peer, &guard))
            .and_then(|sock| sock.acquire_operational())
            .map(|payload| payload.sctp_local_addrs())
            .unwrap_or_default();
        if addrs.is_empty() {
            alloc::vec![peer]
        } else {
            addrs
        }
    }

    /// Number of 1-to-many (SEQPACKET) associations on this socket.
    pub fn sctp_assoc_count(&self) -> usize {
        self.imp.sctp()
            .map_or(0, |raw| raw.peers_snapshot().len())
    }

    pub fn record_packet_frame(&self, source: SockAddrLl, payload: Vec<u8>) -> Option<bool> {
        let raw_packet = self.imp.packet()?;
        let became_readable = raw_packet.ingest_frame(source, payload)?;
        Some(became_readable)
    }

    pub fn accept_queue_len(&self) -> usize {
        self.tcp_backlog.lock().connected_len()
    }

    pub fn connecting_backlog_len(&self) -> usize {
        self.tcp_backlog.lock().connecting_len()
    }

    pub fn tcp_backlog_next_deadline(&self) -> Option<Instant> {
        self.tcp_backlog.lock().next_deadline()
    }

    pub(crate) fn record_recv_payload(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        payload: Vec<u8>,
    ) -> bool {
        // TCP RX no longer lands here: established-connection segments feed
        // smoltcp via `process_segment` and the data lives in its rx ring.
        let became_readable = match self.imp.udp() {
            Some(raw_udp) => raw_udp.ingest_rx_datagram(src, dst, payload),
            None => false,
        };
        became_readable
    }

    pub(crate) fn consume_recv_bytes(&self, len: usize) -> Option<SocketIoConsume> {
        let (bytes, became_empty) = self.raw_recv_len(len, false)?;
        Some(SocketIoConsume {
            bytes,
            became_empty,
        })
    }

    pub(crate) fn consume_recv_bytes_into(
        &self,
        out: &mut [u8],
        flags: super::types::SendRecvFlags,
    ) -> Option<SocketRecvBytesOutcome> {
        let peek = flags.contains(super::types::SendRecvFlags::MSG_PEEK);
        if let Some(raw_packet) = self.imp.packet() {
            let drain = raw_packet.recv_frame_bytes(out, peek)?;
            return Some(SocketRecvBytesOutcome {
                bytes: drain.bytes,
                source: None,
                unix_source: None,
                packet_source: Some(drain.source),
                destination: None,
                truncated: drain.truncated,
                became_empty: drain.became_empty,
                eor: false,
                sctp_notification: false,
                sctp_stream: 0,
                sctp_ppid: 0,
            });
        }
        let unix_stream = self.with_protocol(|protocol| {
            matches!(
                protocol,
                SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
            )
        });
        let outcome = match &self.imp {
            SocketImpl::Tcp(raw_tcp) => match raw_tcp.recv_bytes(out, peek) {
                Some((bytes, became_empty)) => SocketRecvBytesOutcome {
                    bytes,
                    source: None,
                    unix_source: None,
                    packet_source: None,
                    destination: None,
                    truncated: false,
                    became_empty,
                    eor: false,
                    sctp_notification: false,
                    sctp_stream: 0,
                    sctp_ppid: 0,
                },
                None if raw_tcp.is_recv_closed() => SocketRecvBytesOutcome {
                    bytes: 0,
                    source: None,
                    unix_source: None,
                    packet_source: None,
                    destination: None,
                    truncated: false,
                    became_empty: false,
                    eor: false,
                    sctp_notification: false,
                    sctp_stream: 0,
                    sctp_ppid: 0,
                },
                None => return None,
            },
            SocketImpl::Udp(raw_udp) => {
                let drain = raw_udp.recv_datagram_bytes(out, peek)?;
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: Some(drain.source),
                    unix_source: None,
                    packet_source: None,
                    destination: Some(drain.destination),
                    truncated: drain.truncated,
                    became_empty: drain.became_empty,
                    eor: false,
                    sctp_notification: false,
                    sctp_stream: 0,
                    sctp_ppid: 0,
                }
            }
            SocketImpl::Icmp(raw_icmp) => {
                let drain = raw_icmp.recv_bytes(out, peek)?;
                let source = match drain.source {
                    RawIpAddress::V4(addr) => IpEndpoint::new(addr, 0),
                    RawIpAddress::V6(addr) => IpEndpoint::new_v6(addr, 0),
                };
                let destination = match drain.destination {
                    RawIpAddress::V4(addr) => IpEndpoint::new(addr, 0),
                    RawIpAddress::V6(addr) => IpEndpoint::new_v6(addr, 0),
                };
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: Some(source),
                    unix_source: None,
                    packet_source: None,
                    destination: Some(destination),
                    truncated: drain.truncated,
                    became_empty: drain.became_empty,
                    eor: false,
                    sctp_notification: false,
                    sctp_stream: 0,
                    sctp_ppid: 0,
                }
            }
            SocketImpl::Unix(raw_unix) => {
                let drain = raw_unix.recv_bytes(out, peek, unix_stream)?;
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: None,
                    unix_source: drain.source,
                    packet_source: None,
                    destination: None,
                    truncated: drain.truncated,
                    became_empty: drain.became_empty,
                    eor: false,
                    sctp_notification: false,
                    sctp_stream: 0,
                    sctp_ppid: 0,
                }
            }
            SocketImpl::Rds(raw_rds) => {
                let drain = raw_rds.recv_packet_bytes(out, peek)?;
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: Some(drain.source),
                    unix_source: None,
                    packet_source: None,
                    destination: Some(drain.destination),
                    truncated: drain.truncated,
                    became_empty: drain.became_empty,
                    eor: false,
                    sctp_notification: false,
                    sctp_stream: 0,
                    sctp_ppid: 0,
                }
            }
            SocketImpl::Sctp(raw_sctp) => {
                let drain = raw_sctp.recv_message(out, peek)?;
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: drain.source,
                    unix_source: None,
                    packet_source: None,
                    destination: None,
                    truncated: false,
                    became_empty: drain.became_empty,
                    eor: drain.eor,
                    sctp_notification: drain.notification,
                    sctp_stream: drain.stream,
                    sctp_ppid: drain.ppid,
                }
            }
            _ => return None,
        };
        Some(outcome)
    }

    pub(crate) fn peek_recv_bytes(&self, len: usize) -> Option<usize> {
        self.raw_recv_len(len, true).map(|(bytes, _)| bytes)
    }

    pub fn udp_corked_send_len(&self) -> usize {
        self.imp.udp()
            .map(RawUdpSocket::corked_tx_len)
            .unwrap_or(0)
    }

    pub(crate) fn reserve_send_space(&self, len: usize) -> Option<SocketSendReserve> {
        let (bytes, became_full, needs_poll_kick) =
            match &self.imp {
                SocketImpl::Tcp(raw_tcp) => {
                    let reserve = raw_tcp.enqueue_tx_len(len)?;
                    (
                        reserve.bytes,
                        reserve.became_full,
                        reserve.flushed_to_protocol,
                    )
                }
                SocketImpl::Udp(raw_udp) => match self.udp_connected_remote() {
                    Some(dst) => {
                        raw_udp.set_tx_src_hint(self.udp_tx_src_hint(dst));
                        let (bytes, became_full) = raw_udp.enqueue_tx_len_to(dst, len)?;
                        (bytes, became_full, false)
                    }
                    None => {
                        let (bytes, became_full) = raw_udp.enqueue_tx_len(len)?;
                        (bytes, became_full, false)
                    }
                },
                _ => return None,
            };
        Some(SocketSendReserve {
            bytes,
            became_full,
            needs_poll_kick,
        })
    }

    pub(crate) fn reserve_send_bytes_with_flags(
        &self,
        bytes: &[u8],
        flags: super::types::SendRecvFlags,
    ) -> Result<Option<SocketSendReserve>, crate::execution::Errno> {
        let more = flags.contains(super::types::SendRecvFlags::MSG_MORE);
        let (bytes, became_full, needs_poll_kick) =
            match &self.imp {
                SocketImpl::Tcp(raw_tcp) => match raw_tcp.enqueue_tx_bytes_with_more(bytes, more) {
                    Some(reserve) => (
                        reserve.bytes,
                        reserve.became_full,
                        reserve.flushed_to_protocol,
                    ),
                    None => return Ok(None),
                },
                SocketImpl::Udp(raw_udp) => {
                    // A plain send() with no msg_name still needs a destination:
                    // use the connected peer, or fail EDESTADDRREQ like Linux (and
                    // like the sendto path in reserve_send_bytes_to_with_flags).
                    // The pre-refactor VecDeque accepted an unaddressable datagram
                    // and silently dropped it at drain; the smoltcp tx ring cannot
                    // stage an unspecified dst, so reject up front rather than
                    // report success for bytes that never leave (and leave the
                    // send-buffer accounting inconsistent).
                    let dst = match self.udp_connected_remote() {
                        Some(dst) => dst,
                        None => return Err(crate::execution::Errno::EDESTADDRREQ),
                    };
                    raw_udp.set_tx_src_hint(self.udp_tx_src_hint(dst));
                    match raw_udp.enqueue_tx_bytes_to_with_more(dst, bytes, more) {
                        Some((bytes, became_full)) => (bytes, became_full, false),
                        None => return Ok(None),
                    }
                }
                _ => return Ok(None),
            };
        Ok(Some(SocketSendReserve {
            bytes,
            became_full,
            needs_poll_kick,
        }))
    }

    pub(crate) fn reserve_send_bytes_to_with_flags(
        &self,
        dst: Option<IpEndpoint>,
        bytes: &[u8],
        flags: super::types::SendRecvFlags,
    ) -> Result<Option<SocketSendReserve>, crate::execution::Errno> {
        let more = flags.contains(super::types::SendRecvFlags::MSG_MORE);
        let (bytes, became_full, needs_poll_kick) =
            match &self.imp {
                SocketImpl::Tcp(raw_tcp) => {
                    match raw_tcp.enqueue_tx_bytes_with_more(bytes, more) {
                        Some(reserve) => (
                            reserve.bytes,
                            reserve.became_full,
                            reserve.flushed_to_protocol,
                        ),
                        None => return Ok(None),
                    }
                }
                SocketImpl::Udp(raw_udp) => {
                    let dst = match dst.or_else(|| self.udp_connected_remote()) {
                        Some(dst) => dst,
                        None => return Err(crate::execution::Errno::EDESTADDRREQ),
                    };
                    raw_udp.set_tx_src_hint(self.udp_tx_src_hint(dst));
                    match raw_udp.enqueue_tx_bytes_to_with_more(dst, bytes, more) {
                        Some((bytes, became_full)) => (bytes, became_full, false),
                        None => return Ok(None),
                    }
                }
                SocketImpl::Icmp(raw_icmp) => {
                    let dst = match dst {
                        Some(dst) => dst,
                        None => return Err(crate::execution::Errno::EDESTADDRREQ),
                    };
                    if dst.family != AddressFamily::Inet {
                        return Err(crate::execution::Errno::EAFNOSUPPORT);
                    }
                    let local = self.raw_icmp_bound_local().unwrap_or(Ipv4Address::LOOPBACK);
                    let event = match parse_icmpv4_payload(local, dst.addr, bytes) {
                        Icmpv4Event::Malformed if self.is_icmp_datagram_socket() => {
                            parse_icmpv4_echo_payload_unchecked(local, dst.addr, bytes)
                        }
                        Icmpv4Event::Malformed if self.is_raw_icmp_socket() => {
                            parse_raw_icmpv4_echo_payload_unchecked(local, dst.addr, bytes)
                        }
                        event => event,
                    };
                    let packet = match event {
                        Icmpv4Event::EchoRequest(packet) | Icmpv4Event::EchoReply(packet) => packet,
                        Icmpv4Event::Malformed => return Err(crate::execution::Errno::EINVAL),
                        Icmpv4Event::Unsupported => {
                            return Err(crate::execution::Errno::EOPNOTSUPP)
                        }
                    };
                    match raw_icmp.enqueue_tx_echo(packet) {
                        Some((bytes, became_full)) => (bytes, became_full, false),
                        None => return Ok(None),
                    }
                }
                _ => return Ok(None),
            };
        Ok(Some(SocketSendReserve {
            bytes,
            became_full,
            needs_poll_kick,
        }))
    }

    pub(crate) fn take_udp_tx_datagram(&self) -> Option<SocketUdpTxDrain> {
        let raw_udp = self.imp.udp()?;
        let drain = raw_udp.pop_tx_datagram()?;
        Some(SocketUdpTxDrain {
            datagram: drain.datagram,
            src: drain.src,
            became_available: drain.became_available,
        })
    }

    pub(crate) fn take_icmp_tx_echo(&self) -> Option<SocketIcmpTxDrain> {
        let raw_icmp = self.imp.icmp()?;
        let drain = raw_icmp.pop_tx_echo()?;
        Some(SocketIcmpTxDrain {
            packet: drain.packet,
            became_available: drain.became_available,
        })
    }

    pub(crate) fn peek_icmp_tx_echo(&self) -> Option<Icmpv4EchoPacket> {
        self.imp.icmp()?.peek_tx_echo()
    }

    pub(crate) fn commit_icmp_tx_echo_sent(&self) -> Option<SocketIcmpTxDrain> {
        let raw_icmp = self.imp.icmp()?;
        let drain = raw_icmp.commit_tx_echo_sent()?;
        Some(SocketIcmpTxDrain {
            packet: drain.packet,
            became_available: drain.became_available,
        })
    }

    // External v6 echo TX queue (mirror of the v4 icmp_tx_echo family above).
    pub(crate) fn enqueue_icmp6_tx_echo(
        &self,
        packet: crate::net::protocol::Icmpv6EchoPacket,
    ) -> Option<(usize, bool)> {
        self.imp.icmp()?.enqueue_tx6_echo(packet)
    }

    pub(crate) fn peek_icmp6_tx_echo(&self) -> Option<crate::net::protocol::Icmpv6EchoPacket> {
        self.imp.icmp()?.peek_tx6_echo()
    }

    /// Returns `became_available` after the sink accepted the head packet.
    pub(crate) fn commit_icmp6_tx_echo_sent(&self) -> Option<bool> {
        self.imp.icmp()?.commit_tx6_echo_sent()
    }

    pub(crate) fn record_icmp_recv_echo_reply(&self, packet: Icmpv4EchoPacket) -> bool {
        let became_readable = self.imp.icmp()
            .is_some_and(|raw_icmp| raw_icmp.ingest_rx_echo_reply(packet));
        became_readable
    }

    pub(crate) fn record_raw_ipv6_packet(&self, packet: RawIpv6Packet) -> bool {
        let became_readable = self.imp.icmp()
            .is_some_and(|raw_icmp| raw_icmp.ingest_rx_ipv6_packet(packet));
        became_readable
    }

    pub(crate) fn set_raw_icmp_protocol(
        &self,
        protocol: ProtocolNumber,
    ) -> Result<(), crate::execution::Errno> {
        self.with_protocol_mut(|socket_protocol| match socket_protocol {
            SocketProtocol::RawIcmp(state) => {
                state.protocol = protocol;
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub fn raw_icmp_protocol(&self) -> Option<ProtocolNumber> {
        match &*self.protocol.lock() {
            SocketProtocol::RawIcmp(state) => Some(state.protocol),
            _ => None,
        }
    }

    pub(crate) fn raw_icmp_bound_local6(&self) -> Option<Ipv6Address> {
        match &*self.protocol.lock() {
            SocketProtocol::RawIcmp(state) => state.bound_local6,
            _ => None,
        }
    }

    pub fn raw_icmp6_filter(&self) -> Option<[u32; 8]> {
        match &*self.protocol.lock() {
            SocketProtocol::RawIcmp(state) => Some(state.icmp6_filter),
            _ => None,
        }
    }

    pub fn set_raw_icmp6_filter(&self, filter: [u32; 8]) -> Result<(), crate::execution::Errno> {
        self.with_protocol_mut(|socket_protocol| match socket_protocol {
            SocketProtocol::RawIcmp(state) => {
                state.icmp6_filter = filter;
                true
            }
            _ => false,
        })
        .then_some(())
        .ok_or(crate::execution::Errno::EINVAL)
    }

    pub(crate) fn peek_udp_tx_datagram(&self) -> Option<UdpTxDatagram> {
        self.imp.udp()?.peek_tx_datagram()
    }

    pub(crate) fn set_accept_limit(&self, limit: usize) {
        self.tcp_backlog.lock().set_limit(limit);
    }

    pub(crate) fn enqueue_accept_entry(&self, entry: SocketAcceptEntry) -> Option<bool> {
        let mut backlog = self.tcp_backlog.lock();
        let was_empty = backlog.connected_is_empty();
        backlog.push_connected(entry)?;
        Some(was_empty)
    }

    pub(crate) fn enqueue_connecting_entry(&self, entry: TcpBacklogEntry) -> Option<()> {
        self.tcp_backlog.lock().push_connecting(entry)
    }

    pub(crate) fn cleanup_tcp_backlog(&self, now: Instant) -> (usize, usize, usize, usize) {
        self.tcp_backlog.lock().cleanup_connecting(now)
    }

    pub(crate) fn poll_tcp_backlog_retransmit(
        &self,
        now: Instant,
        retransmit: impl FnMut(&TcpBacklogEntry) -> bool,
    ) -> TcpBacklogRetransmitOutcome {
        self.tcp_backlog
            .lock()
            .poll_retransmit_connecting(now, retransmit)
    }

    pub(crate) fn connecting_child(
        &self,
        local: IpEndpoint,
        peer: IpEndpoint,
    ) -> Option<Cap<SocketIdentity>> {
        self.tcp_backlog.lock().connecting_child(local, peer)
    }

    /// Snapshot of every half-open child in the connecting backlog. The
    /// device-TX half-open lane drives their pending smoltcp segments
    /// (initial SYN-ACK + RTO retransmits) — they are not in the
    /// connections table until the final ACK promotes them (P2-S3).
    pub(crate) fn connecting_children(&self) -> Vec<Cap<SocketIdentity>> {
        self.tcp_backlog.lock().connecting_children()
    }

    /// P3-C S1 (R2b): drain BOTH backlog queues on listener close and
    /// return the `connected` (accept-ready) children — those were
    /// double-registered into a connections table at handshake time
    /// (step_connect.rs `insert_*_connection`/`insert_unix_stream_peer` +
    /// `enqueue_accept_entry`), so the caller must withdraw them from the
    /// right table (keyed by `(local, peer)` for TCP/SCTP, by `child.raw()`
    /// for UnixStream) to release the strong `Cap` that otherwise pins the
    /// child (and its ns) forever. `connecting` (half-open) children live
    /// only in the backlog Vec and are freed as the drained entries drop.
    pub(crate) fn drain_backlog_for_close(&self) -> Vec<SocketAcceptEntry> {
        let mut backlog = self.tcp_backlog.lock();
        let mut connected = Vec::new();
        while let Some(entry) = backlog.pop_connected() {
            connected.push(entry);
        }
        backlog.clear_connecting();
        connected
    }

    pub(crate) fn promote_connecting_to_accept(
        &self,
        local: IpEndpoint,
        peer: IpEndpoint,
    ) -> Option<bool> {
        let mut backlog = self.tcp_backlog.lock();
        let was_empty = backlog.connected_is_empty();
        backlog.promote_connecting(local, peer)?;
        Some(was_empty)
    }

    pub(crate) fn pop_accept_entry(&self) -> Option<SocketAcceptPop> {
        let mut backlog = self.tcp_backlog.lock();
        let entry = backlog.pop_connected()?;
        let became_empty = backlog.connected_is_empty();
        Some(SocketAcceptPop {
            entry,
            became_empty,
        })
    }

    fn raw_recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if let Some(raw_packet) = self.imp.packet() {
            return raw_packet.recv_len(len, peek);
        }
        let unix_stream = self.with_protocol(|protocol| {
            matches!(
                protocol,
                SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
            )
        });
        match &self.imp {
            SocketImpl::Tcp(raw_tcp) => raw_tcp
                .recv_len(len, peek)
                .or_else(|| raw_tcp.is_recv_closed().then_some((0, false))),
            SocketImpl::Udp(raw_udp) => {
                raw_udp.recv_len(len, peek)
            }
            SocketImpl::Icmp(raw_icmp) => {
                raw_icmp.recv_len(len, peek)
            }
            SocketImpl::Unix(raw_unix) => {
                raw_unix.recv_len(len, peek, unix_stream)
            }
            SocketImpl::Rds(raw_rds) => {
                raw_rds.recv_len(len, peek)
            }
            SocketImpl::Sctp(raw_sctp) => {
                raw_sctp.recv_len(len, peek)
            }
            SocketImpl::NetlinkRoute(raw_netlink) => {
                raw_netlink.recv_len(len)
            }
            SocketImpl::NetlinkNetfilter(raw_netlink) => {
                raw_netlink.recv_len(len)
            }
            _ => None,
        }
    }

    fn raw_recv_available(&self) -> usize {
        if let Some(raw_packet) = self.imp.packet() {
            return raw_packet.recv_available();
        }
        let unix_stream = self.with_protocol(|protocol| {
            matches!(
                protocol,
                SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
            )
        });
        match &self.imp {
            SocketImpl::Tcp(raw_tcp) => raw_tcp.recv_available(),
            SocketImpl::Udp(raw_udp) => raw_udp.recv_available(),
            SocketImpl::Icmp(raw_icmp) => raw_icmp.recv_available(),
            SocketImpl::Unix(raw_unix) => {
                raw_unix.recv_available(unix_stream)
            }
            SocketImpl::Rds(raw_rds) => raw_rds.recv_available(),
            SocketImpl::Sctp(raw_sctp) => raw_sctp.recv_available(),
            SocketImpl::NetlinkRoute(raw_netlink) => {
                raw_netlink.recv_available()
            }
            SocketImpl::NetlinkNetfilter(raw_netlink) => {
                raw_netlink.recv_available()
            }
            _ => 0,
        }
    }

    fn raw_send_available(&self) -> usize {
        if let Some(raw_packet) = self.imp.packet() {
            return raw_packet.send_available();
        }
        match &self.imp {
            SocketImpl::Tcp(raw_tcp) => raw_tcp.send_available(),
            SocketImpl::Udp(raw_udp) => raw_udp.send_available(),
            SocketImpl::Icmp(raw_icmp) => raw_icmp.send_available(),
            SocketImpl::Unix(raw_unix) => raw_unix.send_available(),
            SocketImpl::Rds(raw_rds) => raw_rds.send_available(),
            SocketImpl::Sctp(raw_sctp) => raw_sctp.send_available(),
            SocketImpl::NetlinkRoute(raw_netlink) => {
                raw_netlink.send_available()
            }
            SocketImpl::NetlinkNetfilter(raw_netlink) => {
                raw_netlink.send_available()
            }
            _ => 0,
        }
    }

    pub(crate) fn raw_icmp_bound_local(&self) -> Option<Ipv4Address> {
        match &*self.protocol.lock() {
            SocketProtocol::RawIcmp(state) => state.bound_local,
            _ => None,
        }
    }

    fn is_icmp_datagram_socket(&self) -> bool {
        self.with_options(|options| options.socket.sock_type == SocketType::Dgram)
    }

    fn is_raw_icmp_socket(&self) -> bool {
        self.with_options(|options| options.socket.sock_type == SocketType::Raw)
    }

    fn udp_connected_remote(&self) -> Option<IpEndpoint> {
        match &*self.protocol.lock() {
            SocketProtocol::Udp(UdpInner::Connected { remote, .. }) => Some(remote),
            _ => None,
        }
        .copied()
    }

    fn udp_bound_local(&self) -> Option<IpEndpoint> {
        match &*self.protocol.lock() {
            SocketProtocol::Udp(UdpInner::Bound { local })
            | SocketProtocol::Udp(UdpInner::Connected { local, .. }) => Some(*local),
            _ => None,
        }
    }

    /// Resolve the source address to stamp on outgoing UDP datagrams
    /// (P2-S6). The context iface carries no addresses, so smoltcp's
    /// dispatch-side source selection cannot be relied on: use the bound
    /// address when concrete, the loopback rule for loopback-destined
    /// datagrams, else the namespace route's preferred source.
    fn udp_tx_src_hint(&self, dst: IpEndpoint) -> Option<IpEndpoint> {
        if let Some(local) = self.udp_bound_local() {
            if !local.is_unspecified() {
                return Some(local);
            }
        }
        if dst.is_loopback() || dst.is_unspecified() {
            return Some(IpEndpoint::loopback_for_family(dst.family, 0));
        }
        match dst.ip_addr() {
            super::IpAddress::V4(addr) => self
                .net_namespace()
                .best_ipv4_route(addr)
                .and_then(|route| {
                    route.preferred_src.or_else(|| {
                        self.net_namespace()
                            .link_snapshot()
                            .into_iter()
                            .find(|link| {
                                link.name == route.oif_name
                                    && link.is_up
                                    && !link.is_loopback
                                    && link.ipv4_addr.is_some()
                            })
                            .and_then(|link| link.ipv4_addr)
                    })
                })
                .map(|src| IpEndpoint::new(src, 0)),
            super::IpAddress::V6(_) => None,
        }
    }

}

const fn default_family_for_kind(kind: SocketKind) -> AddressFamily {
    match kind {
        SocketKind::UnixDatagram | SocketKind::UnixStream => AddressFamily::Unix,
        SocketKind::Tcp | SocketKind::Udp | SocketKind::Sctp | SocketKind::RawIcmp => {
            AddressFamily::Inet
        }
        SocketKind::NetlinkRoute | SocketKind::NetlinkXfrm | SocketKind::NetlinkNetfilter => {
            AddressFamily::Netlink
        }
        SocketKind::Packet => AddressFamily::Packet,
        SocketKind::RdsSeqPacket => AddressFamily::Rds,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketIoState {
    pub recv_len: usize,
    pub send_space: usize,
    pub accept_pending: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnixPeerCred {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketIoConsume {
    pub bytes: usize,
    pub became_empty: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketRecvBytesOutcome {
    pub bytes: usize,
    pub source: Option<IpEndpoint>,
    pub unix_source: Option<UnixSocketPath>,
    pub packet_source: Option<SockAddrLl>,
    pub destination: Option<IpEndpoint>,
    pub truncated: bool,
    pub became_empty: bool,
    /// End-of-record: the read consumed a complete message (SCTP message
    /// boundary). Maps to `MSG_EOR` in recvmsg. Always false for byte streams.
    pub eor: bool,
    /// The delivered message is an SCTP control notification (`MSG_NOTIFICATION`).
    pub sctp_notification: bool,
    /// sctp_sndrcvinfo stream id / payload protocol id for an SCTP data message.
    pub sctp_stream: u16,
    pub sctp_ppid: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketSendReserve {
    pub bytes: usize,
    pub became_full: bool,
    pub needs_poll_kick: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketUdpTxDrain {
    pub datagram: UdpTxDatagram,
    /// Dispatch-resolved source endpoint (see `UdpTxDatagramDrain::src`).
    pub src: IpEndpoint,
    pub became_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketIcmpTxDrain {
    pub packet: Icmpv4EchoPacket,
    pub became_available: bool,
}

pub(crate) struct RawPacketSocket {
    state: SpinMutex<RawPacketState>,
    recv_limit: usize,
    send_space: usize,
}

impl RawPacketSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        Self {
            state: SpinMutex::new(RawPacketState::new()),
            recv_limit: options.socket.recv_buf_size.max(1),
            send_space: options.socket.send_buf_size.max(1),
        }
    }

    pub fn ingest_frame(&self, source: SockAddrLl, bytes: Vec<u8>) -> Option<bool> {
        self.state
            .lock()
            .push_frame(PacketFrame { source, bytes }, self.recv_limit)
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        self.state.lock().recv_len(len, peek)
    }

    pub fn recv_frame_bytes(&self, out: &mut [u8], peek: bool) -> Option<PacketRecvDrain> {
        self.state.lock().recv_frame_bytes(out, peek)
    }

    pub fn recv_available(&self) -> usize {
        self.state.lock().recv_available()
    }

    pub const fn send_available(&self) -> usize {
        self.send_space
    }
}

struct RawPacketState {
    frames: Vec<PacketFrame>,
    queued_bytes: usize,
}

impl RawPacketState {
    const fn new() -> Self {
        Self {
            frames: Vec::new(),
            queued_bytes: 0,
        }
    }

    fn push_frame(&mut self, frame: PacketFrame, recv_limit: usize) -> Option<bool> {
        let was_empty = self.frames.is_empty();
        let len = frame.bytes.len();
        if self.queued_bytes.saturating_add(len) > recv_limit {
            return None;
        }
        self.queued_bytes += len;
        self.frames.push(frame);
        Some(was_empty)
    }

    fn recv_len(&mut self, len: usize, peek: bool) -> Option<(usize, bool)> {
        let frame_len = self.frames.first()?.bytes.len();
        let bytes = core::cmp::min(len, frame_len);
        if !peek {
            let frame = self.frames.remove(0);
            self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
        }
        Some((bytes, !peek && self.frames.is_empty()))
    }

    fn recv_frame_bytes(&mut self, out: &mut [u8], peek: bool) -> Option<PacketRecvDrain> {
        let frame = self.frames.first()?;
        let bytes = core::cmp::min(out.len(), frame.bytes.len());
        out[..bytes].copy_from_slice(&frame.bytes[..bytes]);
        let source = frame.source;
        let truncated = frame.bytes.len() > out.len();
        if !peek {
            let frame = self.frames.remove(0);
            self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
        }
        Some(PacketRecvDrain {
            bytes,
            source,
            truncated,
            became_empty: !peek && self.frames.is_empty(),
        })
    }

    fn recv_available(&self) -> usize {
        self.frames.first().map_or(0, |frame| frame.bytes.len())
    }
}

struct PacketFrame {
    source: SockAddrLl,
    bytes: Vec<u8>,
}

pub(crate) struct PacketRecvDrain {
    pub bytes: usize,
    pub source: SockAddrLl,
    pub truncated: bool,
    pub became_empty: bool,
}

pub(crate) struct RawRdsSocket {
    state: SpinMutex<RawRdsState>,
    recv_limit: usize,
    send_space: usize,
}

impl RawRdsSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        Self {
            state: SpinMutex::new(RawRdsState::new()),
            recv_limit: options.socket.recv_buf_size.max(1),
            send_space: options.socket.send_buf_size.max(1),
        }
    }

    pub fn ingest_packet(
        &self,
        source: IpEndpoint,
        destination: IpEndpoint,
        bytes: Vec<u8>,
    ) -> Option<bool> {
        self.state.lock().push_frame(
            RdsFrame {
                source,
                destination,
                bytes,
            },
            self.recv_limit,
        )
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        self.state.lock().recv_len(len, peek)
    }

    pub fn recv_packet_bytes(&self, out: &mut [u8], peek: bool) -> Option<RdsRecvDrain> {
        self.state.lock().recv_packet_bytes(out, peek)
    }

    pub fn recv_available(&self) -> usize {
        self.state.lock().recv_available()
    }

    pub const fn send_available(&self) -> usize {
        self.send_space
    }
}

struct RawRdsState {
    frames: Vec<RdsFrame>,
    queued_bytes: usize,
}

impl RawRdsState {
    const fn new() -> Self {
        Self {
            frames: Vec::new(),
            queued_bytes: 0,
        }
    }

    fn push_frame(&mut self, frame: RdsFrame, recv_limit: usize) -> Option<bool> {
        let was_empty = self.frames.is_empty();
        let len = frame.bytes.len();
        if self.queued_bytes.saturating_add(len) > recv_limit {
            return None;
        }
        self.queued_bytes += len;
        self.frames.push(frame);
        Some(was_empty)
    }

    fn recv_len(&mut self, len: usize, peek: bool) -> Option<(usize, bool)> {
        let frame_len = self.frames.first()?.bytes.len();
        let bytes = core::cmp::min(len, frame_len);
        if !peek {
            let frame = self.frames.remove(0);
            self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
        }
        Some((bytes, !peek && self.frames.is_empty()))
    }

    fn recv_packet_bytes(&mut self, out: &mut [u8], peek: bool) -> Option<RdsRecvDrain> {
        let frame = self.frames.first()?;
        let bytes = core::cmp::min(out.len(), frame.bytes.len());
        out[..bytes].copy_from_slice(&frame.bytes[..bytes]);
        let source = frame.source;
        let destination = frame.destination;
        let truncated = frame.bytes.len() > out.len();
        if !peek {
            let frame = self.frames.remove(0);
            self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
        }
        Some(RdsRecvDrain {
            bytes,
            source,
            destination,
            truncated,
            became_empty: !peek && self.frames.is_empty(),
        })
    }

    fn recv_available(&self) -> usize {
        self.frames.first().map_or(0, |frame| frame.bytes.len())
    }
}

struct RdsFrame {
    source: IpEndpoint,
    destination: IpEndpoint,
    bytes: Vec<u8>,
}

pub(crate) struct RdsRecvDrain {
    pub bytes: usize,
    pub source: IpEndpoint,
    pub destination: IpEndpoint,
    pub truncated: bool,
    pub became_empty: bool,
}

/// One queued SCTP message (boundary-preserved) plus its ancillary metadata.
/// `notification` marks a control event (assoc_change / shutdown) that recvmsg
/// surfaces with MSG_NOTIFICATION; `stream`/`ppid` carry the sctp_sndrcvinfo;
/// `source` is the sender's endpoint for 1-to-many recvmsg msg_name (None for
/// 1-to-1, where there is a single fixed peer).
#[derive(Clone)]
struct SctpFrame {
    data: Vec<u8>,
    notification: bool,
    stream: u16,
    ppid: u32,
    source: Option<IpEndpoint>,
}

/// A 1-to-many (SEQPACKET) association tracked on a socket: the peer endpoint and
/// the locally-assigned association id.
#[derive(Clone, Copy)]
pub(crate) struct SctpAssoc {
    pub peer: IpEndpoint,
    pub assoc_id: u32,
}

pub(crate) struct RawSctpSocket {
    state: SpinMutex<RawSctpState>,
    recv_limit: usize,
    send_space: usize,
}

impl RawSctpSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        Self {
            state: SpinMutex::new(RawSctpState::new()),
            recv_limit: options.socket.recv_buf_size.max(1),
            send_space: options.socket.send_buf_size.max(1),
        }
    }

    pub fn ingest_message(
        &self,
        bytes: Vec<u8>,
        notification: bool,
        stream: u16,
        ppid: u32,
        source: Option<IpEndpoint>,
    ) -> Option<bool> {
        self.state
            .lock()
            .push_message(bytes, notification, stream, ppid, source, self.recv_limit)
    }

    pub fn ensure_assoc(&self, peer: IpEndpoint) -> (u32, bool) {
        self.state.lock().ensure_assoc(peer)
    }

    pub fn remove_assoc(&self, assoc_id: u32) -> Option<IpEndpoint> {
        self.state.lock().remove_assoc(assoc_id)
    }

    pub fn add_local_addr(&self, endpoint: IpEndpoint) {
        self.state.lock().add_local_addr(endpoint);
    }

    pub fn local_addrs(&self) -> Vec<IpEndpoint> {
        self.state.lock().local_addrs()
    }

    pub fn peers_snapshot(&self) -> Vec<SctpAssoc> {
        self.state.lock().peers_snapshot()
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        self.state.lock().recv_len(len, peek)
    }

    pub fn recv_message(&self, out: &mut [u8], peek: bool) -> Option<SctpRecvDrain> {
        self.state.lock().recv_message(out, peek)
    }

    pub fn recv_available(&self) -> usize {
        self.state.lock().recv_available()
    }

    pub const fn send_available(&self) -> usize {
        self.send_space
    }
}

struct RawSctpState {
    frames: Vec<SctpFrame>,
    queued_bytes: usize,
    /// 1-to-many peer associations (SEQPACKET). Empty for 1-to-1 sockets.
    peers: Vec<SctpAssoc>,
    /// All local addresses this socket is bound to (primary bind + sctp_bindx),
    /// used to report the full multi-homed address set to peers via getpaddrs.
    local_addrs: Vec<IpEndpoint>,
}

/// Association ids are drawn from a process-global monotonic counter so that
/// ids are unique across sockets: a peer's association id never coincides with
/// this socket's, which the SCTP API tests rely on when probing an "incorrect"
/// association id from the other end.
static NEXT_SCTP_ASSOC_ID: AtomicU32 = AtomicU32::new(1);

impl RawSctpState {
    const fn new() -> Self {
        Self {
            frames: Vec::new(),
            queued_bytes: 0,
            peers: Vec::new(),
            local_addrs: Vec::new(),
        }
    }

    fn add_local_addr(&mut self, endpoint: IpEndpoint) {
        if !self.local_addrs.contains(&endpoint) {
            self.local_addrs.push(endpoint);
        }
    }

    fn local_addrs(&self) -> Vec<IpEndpoint> {
        self.local_addrs.clone()
    }

    fn push_message(
        &mut self,
        bytes: Vec<u8>,
        notification: bool,
        stream: u16,
        ppid: u32,
        source: Option<IpEndpoint>,
        recv_limit: usize,
    ) -> Option<bool> {
        let was_empty = self.queued_bytes == 0;
        if self.queued_bytes.saturating_add(bytes.len()) > recv_limit {
            return None;
        }
        self.queued_bytes += bytes.len();
        self.frames.push(SctpFrame {
            data: bytes,
            notification,
            stream,
            ppid,
            source,
        });
        Some(was_empty)
    }

    /// Find an existing 1-to-many association to `peer`, or create one. Returns
    /// (assoc_id, is_new).
    fn ensure_assoc(&mut self, peer: IpEndpoint) -> (u32, bool) {
        if let Some(assoc) = self.peers.iter().find(|a| a.peer == peer) {
            return (assoc.assoc_id, false);
        }
        let assoc_id = NEXT_SCTP_ASSOC_ID.fetch_add(1, Ordering::Relaxed).max(1);
        self.peers.push(SctpAssoc { peer, assoc_id });
        (assoc_id, true)
    }

    /// Remove the association named by `assoc_id`, returning its peer endpoint if
    /// it existed.
    fn remove_assoc(&mut self, assoc_id: u32) -> Option<IpEndpoint> {
        let idx = self.peers.iter().position(|a| a.assoc_id == assoc_id)?;
        Some(self.peers.remove(idx).peer)
    }

    fn peers_snapshot(&self) -> Vec<SctpAssoc> {
        self.peers.clone()
    }

    fn recv_len(&mut self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if self.queued_bytes == 0 {
            return None;
        }
        let bytes = core::cmp::min(len, self.queued_bytes);
        if !peek {
            self.drop_front_bytes(bytes)?;
        }
        Some((bytes, !peek && self.queued_bytes == 0))
    }

    /// Message-oriented receive: deliver bytes from the FRONT message only
    /// (SCTP preserves message boundaries). `eor` is set when the whole front
    /// message fit in `out`; otherwise the remainder stays queued for the next
    /// recv and `eor` is false (partial delivery).
    fn recv_message(&mut self, out: &mut [u8], peek: bool) -> Option<SctpRecvDrain> {
        let front = self.frames.first()?;
        let front_len = front.data.len();
        let notification = front.notification;
        let stream = front.stream;
        let ppid = front.ppid;
        let source = front.source;
        let n = core::cmp::min(out.len(), front_len);
        out[..n].copy_from_slice(&front.data[..n]);
        let eor = n == front_len;
        if !peek {
            self.drop_front_bytes(n)?;
        }
        Some(SctpRecvDrain {
            bytes: n,
            became_empty: !peek && self.queued_bytes == 0,
            eor,
            notification,
            stream,
            ppid,
            source,
        })
    }

    fn recv_available(&self) -> usize {
        self.queued_bytes
    }

    fn drop_front_bytes(&mut self, mut remaining: usize) -> Option<()> {
        while remaining > 0 {
            let front_len = self.frames.first()?.data.len();
            if front_len <= remaining {
                let frame = self.frames.remove(0);
                self.queued_bytes = self.queued_bytes.saturating_sub(frame.data.len());
                remaining -= frame.data.len();
            } else {
                let front = self.frames.first_mut()?;
                front.data.drain(..remaining);
                self.queued_bytes = self.queued_bytes.saturating_sub(remaining);
                remaining = 0;
            }
        }
        Some(())
    }
}

pub(crate) struct SctpRecvDrain {
    pub bytes: usize,
    pub became_empty: bool,
    /// The returned bytes completed a whole SCTP message (set `MSG_EOR`).
    pub eor: bool,
    /// The message is a control notification (set `MSG_NOTIFICATION`).
    pub notification: bool,
    /// sctp_sndrcvinfo stream id / payload protocol id for the message.
    pub stream: u16,
    pub ppid: u32,
    /// Sender endpoint for 1-to-many recvmsg msg_name (None for 1-to-1).
    pub source: Option<IpEndpoint>,
}

pub(crate) struct RawUnixSocket {
    state: SpinMutex<RawUnixState>,
    recv_limit: usize,
    send_space: usize,
}

impl RawUnixSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        Self {
            state: SpinMutex::new(RawUnixState::new()),
            recv_limit: options.socket.recv_buf_size.max(1),
            send_space: options.socket.send_buf_size.max(1),
        }
    }

    pub fn ingest_datagram(&self, source: Option<UnixSocketPath>, bytes: Vec<u8>) -> Option<bool> {
        self.state
            .lock()
            .push_frame(UnixFrame { source, bytes }, self.recv_limit)
    }

    pub fn ingest_stream_bytes(&self, bytes: Vec<u8>) -> Option<bool> {
        self.state.lock().push_frame(
            UnixFrame {
                source: None,
                bytes,
            },
            self.recv_limit,
        )
    }

    pub fn recv_len(&self, len: usize, peek: bool, stream: bool) -> Option<(usize, bool)> {
        self.state.lock().recv_len(len, peek, stream)
    }

    pub fn recv_bytes(&self, out: &mut [u8], peek: bool, stream: bool) -> Option<UnixRecvDrain> {
        self.state.lock().recv_bytes(out, peek, stream)
    }

    pub fn recv_available(&self, stream: bool) -> usize {
        self.state.lock().recv_available(stream)
    }

    pub const fn send_available(&self) -> usize {
        self.send_space
    }
}

struct RawUnixState {
    frames: Vec<UnixFrame>,
    queued_bytes: usize,
}

impl RawUnixState {
    const fn new() -> Self {
        Self {
            frames: Vec::new(),
            queued_bytes: 0,
        }
    }

    fn push_frame(&mut self, frame: UnixFrame, recv_limit: usize) -> Option<bool> {
        let was_empty = self.frames.is_empty();
        let len = frame.bytes.len();
        if self.queued_bytes.saturating_add(len) > recv_limit {
            return None;
        }
        self.queued_bytes += len;
        self.frames.push(frame);
        Some(was_empty)
    }

    fn recv_len(&mut self, len: usize, peek: bool, stream: bool) -> Option<(usize, bool)> {
        if stream {
            return self.recv_stream_len(len, peek);
        }
        let frame_len = self.frames.first()?.bytes.len();
        let bytes = core::cmp::min(len, frame_len);
        if !peek {
            let frame = self.frames.remove(0);
            self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
        }
        Some((bytes, self.frames.is_empty()))
    }

    fn recv_bytes(&mut self, out: &mut [u8], peek: bool, stream: bool) -> Option<UnixRecvDrain> {
        if stream {
            return self.recv_stream_bytes(out, peek);
        }
        let frame = self.frames.first()?;
        let bytes = core::cmp::min(out.len(), frame.bytes.len());
        out[..bytes].copy_from_slice(&frame.bytes[..bytes]);
        let source = frame.source;
        let truncated = frame.bytes.len() > out.len();
        if !peek {
            let frame = self.frames.remove(0);
            self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
        }
        Some(UnixRecvDrain {
            bytes,
            source,
            truncated,
            became_empty: self.frames.is_empty(),
        })
    }

    fn recv_available(&self, stream: bool) -> usize {
        if stream {
            self.queued_bytes
        } else {
            self.frames.first().map_or(0, |frame| frame.bytes.len())
        }
    }

    fn recv_stream_len(&mut self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if self.queued_bytes == 0 {
            return None;
        }
        let mut remaining = core::cmp::min(len, self.queued_bytes);
        let bytes = remaining;
        if !peek {
            while remaining > 0 {
                let front_len = self.frames.first()?.bytes.len();
                if front_len <= remaining {
                    let frame = self.frames.remove(0);
                    self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
                    remaining -= frame.bytes.len();
                } else {
                    let front = self.frames.first_mut()?;
                    front.bytes.drain(..remaining);
                    self.queued_bytes = self.queued_bytes.saturating_sub(remaining);
                    remaining = 0;
                }
            }
        }
        Some((bytes, self.queued_bytes == 0))
    }

    fn recv_stream_bytes(&mut self, out: &mut [u8], peek: bool) -> Option<UnixRecvDrain> {
        if self.queued_bytes == 0 {
            return None;
        }
        let mut copied = 0usize;
        for frame in &self.frames {
            if copied >= out.len() {
                break;
            }
            let n = core::cmp::min(out.len() - copied, frame.bytes.len());
            out[copied..copied + n].copy_from_slice(&frame.bytes[..n]);
            copied += n;
        }
        if !peek {
            let mut remaining = copied;
            while remaining > 0 {
                let front_len = self.frames.first()?.bytes.len();
                if front_len <= remaining {
                    let frame = self.frames.remove(0);
                    self.queued_bytes = self.queued_bytes.saturating_sub(frame.bytes.len());
                    remaining -= frame.bytes.len();
                } else {
                    let front = self.frames.first_mut()?;
                    front.bytes.drain(..remaining);
                    self.queued_bytes = self.queued_bytes.saturating_sub(remaining);
                    remaining = 0;
                }
            }
        }
        Some(UnixRecvDrain {
            bytes: copied,
            source: None,
            truncated: false,
            became_empty: self.queued_bytes == 0,
        })
    }
}

struct UnixFrame {
    source: Option<UnixSocketPath>,
    bytes: Vec<u8>,
}

pub(crate) struct UnixRecvDrain {
    pub bytes: usize,
    pub source: Option<UnixSocketPath>,
    pub truncated: bool,
    pub became_empty: bool,
}

#[derive(Clone)]
pub struct SocketAcceptEntry {
    pub child: Cap<SocketIdentity>,
    pub local: IpEndpoint,
    pub peer: IpEndpoint,
    pub unix_peer: Option<UnixSocketPath>,
}

#[derive(Clone)]
pub struct TcpBacklogEntry {
    pub child: Cap<SocketIdentity>,
    pub local: IpEndpoint,
    pub peer: IpEndpoint,
    pub created_at: Instant,
    pub deadline: Instant,
    pub attempts: u8,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TcpBacklogRetransmitOutcome {
    pub scanned: usize,
    pub retransmitted: usize,
    pub expired: usize,
    pub failed: usize,
    pub remaining_connecting: usize,
    pub next_deadline: Option<Instant>,
}

pub(crate) struct SocketAcceptPop {
    pub entry: SocketAcceptEntry,
    pub became_empty: bool,
}

pub struct TcpBacklog {
    connecting: Vec<TcpBacklogEntry>,
    connected: SocketAcceptQueue,
    limit: usize,
}

impl TcpBacklog {
    pub const fn new() -> Self {
        Self {
            connecting: Vec::new(),
            connected: SocketAcceptQueue::new(),
            limit: 0,
        }
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        self.connected.set_limit(limit);
    }

    pub fn connecting_len(&self) -> usize {
        self.connecting.len()
    }

    pub fn connected_len(&self) -> usize {
        self.connected.len()
    }

    pub fn connected_is_empty(&self) -> bool {
        self.connected.is_empty()
    }

    pub fn push_connecting(&mut self, entry: TcpBacklogEntry) -> Option<()> {
        if self.is_full() || self.find_connecting(entry.local, entry.peer).is_some() {
            return None;
        }
        self.connecting.push(entry);
        Some(())
    }

    pub fn connecting_child(
        &self,
        local: IpEndpoint,
        peer: IpEndpoint,
    ) -> Option<Cap<SocketIdentity>> {
        self.find_connecting(local, peer)
            .map(|index| self.connecting[index].child.clone())
    }

    pub fn connecting_children(&self) -> Vec<Cap<SocketIdentity>> {
        self.connecting
            .iter()
            .map(|entry| entry.child.clone())
            .collect()
    }

    /// P3-C S1 (R2b): drop every half-open child on listener close. These
    /// are not in the connections table (promoted only on final ACK), so
    /// clearing the Vec releases their `Cap` directly.
    pub fn clear_connecting(&mut self) {
        self.connecting.clear();
    }

    pub fn cleanup_connecting(&mut self, now: Instant) -> (usize, usize, usize, usize) {
        let scanned = self.connecting.len();
        let mut expired = 0;
        let mut failed = 0;
        let mut kept = Vec::with_capacity(self.connecting.len());

        for entry in self.connecting.drain(..) {
            if entry.deadline <= now {
                expired += 1;
            } else if connecting_entry_failed(&entry) {
                failed += 1;
            } else {
                kept.push(entry);
            }
        }

        self.connecting = kept;
        (scanned, expired, failed, self.connecting.len())
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.connecting.iter().map(|entry| entry.deadline).min()
    }

    pub fn poll_retransmit_connecting(
        &mut self,
        now: Instant,
        mut retransmit: impl FnMut(&TcpBacklogEntry) -> bool,
    ) -> TcpBacklogRetransmitOutcome {
        let mut outcome = TcpBacklogRetransmitOutcome {
            scanned: self.connecting.len(),
            ..TcpBacklogRetransmitOutcome::default()
        };
        let mut kept = Vec::with_capacity(self.connecting.len());

        for mut entry in self.connecting.drain(..) {
            if connecting_entry_failed(&entry) {
                outcome.failed += 1;
                continue;
            }

            if entry.deadline <= now {
                if entry.attempts >= TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING {
                    outcome.expired += 1;
                    continue;
                }

                if !retransmit(&entry) {
                    outcome.failed += 1;
                    continue;
                }

                entry.attempts = entry.attempts.saturating_add(1);
                entry.deadline =
                    now + Duration::from_millis(TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS as u64);
                outcome.retransmitted += 1;
            }

            outcome.next_deadline = earliest_deadline(outcome.next_deadline, entry.deadline);
            kept.push(entry);
        }

        self.connecting = kept;
        outcome.remaining_connecting = self.connecting.len();
        outcome
    }

    pub fn promote_connecting(&mut self, local: IpEndpoint, peer: IpEndpoint) -> Option<()> {
        let index = self.find_connecting(local, peer)?;
        let entry = self.connecting.remove(index);
        self.push_connected(SocketAcceptEntry {
            child: entry.child,
            local: entry.local,
            peer: entry.peer,
            unix_peer: None,
        })?;
        Some(())
    }

    pub fn push_connected(&mut self, entry: SocketAcceptEntry) -> Option<()> {
        if self.is_full() || !self.connected.push(entry) {
            return None;
        }
        Some(())
    }

    pub fn pop_connected(&mut self) -> Option<SocketAcceptEntry> {
        self.connected.pop_front()
    }

    fn is_full(&self) -> bool {
        // Linux semantics: the accept queue is full when the pending count
        // EXCEEDS the backlog (`sk_ack_backlog > sk_max_ack_backlog`), so a
        // listen(N) admits N+1 pending connections. `limit == 0` means the
        // socket is not listening, so it accepts nothing.
        self.limit == 0 || self.connecting.len() + self.connected.len() > self.limit
    }

    fn find_connecting(&self, local: IpEndpoint, peer: IpEndpoint) -> Option<usize> {
        self.connecting
            .iter()
            .position(|entry| entry.local == local && entry.peer == peer)
    }
}

fn connecting_entry_failed(entry: &TcpBacklogEntry) -> bool {
    let Some(payload) = entry.child.acquire_operational() else {
        return true;
    };
    let Some(raw_tcp) = payload.raw_tcp_socket() else {
        return true;
    };

    raw_tcp.protocol_runtime_state().is_rst_closed || raw_tcp.protocol_state() == tcp::State::Closed
}

fn earliest_deadline(current: Option<Instant>, candidate: Instant) -> Option<Instant> {
    Some(current.map_or(candidate, |current| current.min(candidate)))
}

impl Default for TcpBacklog {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SocketAcceptQueue {
    entries: Vec<SocketAcceptEntry>,
    limit: usize,
}

impl SocketAcceptQueue {
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            limit: 0,
        }
    }

    pub fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, entry: SocketAcceptEntry) -> bool {
        // Matches `TcpBacklog::is_full`: a listen(N) accept queue holds N+1
        // entries (Linux `sk_ack_backlog > sk_max_ack_backlog`). Only reached
        // when the backlog is not full, so `limit` is always > 0 here.
        if self.entries.len() > self.limit {
            return false;
        }
        self.entries.push(entry);
        true
    }

    pub fn pop_front(&mut self) -> Option<SocketAcceptEntry> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.entries.remove(0))
        }
    }
}

impl Default for SocketAcceptQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ShutdownMark {
    pub recv: bool,
    pub send: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SocketProtocol {
    UnixDatagram(UnixDatagramState),
    UnixStream(UnixStreamState),
    Tcp(TcpState),
    Udp(UdpInner),
    Sctp(TcpState),
    Rds(RdsState),
    RawIcmp(RawIcmpState),
    NetlinkRoute(NetlinkRouteState),
    NetlinkNetfilter(NetlinkNetfilterState),
    Packet(PacketSocketState),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnixDatagramState {
    Unbound,
    Bound {
        local: UnixSocketPath,
    },
    Connected {
        local: Option<UnixSocketPath>,
        peer: UnixSocketPath,
    },
    ConnectedPair {
        peer_raw: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnixStreamState {
    Init,
    Bound {
        local: UnixSocketPath,
    },
    Listening {
        local: UnixSocketPath,
        backlog_limit: usize,
    },
    Connected {
        local: Option<UnixSocketPath>,
        peer_raw: u32,
    },
    Closed,
}

pub struct Takeable<T> {
    value: Option<T>,
}

impl<T> Takeable<T> {
    pub fn new(value: T) -> Self {
        Self { value: Some(value) }
    }

    pub fn take(&mut self) -> T {
        self.value.take().expect("takeable value present")
    }

    pub fn put(&mut self, value: T) {
        assert!(self.value.is_none());
        self.value = Some(value);
    }
}

impl<T> AsRef<T> for Takeable<T> {
    fn as_ref(&self) -> &T {
        self.value.as_ref().expect("takeable value present")
    }
}

impl<T> AsMut<T> for Takeable<T> {
    fn as_mut(&mut self) -> &mut T {
        self.value.as_mut().expect("takeable value present")
    }
}
