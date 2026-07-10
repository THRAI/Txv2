use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::phy::{ChecksumCapabilities, PacketMeta};
use smoltcp::socket::udp;
use smoltcp::wire::{
    IpAddress, IpListenEndpoint, IpProtocol, IpRepr, Ipv4Packet, Ipv4Repr, Ipv6Packet, Ipv6Repr,
    UdpPacket, UdpRepr,
};

use super::tcp::with_context;
use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::{
    AddressFamily, IpEndpoint, Ipv4Address, Ipv6Address as TxIpv6Address, SocketOptionSet,
};
use crate::sync::SpinMutex;

const MAX_UDP_PACKET_METADATA_CAPACITY: usize = 64;
/// Upper bound on the smoltcp ring backing (P2-S6): the ring IS the
/// datagram queue now, so it must track the socket buffer size — but a
/// default SO_SNDBUF/SO_RCVBUF of ~208 KiB per direction per socket would
/// be a real allocation (audit R2d), so cap it.
///
/// **Hard floor = one whole datagram.** The ring must hold at least one
/// maximum-size UDP datagram (`UDP_IPV4_MAX_PAYLOAD_BYTES` = 65507); a cap
/// below that cannot store even a single large datagram intact, so smoltcp
/// truncates/drops it and the receiver reads garbage (iperf3's default UDP
/// len is 65495 → its BASIC/REVERSE UDP tests corrupt at a 32 KiB cap).
/// Matches TCP's `TCP_SMOLTCP_BACKING_MAX_BYTES` (64 KiB) — the correctness
/// minimum for holding one datagram. (Raising it holds more back-to-back
/// datagrams → less UDP loss under a fast sender, at more per-socket memory:
/// the R2d tradeoff.) NOTE: the primary large-datagram corruption cause was
/// the 4 KiB `read`/`write` syscall cap (`TTY_WRITE_MAX_INLINE`) shredding
/// datagrams *before* they reached this ring; this floor only lets the
/// (now-intact) large datagram be stored.
const UDP_SMOLTCP_BACKING_MAX_BYTES: usize = 65_536;
const UDP_PACKET_CAPACITY_DIVISOR: usize = 1500;
pub const UDP_IPV4_MAX_PAYLOAD_BYTES: usize = u16::MAX as usize - 20 - 8;

/// Doc-named owner for the smoltcp UDP socket and its packet buffers.
///
/// P2-S6: the smoltcp socket IS the data path — RX lands via
/// `accepts`/`process`, TX leaves via `send_slice`/`dispatch`/`peek_send`.
/// The former shadow `VecDeque` datagram queues are gone; only the
/// MSG_MORE corking staging survives outside smoltcp (same shape as TCP's
/// `corked_tx`).
/// P3-B S2 (D4): socket + corking + src-hint under ONE lock — the
/// capacity check and the actual `send_slice` become a composite atomic
/// (same R1d family as TCP). Lock order unchanged: CONTEXT_IFACE outer,
/// `inner` inner.
struct UdpInnerState {
    socket: Box<udp::Socket<'static>>,
    corked_tx: Option<UdpTxDatagram>,
    /// Source-address hint for corked/queued datagrams (resolved by the
    /// payload layer at enqueue time: bound address, loopback rule, or the
    /// namespace route's preferred source). The context iface carries no
    /// addresses, so dispatch-side source selection cannot be relied on.
    tx_src_hint: Option<IpAddress>,
}

