use core::sync::atomic::{AtomicBool, Ordering};

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
    parse_icmpv4_payload, Icmpv4EchoPacket, Icmpv4Event, RawIcmpSocket, RawTcpSocket, RawUdpSocket,
    UdpTxDatagram,
};
use super::identity::SocketIdentity;
use super::multicast::{Ipv4MulticastGroup, Ipv4MulticastMemberships};
use super::types::{
    AddressFamily, IpEndpoint, Ipv4Address, PacketSocketState, ProtocolNumber, RawIcmpState,
    SockAddrLl, SockShutdownCmd, SocketKind, SocketOptionSet, TcpState, UdpInner, UnixSocketPath,
};

pub type SocketOperationalEvidence = PayloadCap<SocketPayload>;
pub const TCP_BACKLOG_TIMEOUT_STAGING_MILLIS: i64 = 30_000;
pub const TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING: u8 = 3;
pub const TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS: i64 = 1_000;

pub struct SocketPayload {
    pub(crate) family: SpinMutex<AddressFamily>,
    pub(crate) net_namespace: PayloadCap<NetNamespacePayload>,
    pub(crate) protocol: SpinMutex<SocketProtocol>,
    pub(crate) options: SpinMutex<SocketOptionSet>,
    pub(crate) ip_multicast: SpinMutex<Ipv4MulticastMemberships>,
    pub(crate) raw_tcp: Option<RawTcpSocket>,
    pub(crate) raw_udp: Option<RawUdpSocket>,
    pub(crate) raw_icmp: Option<RawIcmpSocket>,
    pub(crate) raw_unix: Option<RawUnixSocket>,
    pub(crate) raw_netlink_route: Option<RawNetlinkRouteSocket>,
    pub(crate) raw_netlink_netfilter: Option<RawNetlinkNetfilterSocket>,
    pub(crate) io: SpinMutex<SocketIoState>,
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
        let (
            protocol,
            raw_tcp,
            raw_udp,
            raw_icmp,
            raw_unix,
            raw_netlink_route,
            raw_netlink_netfilter,
        ) = match kind {
            SocketKind::UnixDatagram => (
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound),
                None,
                None,
                None,
                Some(RawUnixSocket::new(&options)),
                None,
                None,
            ),
            SocketKind::UnixStream => (
                SocketProtocol::UnixStream(UnixStreamState::Init),
                None,
                None,
                None,
                Some(RawUnixSocket::new(&options)),
                None,
                None,
            ),
            SocketKind::Tcp => (
                SocketProtocol::Tcp(TcpState::Init),
                Some(RawTcpSocket::new(&options)),
                None,
                None,
                None,
                None,
                None,
            ),
            SocketKind::Udp => (
                SocketProtocol::Udp(UdpInner::Unbound),
                None,
                Some(RawUdpSocket::new(&options)),
                None,
                None,
                None,
                None,
            ),
            SocketKind::RawIcmp => (
                SocketProtocol::RawIcmp(RawIcmpState::new(ProtocolNumber(1))),
                None,
                None,
                Some(RawIcmpSocket::new(&options)),
                None,
                None,
                None,
            ),
            SocketKind::NetlinkRoute => (
                SocketProtocol::NetlinkRoute(NetlinkRouteState),
                None,
                None,
                None,
                None,
                Some(RawNetlinkRouteSocket::new()),
                None,
            ),
            SocketKind::NetlinkNetfilter => (
                SocketProtocol::NetlinkNetfilter(NetlinkNetfilterState),
                None,
                None,
                None,
                None,
                None,
                Some(RawNetlinkNetfilterSocket::new()),
            ),
            SocketKind::Packet => (
                SocketProtocol::Packet(PacketSocketState::new(0)),
                None,
                None,
                None,
                None,
                None,
                None,
            ),
        };
        let payload = Self {
            family: SpinMutex::new(family),
            net_namespace,
            protocol: SpinMutex::new(protocol),
            options: SpinMutex::new(options),
            ip_multicast: SpinMutex::new(Ipv4MulticastMemberships::empty()),
            raw_tcp,
            raw_udp,
            raw_icmp,
            raw_unix,
            raw_netlink_route,
            raw_netlink_netfilter,
            io: SpinMutex::new(SocketIoState::new()),
            unix_peer_cred: SpinMutex::new(None),
            tcp_backlog: SpinMutex::new(TcpBacklog::new()),
            shutdown_rd: AtomicBool::new(false),
            shutdown_wr: AtomicBool::new(false),
        };
        payload.refresh_io_from_raw();
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

    pub fn io_snapshot(&self) -> SocketIoState {
        *self.io.lock()
    }

    pub fn unix_peer_cred(&self) -> Option<UnixPeerCred> {
        *self.unix_peer_cred.lock()
    }

    pub fn set_unix_peer_cred(&self, cred: UnixPeerCred) {
        *self.unix_peer_cred.lock() = Some(cred);
    }

    pub fn raw_tcp_socket(&self) -> Option<&RawTcpSocket> {
        self.raw_tcp.as_ref()
    }

    pub fn reset_raw_tcp_socket(&self) -> Result<(), crate::execution::Errno> {
        let Some(raw_tcp) = self.raw_tcp.as_ref() else {
            return Err(crate::execution::Errno::EOPNOTSUPP);
        };
        self.with_options(|options| raw_tcp.reset(options));
        self.refresh_io_from_raw();
        Ok(())
    }

    pub fn tcp_recv_closed_by_peer(&self) -> bool {
        self.raw_tcp
            .as_ref()
            .is_some_and(RawTcpSocket::is_recv_closed)
    }

    pub fn raw_udp_socket(&self) -> Option<&RawUdpSocket> {
        self.raw_udp.as_ref()
    }

    pub fn raw_icmp_socket(&self) -> Option<&RawIcmpSocket> {
        self.raw_icmp.as_ref()
    }

    pub(crate) fn raw_netlink_route_socket(&self) -> Option<&RawNetlinkRouteSocket> {
        self.raw_netlink_route.as_ref()
    }

    pub(crate) fn raw_netlink_netfilter_socket(&self) -> Option<&RawNetlinkNetfilterSocket> {
        self.raw_netlink_netfilter.as_ref()
    }

    pub(crate) fn record_unix_datagram(
        &self,
        source: Option<UnixSocketPath>,
        payload: Vec<u8>,
    ) -> Option<bool> {
        let raw_unix = self.raw_unix.as_ref()?;
        let became_readable = raw_unix.ingest_datagram(source, payload)?;
        self.refresh_io_from_raw();
        Some(became_readable)
    }

    pub(crate) fn record_unix_stream_bytes(&self, payload: Vec<u8>) -> Option<bool> {
        let raw_unix = self.raw_unix.as_ref()?;
        let became_readable = raw_unix.ingest_stream_bytes(payload)?;
        self.refresh_io_from_raw();
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
        let became_readable = match (&self.raw_tcp, &self.raw_udp) {
            (Some(raw_tcp), None) => raw_tcp.ingest_rx_bytes(&payload),
            (None, Some(raw_udp)) => raw_udp.ingest_rx_datagram(src, dst, payload),
            _ => false,
        };
        self.refresh_io_from_raw();
        became_readable
    }

    pub(crate) fn record_send_space(&self, bytes: usize) -> bool {
        let became_available = match &self.raw_tcp {
            Some(raw_tcp) => raw_tcp.ack_tx_bytes(bytes),
            None => false,
        };
        self.refresh_io_from_raw();
        became_available
    }

    pub(crate) fn consume_recv_bytes(&self, len: usize) -> Option<SocketIoConsume> {
        let (bytes, became_empty) = self.raw_recv_len(len, false)?;
        self.refresh_io_from_raw();
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
        let unix_stream = self.with_protocol(|protocol| {
            matches!(
                protocol,
                SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
            )
        });
        let outcome = match (&self.raw_tcp, &self.raw_udp, &self.raw_icmp, &self.raw_unix) {
            (Some(raw_tcp), None, None, None) => match raw_tcp.recv_bytes(out, peek) {
                Some((bytes, became_empty)) => SocketRecvBytesOutcome {
                    bytes,
                    source: None,
                    unix_source: None,
                    destination: None,
                    truncated: false,
                    became_empty,
                },
                None if raw_tcp.is_recv_closed() => SocketRecvBytesOutcome {
                    bytes: 0,
                    source: None,
                    unix_source: None,
                    destination: None,
                    truncated: false,
                    became_empty: false,
                },
                None => return None,
            },
            (None, Some(raw_udp), None, None) => {
                let drain = raw_udp.recv_datagram_bytes(out, peek)?;
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: Some(drain.source),
                    unix_source: None,
                    destination: Some(drain.destination),
                    truncated: drain.truncated,
                    became_empty: drain.became_empty,
                }
            }
            (None, None, Some(raw_icmp), None) => {
                let drain = raw_icmp.recv_echo_reply_bytes(out, peek)?;
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: Some(IpEndpoint::new(drain.source, 0)),
                    unix_source: None,
                    destination: Some(IpEndpoint::new(drain.destination, 0)),
                    truncated: drain.truncated,
                    became_empty: drain.became_empty,
                }
            }
            (None, None, None, Some(raw_unix)) => {
                let drain = raw_unix.recv_bytes(out, peek, unix_stream)?;
                SocketRecvBytesOutcome {
                    bytes: drain.bytes,
                    source: None,
                    unix_source: drain.source,
                    destination: None,
                    truncated: drain.truncated,
                    became_empty: drain.became_empty,
                }
            }
            _ => return None,
        };
        self.refresh_io_from_raw();
        Some(outcome)
    }

    pub(crate) fn peek_recv_bytes(&self, len: usize) -> Option<usize> {
        self.raw_recv_len(len, true).map(|(bytes, _)| bytes)
    }

    pub(crate) fn udp_corked_send_len(&self) -> usize {
        self.raw_udp
            .as_ref()
            .map(RawUdpSocket::corked_tx_len)
            .unwrap_or(0)
    }

    pub(crate) fn reserve_send_space(&self, len: usize) -> Option<SocketSendReserve> {
        let (bytes, became_full, needs_poll_kick) =
            match (&self.raw_tcp, &self.raw_udp, &self.raw_icmp) {
                (Some(raw_tcp), None, None) => {
                    let reserve = raw_tcp.enqueue_tx_len(len)?;
                    (
                        reserve.bytes,
                        reserve.became_full,
                        reserve.flushed_to_protocol,
                    )
                }
                (None, Some(raw_udp), None) => match self.udp_connected_remote() {
                    Some(dst) => {
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
        self.refresh_io_from_raw();
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
    ) -> Option<SocketSendReserve> {
        let more = flags.contains(super::types::SendRecvFlags::MSG_MORE);
        let (bytes, became_full, needs_poll_kick) =
            match (&self.raw_tcp, &self.raw_udp, &self.raw_icmp) {
                (Some(raw_tcp), None, None) => {
                    let reserve = raw_tcp.enqueue_tx_bytes_with_more(bytes, more)?;
                    (
                        reserve.bytes,
                        reserve.became_full,
                        reserve.flushed_to_protocol,
                    )
                }
                (None, Some(raw_udp), None) => match self.udp_connected_remote() {
                    Some(dst) => {
                        let (bytes, became_full) =
                            raw_udp.enqueue_tx_bytes_to_with_more(dst, bytes, more)?;
                        (bytes, became_full, false)
                    }
                    None => {
                        let (bytes, became_full) = raw_udp.enqueue_tx_bytes_to_with_more(
                            IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0),
                            bytes,
                            more,
                        )?;
                        (bytes, became_full, false)
                    }
                },
                _ => return None,
            };
        self.refresh_io_from_raw();
        Some(SocketSendReserve {
            bytes,
            became_full,
            needs_poll_kick,
        })
    }

    pub(crate) fn reserve_send_bytes_to_with_flags(
        &self,
        dst: Option<IpEndpoint>,
        bytes: &[u8],
        flags: super::types::SendRecvFlags,
    ) -> Result<Option<SocketSendReserve>, crate::execution::Errno> {
        let more = flags.contains(super::types::SendRecvFlags::MSG_MORE);
        let (bytes, became_full, needs_poll_kick) =
            match (&self.raw_tcp, &self.raw_udp, &self.raw_icmp) {
                (Some(raw_tcp), None, None) => {
                    match raw_tcp.enqueue_tx_bytes_with_more(bytes, more) {
                        Some(reserve) => (
                            reserve.bytes,
                            reserve.became_full,
                            reserve.flushed_to_protocol,
                        ),
                        None => return Ok(None),
                    }
                }
                (None, Some(raw_udp), None) => {
                    let dst = match dst.or_else(|| self.udp_connected_remote()) {
                        Some(dst) => dst,
                        None => return Err(crate::execution::Errno::EDESTADDRREQ),
                    };
                    match raw_udp.enqueue_tx_bytes_to_with_more(dst, bytes, more) {
                        Some((bytes, became_full)) => (bytes, became_full, false),
                        None => return Ok(None),
                    }
                }
                (None, None, Some(raw_icmp)) => {
                    let dst = match dst {
                        Some(dst) => dst,
                        None => return Err(crate::execution::Errno::EDESTADDRREQ),
                    };
                    let local = self.raw_icmp_bound_local().unwrap_or(Ipv4Address::LOOPBACK);
                    let packet = match parse_icmpv4_payload(local, dst.addr, bytes) {
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
        self.refresh_io_from_raw();
        Ok(Some(SocketSendReserve {
            bytes,
            became_full,
            needs_poll_kick,
        }))
    }

    pub(crate) fn take_tcp_tx_bytes(&self, max_len: usize) -> Option<SocketTxDrain> {
        let raw_tcp = self.raw_tcp.as_ref()?;
        let (bytes, became_available) = raw_tcp.dequeue_tx_bytes(max_len)?;
        self.refresh_io_from_raw();
        Some(SocketTxDrain {
            bytes,
            became_available,
        })
    }

    pub(crate) fn take_udp_tx_datagram(&self) -> Option<SocketUdpTxDrain> {
        let raw_udp = self.raw_udp.as_ref()?;
        let drain = raw_udp.pop_tx_datagram()?;
        self.refresh_io_from_raw();
        Some(SocketUdpTxDrain {
            datagram: drain.datagram,
            became_available: drain.became_available,
        })
    }

    pub(crate) fn take_icmp_tx_echo(&self) -> Option<SocketIcmpTxDrain> {
        let raw_icmp = self.raw_icmp.as_ref()?;
        let drain = raw_icmp.pop_tx_echo()?;
        self.refresh_io_from_raw();
        Some(SocketIcmpTxDrain {
            packet: drain.packet,
            became_available: drain.became_available,
        })
    }

    pub(crate) fn peek_icmp_tx_echo(&self) -> Option<Icmpv4EchoPacket> {
        self.raw_icmp.as_ref()?.peek_tx_echo()
    }

    pub(crate) fn commit_icmp_tx_echo_sent(&self) -> Option<SocketIcmpTxDrain> {
        let raw_icmp = self.raw_icmp.as_ref()?;
        let drain = raw_icmp.commit_tx_echo_sent()?;
        self.refresh_io_from_raw();
        Some(SocketIcmpTxDrain {
            packet: drain.packet,
            became_available: drain.became_available,
        })
    }

    pub(crate) fn record_icmp_recv_echo_reply(&self, packet: Icmpv4EchoPacket) -> bool {
        let became_readable = self
            .raw_icmp
            .as_ref()
            .is_some_and(|raw_icmp| raw_icmp.ingest_rx_echo_reply(packet));
        self.refresh_io_from_raw();
        became_readable
    }

    pub(crate) fn peek_udp_tx_datagram(&self) -> Option<UdpTxDatagram> {
        self.raw_udp.as_ref()?.peek_tx_datagram()
    }

    pub(crate) fn commit_udp_tx_datagram_sent(&self) -> Option<SocketUdpTxDrain> {
        let raw_udp = self.raw_udp.as_ref()?;
        let drain = raw_udp.commit_tx_datagram_sent()?;
        self.refresh_io_from_raw();
        Some(SocketUdpTxDrain {
            datagram: drain.datagram,
            became_available: drain.became_available,
        })
    }

    pub(crate) fn set_accept_limit(&self, limit: usize) {
        self.tcp_backlog.lock().set_limit(limit);
    }

    pub(crate) fn enqueue_accept_entry(&self, entry: SocketAcceptEntry) -> Option<bool> {
        let mut backlog = self.tcp_backlog.lock();
        let was_empty = backlog.connected_is_empty();
        backlog.push_connected(entry)?;
        self.io.lock().accept_pending = backlog.connected_len();
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

    pub(crate) fn promote_connecting_to_accept(
        &self,
        local: IpEndpoint,
        peer: IpEndpoint,
    ) -> Option<bool> {
        let mut backlog = self.tcp_backlog.lock();
        let was_empty = backlog.connected_is_empty();
        backlog.promote_connecting(local, peer)?;
        self.io.lock().accept_pending = backlog.connected_len();
        Some(was_empty)
    }

    pub(crate) fn pop_accept_entry(&self) -> Option<SocketAcceptPop> {
        let mut backlog = self.tcp_backlog.lock();
        let entry = backlog.pop_connected()?;
        let became_empty = backlog.connected_is_empty();
        self.io.lock().accept_pending = backlog.connected_len();
        Some(SocketAcceptPop {
            entry,
            became_empty,
        })
    }

    fn raw_recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        let unix_stream = self.with_protocol(|protocol| {
            matches!(
                protocol,
                SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
            )
        });
        match (
            &self.raw_tcp,
            &self.raw_udp,
            &self.raw_icmp,
            &self.raw_unix,
            &self.raw_netlink_route,
            &self.raw_netlink_netfilter,
        ) {
            (Some(raw_tcp), None, None, None, None, None) => raw_tcp
                .recv_len(len, peek)
                .or_else(|| raw_tcp.is_recv_closed().then_some((0, false))),
            (None, Some(raw_udp), None, None, None, None) => raw_udp.recv_len(len, peek),
            (None, None, Some(raw_icmp), None, None, None) => raw_icmp.recv_len(len, peek),
            (None, None, None, Some(raw_unix), None, None) => {
                raw_unix.recv_len(len, peek, unix_stream)
            }
            (None, None, None, None, Some(raw_netlink), None) => raw_netlink.recv_len(len),
            (None, None, None, None, None, Some(raw_netlink)) => raw_netlink.recv_len(len),
            _ => None,
        }
    }

    fn raw_recv_available(&self) -> usize {
        let unix_stream = self.with_protocol(|protocol| {
            matches!(
                protocol,
                SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
            )
        });
        match (
            &self.raw_tcp,
            &self.raw_udp,
            &self.raw_icmp,
            &self.raw_unix,
            &self.raw_netlink_route,
            &self.raw_netlink_netfilter,
        ) {
            (Some(raw_tcp), None, None, None, None, None) => raw_tcp.recv_available(),
            (None, Some(raw_udp), None, None, None, None) => raw_udp.recv_available(),
            (None, None, Some(raw_icmp), None, None, None) => raw_icmp.recv_available(),
            (None, None, None, Some(raw_unix), None, None) => raw_unix.recv_available(unix_stream),
            (None, None, None, None, Some(raw_netlink), None) => raw_netlink.recv_available(),
            (None, None, None, None, None, Some(raw_netlink)) => raw_netlink.recv_available(),
            _ => 0,
        }
    }

    fn raw_send_available(&self) -> usize {
        match (
            &self.raw_tcp,
            &self.raw_udp,
            &self.raw_icmp,
            &self.raw_unix,
            &self.raw_netlink_route,
            &self.raw_netlink_netfilter,
        ) {
            (Some(raw_tcp), None, None, None, None, None) => raw_tcp.send_available(),
            (None, Some(raw_udp), None, None, None, None) => raw_udp.send_available(),
            (None, None, Some(raw_icmp), None, None, None) => raw_icmp.send_available(),
            (None, None, None, Some(raw_unix), None, None) => raw_unix.send_available(),
            (None, None, None, None, Some(raw_netlink), None) => raw_netlink.send_available(),
            (None, None, None, None, None, Some(raw_netlink)) => raw_netlink.send_available(),
            _ => 0,
        }
    }

    fn raw_icmp_bound_local(&self) -> Option<Ipv4Address> {
        match &*self.protocol.lock() {
            SocketProtocol::RawIcmp(state) => state.bound_local,
            _ => None,
        }
    }

    fn udp_connected_remote(&self) -> Option<IpEndpoint> {
        match &*self.protocol.lock() {
            SocketProtocol::Udp(UdpInner::Connected { remote, .. }) => Some(remote),
            _ => None,
        }
        .copied()
    }

    pub(crate) fn refresh_io_from_raw(&self) {
        let recv_len = self.raw_recv_available();
        let send_space = self.raw_send_available();
        let mut io = self.io.lock();
        io.recv_len = recv_len;
        io.send_space = send_space;
    }
}

const fn default_family_for_kind(kind: SocketKind) -> AddressFamily {
    match kind {
        SocketKind::UnixDatagram | SocketKind::UnixStream => AddressFamily::Unix,
        SocketKind::Tcp | SocketKind::Udp | SocketKind::RawIcmp => AddressFamily::Inet,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter => AddressFamily::Netlink,
        SocketKind::Packet => AddressFamily::Packet,
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketIoState {
    pub recv_len: usize,
    pub send_space: usize,
    pub accept_pending: usize,
}

impl SocketIoState {
    pub const fn new() -> Self {
        Self {
            recv_len: 0,
            send_space: 0,
            accept_pending: 0,
        }
    }
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
    pub destination: Option<IpEndpoint>,
    pub truncated: bool,
    pub became_empty: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketSendReserve {
    pub bytes: usize,
    pub became_full: bool,
    pub needs_poll_kick: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketTxDrain {
    pub bytes: Vec<u8>,
    pub became_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketUdpTxDrain {
    pub datagram: UdpTxDatagram,
    pub became_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SocketIcmpTxDrain {
    pub packet: Icmpv4EchoPacket,
    pub became_available: bool,
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
        self.limit == 0 || self.connecting.len() + self.connected.len() >= self.limit
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
        if self.entries.len() >= self.limit {
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