pub struct RawUdpSocket {
    inner: SpinMutex<UdpInnerState>,
    recv_capacity: usize,
    send_capacity: usize,
    recv_packet_capacity: usize,
    send_packet_capacity: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UdpRxDatagram {
    pub src: IpEndpoint,
    pub dst: IpEndpoint,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UdpTxDatagram {
    pub dst: IpEndpoint,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UdpTxDatagramDrain {
    pub datagram: UdpTxDatagram,
    /// Source endpoint smoltcp resolved at dispatch (bound address or the
    /// enqueue-time hint). The emit path must use this — the socket may be
    /// bound to 0.0.0.0 and a src-unspecified wire packet is garbage.
    pub src: IpEndpoint,
    pub became_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UdpRecvDrain {
    pub bytes: usize,
    pub source: IpEndpoint,
    pub destination: IpEndpoint,
    pub truncated: bool,
    pub became_empty: bool,
}

impl RawUdpSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        // Capacity == ring size (P2-S6): the smoltcp ring is the queue, so
        // availability arithmetic must match what the ring can hold.
        let recv_capacity = smoltcp_backing_bytes(options.socket.recv_buf_size);
        let send_capacity = smoltcp_backing_bytes(options.socket.send_buf_size);
        let recv_packet_capacity = packet_capacity_for_bytes(recv_capacity);
        let send_packet_capacity = packet_capacity_for_bytes(send_capacity);
        let rx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; recv_packet_capacity],
            vec![0u8; smoltcp_backing_bytes(recv_capacity)],
        );
        let tx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; send_packet_capacity],
            vec![0u8; smoltcp_backing_bytes(send_capacity)],
        );
        let mut socket = udp::Socket::new(rx_buf, tx_buf);

        if options.ip.ttl != 0 {
            socket.set_hop_limit(Some(options.ip.ttl));
        }

        Self {
            inner: SpinMutex::new(UdpInnerState {
                socket: Box::new(socket),
                corked_tx: None,
                tx_src_hint: None,
            }),
            recv_capacity,
            send_capacity,
            recv_packet_capacity,
            send_packet_capacity,
        }
    }

    /// Bind the smoltcp socket so `accepts`/`process` admit inbound
    /// datagrams and `send` becomes addressable. Idempotent on the same
    /// port; a rebind to a different port closes and rebinds.
    pub fn bind_endpoint(&self, local: IpEndpoint) -> bool {
        if local.port == 0 {
            return false;
        }
        let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
        let current = socket.endpoint();
        if current.port == local.port {
            return true;
        }
        if current.port != 0 {
            socket.close();
        }
        let listen = IpListenEndpoint {
            addr: if local.is_unspecified() {
                None
            } else {
                Some(to_smol_ip(&local))
            },
            port: local.port,
        };
        socket.bind(listen).is_ok()
    }

    pub fn recv_capacity(&self) -> usize {
        self.recv_capacity
    }

    pub fn send_capacity(&self) -> usize {
        self.send_capacity
    }

    pub fn ingest_rx_datagram(&self, src: IpEndpoint, dst: IpEndpoint, payload: Vec<u8>) -> bool {
        if payload.is_empty() {
            return false;
        }

        with_context(|cx| {
            let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
            let was_empty = !socket.can_recv();
            let udp_repr = UdpRepr {
                src_port: src.port,
                dst_port: dst.port,
            };
            let ip_repr = ip_repr_for(&src, &dst, udp_repr.header_len() + payload.len());
            if !socket.accepts(cx, &ip_repr, &udp_repr) {
                return false;
            }
            socket.process(cx, PacketMeta::default(), &ip_repr, &udp_repr, &payload);
            // Edge semantics as before: report only the empty→non-empty
            // transition (a full ring drops the datagram inside process,
            // in which case can_recv stays false and we report false).
            was_empty && socket.can_recv()
        })
    }

    pub fn recv_available(&self) -> usize {
        self.inner.lock().socket.payload_recv_bytes()
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if len == 0 {
            return Some((0, false));
        }

        let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
        if peek {
            let (payload, _meta) = socket.peek().ok()?;
            return Some((core::cmp::min(payload.len(), len), false));
        }
        let (payload, _meta) = socket.recv().ok()?;
        let bytes = core::cmp::min(payload.len(), len);
        let became_empty = !socket.can_recv();
        Some((bytes, became_empty))
    }

    pub fn recv_datagram_bytes(&self, out: &mut [u8], peek: bool) -> Option<UdpRecvDrain> {
        if out.is_empty() {
            return Some(UdpRecvDrain {
                bytes: 0,
                source: unspecified_endpoint(),
                destination: unspecified_endpoint(),
                truncated: false,
                became_empty: false,
            });
        }

        let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
        let bound_port = socket.endpoint().port;
        if peek {
            let (payload, meta) = socket.peek().ok()?;
            let bytes = core::cmp::min(payload.len(), out.len());
            out[..bytes].copy_from_slice(&payload[..bytes]);
            let truncated = bytes < payload.len();
            let source = from_smol_endpoint(meta.endpoint);
            let destination = meta
                .local_address
                .map(|addr| endpoint_from_smol_ip(addr, bound_port))
                .unwrap_or_else(unspecified_endpoint);
            return Some(UdpRecvDrain {
                bytes,
                source,
                destination,
                truncated,
                became_empty: false,
            });
        }

        let (bytes, source, destination, truncated) = {
            let (payload, meta) = socket.recv().ok()?;
            let bytes = core::cmp::min(payload.len(), out.len());
            out[..bytes].copy_from_slice(&payload[..bytes]);
            (
                bytes,
                from_smol_endpoint(meta.endpoint),
                meta.local_address
                    .map(|addr| endpoint_from_smol_ip(addr, bound_port))
                    .unwrap_or_else(unspecified_endpoint),
                bytes < payload.len(),
            )
        };
        Some(UdpRecvDrain {
            bytes,
            source,
            destination,
            truncated,
            became_empty: !socket.can_recv(),
        })
    }

    pub fn send_available(&self) -> usize {
        let inner = self.inner.lock();
        udp_send_available_inner(&inner, self.send_capacity)
    }

    pub fn corked_tx_len(&self) -> usize {
        self.inner
            .lock()
            .corked_tx
            .as_ref()
            .map(|datagram| datagram.payload.len())
            .unwrap_or(0)
    }

    pub fn enqueue_tx_len(&self, len: usize) -> Option<(usize, bool)> {
        self.enqueue_tx_len_to(unspecified_endpoint(), len)
    }

    pub fn enqueue_tx_len_to(&self, dst: IpEndpoint, len: usize) -> Option<(usize, bool)> {
        let bytes = vec![0u8; len];
        self.enqueue_tx_datagram(dst, bytes)
    }

    pub fn enqueue_tx_datagram(&self, dst: IpEndpoint, payload: Vec<u8>) -> Option<(usize, bool)> {
        self.enqueue_tx_datagram_with_more(dst, payload, false)
    }

    /// Record the source-address hint the payload layer resolved for
    /// outgoing datagrams (bound address / loopback rule / route
    /// preferred-src). Consulted when flushing into the smoltcp tx ring.
    pub fn set_tx_src_hint(&self, src: Option<IpEndpoint>) {
        self.inner.lock().tx_src_hint = src
            .filter(|endpoint| !endpoint.is_unspecified())
            .map(|endpoint| to_smol_ip(&endpoint));
    }

    pub fn enqueue_tx_datagram_with_more(
        &self,
        dst: IpEndpoint,
        payload: Vec<u8>,
        more: bool,
    ) -> Option<(usize, bool)> {
        if payload.is_empty() {
            return Some((0, false));
        }

        // D4 composite atomic: capacity check, corking and the actual
        // send_slice all under one lock acquisition.
        let inner = &mut *self.inner.lock();
        let available = udp_send_available_inner(inner, self.send_capacity);
        if payload.len() > available {
            return None;
        }

        let bytes = payload.len();
        if more {
            match inner.corked_tx.as_mut() {
                Some(datagram) => {
                    datagram.payload.extend(payload);
                    if datagram.dst.port == 0 {
                        datagram.dst = dst;
                    }
                }
                None => {
                    inner.corked_tx = Some(UdpTxDatagram { dst, payload });
                }
            }
        } else {
            let flushed = if let Some(mut datagram) = inner.corked_tx.take() {
                if datagram.dst.port == 0 {
                    datagram.dst = dst;
                }
                datagram.payload.extend(payload);
                datagram
            } else {
                UdpTxDatagram { dst, payload }
            };
            push_datagram_inner(inner, flushed)?;
        }
        Some((
            bytes,
            udp_send_available_inner(inner, self.send_capacity) == 0,
        ))
    }

    pub fn enqueue_tx_bytes(&self, bytes: &[u8]) -> Option<(usize, bool)> {
        self.enqueue_tx_datagram(unspecified_endpoint(), bytes.to_vec())
    }

    pub fn enqueue_tx_bytes_to(&self, dst: IpEndpoint, bytes: &[u8]) -> Option<(usize, bool)> {
        self.enqueue_tx_datagram(dst, bytes.to_vec())
    }

    pub fn enqueue_tx_bytes_to_with_more(
        &self,
        dst: IpEndpoint,
        bytes: &[u8],
        more: bool,
    ) -> Option<(usize, bool)> {
        self.enqueue_tx_datagram_with_more(dst, bytes.to_vec(), more)
    }

    pub fn pop_tx_datagram(&self) -> Option<UdpTxDatagramDrain> {
        with_context(|cx| {
            let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
            let mut out = None;
            let result: Result<(), ()> =
                socket.dispatch(cx, |_cx, _meta, (ip_repr, udp_repr, payload)| {
                    out = Some((
                        UdpTxDatagram {
                            dst: endpoint_from_smol_ip(ip_repr.dst_addr(), udp_repr.dst_port),
                            payload: payload.to_vec(),
                        },
                        endpoint_from_smol_ip(ip_repr.src_addr(), udp_repr.src_port),
                    ));
                    Ok(())
                });
            result.ok()?;
            out.map(|(datagram, src)| UdpTxDatagramDrain {
                datagram,
                src,
                became_available: true,
            })
        })
    }

    pub fn peek_tx_datagram(&self) -> Option<UdpTxDatagram> {
        let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
        let (payload, meta) = socket.peek_send().ok()?;
        Some(UdpTxDatagram {
            dst: from_smol_endpoint(meta.endpoint),
            payload: payload.to_vec(),
        })
    }

    pub fn recv_packet_capacity(&self) -> usize {
        self.recv_packet_capacity
    }

    pub fn send_packet_capacity(&self) -> usize {
        self.send_packet_capacity
    }

    pub fn can_recv(&self) -> bool {
        self.inner.lock().socket.can_recv()
    }

    pub fn can_send(&self) -> bool {
        self.inner.lock().socket.can_send()
    }

    pub fn close(&self) {
        // Drain both rings so queued payloads are released (queue-era
        // `close` cleared the VecDeques; smoltcp `close` only unbinds).
        with_context(|cx| {
            let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
            while socket.recv().is_ok() {}
            loop {
                let mut popped = false;
                let result: Result<(), ()> = socket.dispatch(cx, |_cx, _meta, _emit| {
                    popped = true;
                    Ok(())
                });
                if result.is_err() || !popped {
                    break;
                }
            }
            socket.close();
            let _ = inner.corked_tx.take();
        });
    }
}

impl UdpRxDatagram {
    // 名字沿革:与 TCP 的 parse_ipv4_packet 同款——实际同时处理 v4/v6。
    pub fn parse_ipv4_packet(packet: &LoopbackIpPacket) -> Option<Self> {
        let checksum_caps = ChecksumCapabilities::default();
        if let Some(datagram) = Self::parse_v4(packet, &checksum_caps) {
            return Some(datagram);
        }

        let ipv6 = Ipv6Packet::new_checked(packet.as_bytes()).ok()?;
        let ipv6_repr = Ipv6Repr::parse(&ipv6).ok()?;
        if ipv6_repr.next_header != IpProtocol::Udp {
            return None;
        }
        let udp_packet = UdpPacket::new_checked(ipv6.payload()).ok()?;
        let src_addr = IpAddress::Ipv6(ipv6_repr.src_addr);
        let dst_addr = IpAddress::Ipv6(ipv6_repr.dst_addr);
        let udp_repr = UdpRepr::parse(&udp_packet, &src_addr, &dst_addr, &checksum_caps).ok()?;
        Some(Self {
            src: IpEndpoint::new_v6(from_smoltcp_ipv6(ipv6_repr.src_addr), udp_repr.src_port),
            dst: IpEndpoint::new_v6(from_smoltcp_ipv6(ipv6_repr.dst_addr), udp_repr.dst_port),
            payload: udp_packet.payload().to_vec(),
        })
    }

    fn parse_v4(packet: &LoopbackIpPacket, checksum_caps: &ChecksumCapabilities) -> Option<Self> {
        let ipv4 = Ipv4Packet::new_checked(packet.as_bytes()).ok()?;
        let ipv4_repr = Ipv4Repr::parse(&ipv4, checksum_caps).ok()?;
        if ipv4_repr.next_header != IpProtocol::Udp {
            return None;
        }

        let udp_packet = UdpPacket::new_checked(ipv4.payload()).ok()?;
        let src_addr = IpAddress::Ipv4(ipv4_repr.src_addr);
        let dst_addr = IpAddress::Ipv4(ipv4_repr.dst_addr);
        let udp_repr = UdpRepr::parse(&udp_packet, &src_addr, &dst_addr, checksum_caps).ok()?;
        Some(Self {
            src: IpEndpoint::new(from_smoltcp_ipv4(ipv4_repr.src_addr), udp_repr.src_port),
            dst: IpEndpoint::new(from_smoltcp_ipv4(ipv4_repr.dst_addr), udp_repr.dst_port),
            payload: udp_packet.payload().to_vec(),
        })
    }
}

impl UdpTxDatagram {
    // 名字沿革:同 parse——按 dst 家族分派 v4/v6。
    pub fn emit_ipv4_packet(&self, src: IpEndpoint) -> Option<LoopbackIpPacket> {
        if src.port == 0 || self.dst.port == 0 || self.payload.is_empty() {
            return None;
        }
        if self.dst.family == AddressFamily::Inet6 {
            return self.emit_v6(src);
        }

        let udp_repr = UdpRepr {
            src_port: src.port,
            dst_port: self.dst.port,
        };
        let udp_len = udp_repr.header_len() + self.payload.len();
        let ip_repr = IpRepr::Ipv4(Ipv4Repr {
            src_addr: to_smoltcp_ipv4(src.addr),
            dst_addr: to_smoltcp_ipv4(self.dst.addr),
            next_header: IpProtocol::Udp,
            payload_len: udp_len,
            hop_limit: 64,
        });
        let ip_header_len = ip_repr.header_len();
        let mut bytes = vec![0u8; ip_header_len + udp_len];
        let checksum_caps = ChecksumCapabilities::default();

        ip_repr.emit(&mut bytes[..ip_header_len], &checksum_caps);
        let src_addr = IpAddress::Ipv4(to_smoltcp_ipv4(src.addr));
        let dst_addr = IpAddress::Ipv4(to_smoltcp_ipv4(self.dst.addr));
        let mut udp_packet = UdpPacket::new_unchecked(&mut bytes[ip_header_len..]);
        udp_repr.emit(
            &mut udp_packet,
            &src_addr,
            &dst_addr,
            self.payload.len(),
            |payload| payload.copy_from_slice(&self.payload),
            &checksum_caps,
        );

        Some(LoopbackIpPacket::new(bytes))
    }

    fn emit_v6(&self, src: IpEndpoint) -> Option<LoopbackIpPacket> {
        let udp_repr = UdpRepr {
            src_port: src.port,
            dst_port: self.dst.port,
        };
        let udp_len = udp_repr.header_len() + self.payload.len();
        let ip_repr = IpRepr::Ipv6(Ipv6Repr {
            src_addr: to_smoltcp_ipv6(src.addr6),
            dst_addr: to_smoltcp_ipv6(self.dst.addr6),
            next_header: IpProtocol::Udp,
            payload_len: udp_len,
            hop_limit: 64,
        });
        let ip_header_len = ip_repr.header_len();
        let mut bytes = vec![0u8; ip_header_len + udp_len];
        let checksum_caps = ChecksumCapabilities::default();

        ip_repr.emit(&mut bytes[..ip_header_len], &checksum_caps);
        let src_addr = IpAddress::Ipv6(to_smoltcp_ipv6(src.addr6));
        let dst_addr = IpAddress::Ipv6(to_smoltcp_ipv6(self.dst.addr6));
        let mut udp_packet = UdpPacket::new_unchecked(&mut bytes[ip_header_len..]);
        udp_repr.emit(
            &mut udp_packet,
            &src_addr,
            &dst_addr,
            self.payload.len(),
            |payload| payload.copy_from_slice(&self.payload),
            &checksum_caps,
        );

        Some(LoopbackIpPacket::new(bytes))
    }
}

fn packet_capacity_for_bytes(bytes: usize) -> usize {
    let packet_count = bytes.div_ceil(UDP_PACKET_CAPACITY_DIVISOR);
    packet_count.clamp(1, MAX_UDP_PACKET_METADATA_CAPACITY)
}

fn smoltcp_backing_bytes(bytes: usize) -> usize {
    bytes.clamp(1, UDP_SMOLTCP_BACKING_MAX_BYTES)
}

fn to_smol_ip(endpoint: &IpEndpoint) -> IpAddress {
    match endpoint.family {
        AddressFamily::Inet6 => IpAddress::Ipv6(to_smoltcp_ipv6(endpoint.addr6)),
        _ => IpAddress::Ipv4(to_smoltcp_ipv4(endpoint.addr)),
    }
}

fn to_smol_endpoint(endpoint: &IpEndpoint) -> smoltcp::wire::IpEndpoint {
    smoltcp::wire::IpEndpoint::new(to_smol_ip(endpoint), endpoint.port)
}

fn from_smol_endpoint(endpoint: smoltcp::wire::IpEndpoint) -> IpEndpoint {
    endpoint_from_smol_ip(endpoint.addr, endpoint.port)
}

fn endpoint_from_smol_ip(addr: IpAddress, port: u16) -> IpEndpoint {
    match addr {
        IpAddress::Ipv4(v4) => IpEndpoint::new(from_smoltcp_ipv4(v4), port),
        IpAddress::Ipv6(v6) => IpEndpoint::new_v6(from_smoltcp_ipv6(v6), port),
    }
}

fn ip_repr_for(src: &IpEndpoint, dst: &IpEndpoint, udp_len: usize) -> IpRepr {
    if src.family == AddressFamily::Inet6 || dst.family == AddressFamily::Inet6 {
        IpRepr::Ipv6(Ipv6Repr {
            src_addr: to_smoltcp_ipv6(src.addr6),
            dst_addr: to_smoltcp_ipv6(dst.addr6),
            next_header: IpProtocol::Udp,
            payload_len: udp_len,
            hop_limit: 64,
        })
    } else {
        IpRepr::Ipv4(Ipv4Repr {
            src_addr: to_smoltcp_ipv4(src.addr),
            dst_addr: to_smoltcp_ipv4(dst.addr),
            next_header: IpProtocol::Udp,
            payload_len: udp_len,
            hop_limit: 64,
        })
    }
}

fn udp_send_available_inner(inner: &UdpInnerState, send_capacity: usize) -> usize {
    let queued = inner.socket.payload_send_bytes();
    let corked = inner
        .corked_tx
        .as_ref()
        .map(|datagram| datagram.payload.len())
        .unwrap_or(0);
    send_capacity.saturating_sub(queued + corked)
}

/// Flush one staged datagram into the smoltcp tx ring. A datagram with
/// an unaddressable destination is accepted and dropped (legacy queue
/// behaviour: it would sit until the drain failed to emit it).
fn push_datagram_inner(inner: &mut UdpInnerState, datagram: UdpTxDatagram) -> Option<()> {
    if datagram.dst.port == 0 || datagram.dst.is_unspecified() {
        return Some(());
    }
    let meta = udp::UdpMetadata {
        endpoint: to_smol_endpoint(&datagram.dst),
        local_address: inner.tx_src_hint,
        meta: PacketMeta::default(),
    };
    inner
        .socket
        .send_slice(&datagram.payload, meta)
        .ok()
        .map(|_| ())
}

fn unspecified_endpoint() -> IpEndpoint {
    IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0)
}

fn to_smoltcp_ipv4(addr: Ipv4Address) -> smoltcp::wire::Ipv4Address {
    let [a, b, c, d] = addr.octets();
    smoltcp::wire::Ipv4Address::new(a, b, c, d)
}

fn from_smoltcp_ipv4(addr: smoltcp::wire::Ipv4Address) -> Ipv4Address {
    Ipv4Address::new(addr.octets())
}

fn to_smoltcp_ipv6(addr: TxIpv6Address) -> smoltcp::wire::Ipv6Address {
    smoltcp::wire::Ipv6Address::from(addr.octets())
}

fn from_smoltcp_ipv6(addr: smoltcp::wire::Ipv6Address) -> TxIpv6Address {
    TxIpv6Address::new(addr.octets())
}
