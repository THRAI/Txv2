use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface};
use smoltcp::phy::{ChecksumCapabilities, Loopback, Medium};
use smoltcp::socket::{tcp, PollAt};
use smoltcp::time::Duration;
use smoltcp::wire::{
    HardwareAddress, IpAddress, IpEndpoint as SmoltcpIpEndpoint, IpProtocol, IpRepr, Ipv4Packet,
    Ipv4Repr, Ipv6Packet, Ipv6Repr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber, TcpTimestampRepr,
};

use crate::net::clock::net_now_instant;
use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::{
    AddressFamily, IpEndpoint, Ipv4Address, Ipv6Address as TxIpv6Address, SocketOptionSet,
};
use crate::sync::SpinMutex;

pub const TCP_CORK_AUTO_FLUSH_BYTES: usize = 1460;

/// P3-C S4 (R2d): cap the smoltcp TCP ring backing per direction. The
/// default SO_RCVBUF=256KB + SO_SNDBUF=64KB means every TCP socket eagerly
/// allocated 320KB (loopback included) with no accounting → mass sockets
/// OOM-panic the heap. Clamp the actual `vec!` backing here (mirrors UDP's
/// `UDP_SMOLTCP_BACKING_MAX_BYTES`); the reported SO_RCVBUF/SNDBUF stays at
/// the option value (getsockopt reads `options.socket.*_buf_size`, not the
/// buffer), so this is invisible to sockopt callers. 64KB/dir is ample for
/// the TCG/loopback throughput regime (bulk sends 32KB); real-link tuning
/// is a later concern.
const TCP_SMOLTCP_BACKING_MAX_BYTES: usize = 65_536;

fn tcp_backing_bytes(bytes: usize) -> usize {
    bytes.clamp(1, TCP_SMOLTCP_BACKING_MAX_BYTES)
}

/// P3-B S2 (D4, R1d): the smoltcp socket, the protocol sticky bits and
/// the MSG_MORE corking buffer live under ONE lock. The former
/// three-lock split let a single send make 6–9 independent
/// acquire/release round-trips — `send_available` was read and released
/// before `send_slice` used the stale value, and the corked
/// read→clear window could wipe bytes a concurrent MSG_MORE append had
/// just staged. With one lock the whole
/// available→combine→send_slice→clear composite is atomic by
/// construction. Lock order vs the global context is unchanged:
/// CONTEXT_IFACE outer, `inner` inner (taken inside `with_context`
/// closures only).
struct TcpInner {
    socket: Box<tcp::Socket<'static>>,
    protocol_state: RawTcpProtocolState,
    corked_tx: Vec<u8>,
}

/// Doc-named owner for the smoltcp TCP socket and its backing buffers.
pub struct RawTcpSocket {
    inner: SpinMutex<TcpInner>,
    recv_capacity: usize,
    send_capacity: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RawTcpProtocolState {
    pub is_recv_shut: bool,
    pub is_rst_closed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawTcpSocketError {
    InvalidEndpoint,
    InvalidState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawTcpSendReserve {
    pub bytes: usize,
    pub became_full: bool,
    pub flushed_to_protocol: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SmoltcpTcpSegment {
    pub ip_repr: IpRepr,
    pub tcp: SmoltcpTcpRepr,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SmoltcpTcpRepr {
    pub src_port: u16,
    pub dst_port: u16,
    pub control: TcpControl,
    pub seq_number: TcpSeqNumber,
    pub ack_number: Option<TcpSeqNumber>,
    pub window_len: u16,
    pub window_scale: Option<u8>,
    pub max_seg_size: Option<u16>,
    pub sack_permitted: bool,
    pub sack_ranges: [Option<(u32, u32)>; 3],
    pub timestamp: Option<TcpTimestampRepr>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SmoltcpTcpProcessPublish {
    pub connected: bool,
    pub recv_readable: bool,
    pub received_bytes: usize,
    pub send_writable: bool,
    pub recv_closed: bool,
    pub send_closed: bool,
    pub broken: bool,
}

impl RawTcpSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        let recv_capacity = options.socket.recv_buf_size;
        let send_capacity = options.socket.send_buf_size;
        let socket = new_smoltcp_tcp_socket(recv_capacity, send_capacity, options);

        Self {
            inner: SpinMutex::new(TcpInner {
                socket: Box::new(socket),
                protocol_state: RawTcpProtocolState::default(),
                corked_tx: Vec::new(),
            }),
            recv_capacity,
            send_capacity,
        }
    }

    pub fn recv_capacity(&self) -> usize {
        self.recv_capacity
    }

    pub fn send_capacity(&self) -> usize {
        self.send_capacity
    }

    pub fn recv_available(&self) -> usize {
        self.inner.lock().socket.recv_queue()
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if len == 0 {
            return Some((0, false));
        }

        let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
        if socket.recv_queue() == 0 {
            return None;
        }

        if peek {
            let bytes = core::cmp::min(socket.recv_queue(), len);
            return Some((bytes, false));
        }

        // Consume-and-discard: the smoltcp ring may wrap, so `recv` can hand
        // out less than the queued total per call — loop until done.
        let mut taken_total = 0;
        while taken_total < len {
            let want = len - taken_total;
            let taken = socket
                .recv(|buf| {
                    let take = core::cmp::min(buf.len(), want);
                    (take, take)
                })
                .unwrap_or(0);
            if taken == 0 {
                break;
            }
            taken_total += taken;
        }
        Some((taken_total, socket.recv_queue() == 0))
    }

    pub fn recv_bytes(&self, out: &mut [u8], peek: bool) -> Option<(usize, bool)> {
        if out.is_empty() {
            return Some((0, false));
        }

        let inner = &mut *self.inner.lock();
        let socket = &mut inner.socket;
        if socket.recv_queue() == 0 {
            return None;
        }

        let bytes = if peek {
            socket.peek_slice(out).unwrap_or(0)
        } else {
            socket.recv_slice(out).unwrap_or(0)
        };
        if bytes == 0 {
            return None;
        }
        Some((bytes, !peek && socket.recv_queue() == 0))
    }

    pub fn send_available(&self) -> usize {
        send_available_inner(&self.inner.lock())
    }

    pub fn send_queued(&self) -> usize {
        self.inner.lock().socket.send_queue()
    }

    pub fn enqueue_tx_len(&self, len: usize) -> Option<RawTcpSendReserve> {
        self.enqueue_tx_bytes(&vec![0; len])
    }

    pub fn enqueue_tx_bytes(&self, bytes: &[u8]) -> Option<RawTcpSendReserve> {
        self.enqueue_tx_bytes_with_more(bytes, false)
    }

    pub fn enqueue_tx_bytes_with_more(
        &self,
        bytes: &[u8],
        more: bool,
    ) -> Option<RawTcpSendReserve> {
        if bytes.is_empty() {
            return Some(RawTcpSendReserve {
                bytes: 0,
                became_full: false,
                flushed_to_protocol: false,
            });
        }

        // R1d fix: the whole available→combine→send_slice→clear composite
        // runs under one lock acquisition — no stale-available window, no
        // corked read→clear overwrite window.
        let inner = &mut *self.inner.lock();
        let available = send_available_inner(inner);
        if available == 0 {
            return None;
        }

        let requested = core::cmp::min(available, bytes.len());
        if more {
            inner
                .corked_tx
                .extend(bytes.iter().copied().take(requested));
            let flushed_to_protocol = if inner.corked_tx.len() >= TCP_CORK_AUTO_FLUSH_BYTES {
                flush_corked_inner(inner) > 0
            } else {
                false
            };
            return Some(RawTcpSendReserve {
                bytes: requested,
                became_full: send_available_inner(inner) == 0,
                flushed_to_protocol,
            });
        }

        let corked_len = inner.corked_tx.len();
        let mut combined = Vec::with_capacity(corked_len + requested);
        if corked_len != 0 {
            combined.extend(inner.corked_tx.iter().copied());
        }
        combined.extend_from_slice(&bytes[..requested]);

        let accepted = inner.socket.send_slice(&combined).ok()?;
        if accepted == 0 {
            return None;
        }
        if corked_len != 0 {
            inner.corked_tx.clear();
        }
        let accepted_new = accepted.saturating_sub(corked_len).min(requested);
        Some(RawTcpSendReserve {
            bytes: accepted_new,
            became_full: send_available_inner(inner) == 0,
            flushed_to_protocol: accepted > 0,
        })
    }

    pub fn flush_corked_tx(&self) -> usize {
        flush_corked_inner(&mut self.inner.lock())
    }

    pub fn can_recv(&self) -> bool {
        self.inner.lock().socket.can_recv()
    }

    pub fn can_send(&self) -> bool {
        self.inner.lock().socket.can_send()
    }

    pub fn may_recv(&self) -> bool {
        self.inner.lock().socket.may_recv()
    }

    pub fn may_send(&self) -> bool {
        self.inner.lock().socket.may_send()
    }

    pub fn is_recv_closed(&self) -> bool {
        self.inner.lock().protocol_state.is_recv_shut
    }

    pub fn is_send_closed(&self) -> bool {
        !self.inner.lock().socket.may_send()
    }

    pub fn close(&self) {
        let inner = &mut *self.inner.lock();
        let _ = flush_corked_inner(inner);
        inner.socket.close();
    }

    pub fn abort(&self) {
        let inner = &mut *self.inner.lock();
        inner.corked_tx.clear();
        inner.socket.abort();
    }

    pub fn reset(&self, options: &SocketOptionSet) {
        let inner = &mut *self.inner.lock();
        inner.socket = Box::new(new_smoltcp_tcp_socket(
            self.recv_capacity,
            self.send_capacity,
            options,
        ));
        inner.protocol_state = RawTcpProtocolState::default();
        inner.corked_tx.clear();
    }

    pub fn mark_recv_closed_by_peer(&self) {
        self.inner.lock().protocol_state.is_recv_shut = true;
    }

    pub fn listen_endpoint(&self, local: IpEndpoint) -> Result<(), RawTcpSocketError> {
        let endpoint = to_smoltcp_endpoint(local);
        self.inner
            .lock()
            .socket
            .listen(endpoint)
            .map_err(|error| match error {
                tcp::ListenError::InvalidState => RawTcpSocketError::InvalidState,
                tcp::ListenError::Unaddressable => RawTcpSocketError::InvalidEndpoint,
            })
    }

    pub fn connect_endpoint(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
    ) -> Result<(), RawTcpSocketError> {
        with_context(|cx| {
            self.inner
                .lock()
                .socket
                .connect(cx, to_smoltcp_endpoint(remote), to_smoltcp_endpoint(local))
                .map_err(|error| match error {
                    tcp::ConnectError::InvalidState => RawTcpSocketError::InvalidState,
                    tcp::ConnectError::Unaddressable => RawTcpSocketError::InvalidEndpoint,
                })
        })
    }

    pub fn protocol_state(&self) -> tcp::State {
        self.inner.lock().socket.state()
    }

    pub fn poll_due_now(&self) -> bool {
        with_context(|cx| match self.inner.lock().socket.poll_at(cx) {
            PollAt::Now => true,
            PollAt::Time(deadline) => deadline <= cx.now(),
            PollAt::Ingress => false,
        })
    }

    /// Return smoltcp's authoritative scheduling request for this socket.
    ///
    /// The delegate needs the full `PollAt`, rather than only a due-now
    /// boolean, so established and gracefully-closing streams can arm their
    /// retransmit/TIME-WAIT deadline while no packet is currently queued.
    pub fn poll_at(&self) -> PollAt {
        with_context(|cx| self.inner.lock().socket.poll_at(cx))
    }

    pub fn protocol_runtime_state(&self) -> RawTcpProtocolState {
        self.inner.lock().protocol_state
    }

    pub fn dispatch_segment(&self) -> Option<SmoltcpTcpSegment> {
        with_context(|cx| {
            let inner = &mut *self.inner.lock();
            let socket = &mut inner.socket;
            let mut segment = None;
            let result = socket.dispatch(cx, |_, (ip_repr, tcp_repr)| {
                segment = Some(SmoltcpTcpSegment::from_reprs(ip_repr, tcp_repr));
                Ok::<(), ()>(())
            });
            if result.is_err() {
                return None;
            }
            segment
        })
    }

    pub fn process_segment(&self, segment: &SmoltcpTcpSegment) -> SmoltcpTcpProcessPublish {
        with_context(|cx| {
            let inner = &mut *self.inner.lock();
            let before = observe_socket(&inner.socket);
            let recv_before = inner.socket.recv_queue();
            let tcp_repr = segment.tcp.as_repr(&segment.payload);
            let accepted = inner.socket.accepts(cx, &segment.ip_repr, &tcp_repr);
            let _reply = if accepted {
                inner.socket.process(cx, &segment.ip_repr, &tcp_repr)
            } else {
                None
            };
            let after = observe_socket(&inner.socket);
            let recv_after = inner.socket.recv_queue();
            let protocol_state = &mut inner.protocol_state;
            let mut publish = SmoltcpTcpProcessPublish::default();
            // Count bytes actually admitted to the receive ring, not merely
            // bytes carried by the segment. Retransmitted/duplicate segments
            // carry payload but do not move the stream forward.
            publish.received_bytes = recv_after.saturating_sub(recv_before);
            // Edge-detect "just became connected" from the smoltcp state
            // itself: TCP never re-enters the active set without a reset,
            // so this fires exactly once per connection.
            if !before.is_active && after.is_active {
                publish.connected = true;
            }
            if before.can_send != after.can_send && after.can_send {
                publish.send_writable = true;
            }
            // Publish EOF only when smoltcp's state machine has accepted an
            // in-order FIN. Looking at the raw packet flag is incorrect:
            // smoltcp intentionally defers a FIN that arrives beyond a hole
            // in the receive sequence space.
            if !before.recv_fin_received && after.recv_fin_received && !protocol_state.is_recv_shut
            {
                protocol_state.is_recv_shut = true;
                publish.recv_readable = true;
                publish.recv_closed = true;
            }
            if before.may_send && !after.may_send {
                publish.send_closed = true;
            }
            if !matches!(before.state, tcp::State::Closed)
                && matches!(after.state, tcp::State::Closed)
            {
                protocol_state.is_rst_closed = true;
                publish.broken = true;
            }
            publish
        })
    }
}

/// Space = smoltcp tx ring headroom minus corked (not-yet-committed)
/// bytes, which will need ring space when flushed.
fn send_available_inner(inner: &TcpInner) -> usize {
    if inner.socket.may_send() {
        inner
            .socket
            .send_capacity()
            .saturating_sub(inner.socket.send_queue())
            .saturating_sub(inner.corked_tx.len())
    } else {
        0
    }
}

fn flush_corked_inner(inner: &mut TcpInner) -> usize {
    if inner.corked_tx.is_empty() {
        return 0;
    }
    let bytes = core::mem::take(&mut inner.corked_tx);

    let accepted = match inner.socket.send_slice(&bytes) {
        Ok(accepted) => accepted,
        Err(_) => {
            prepend_corked_inner(inner, &bytes);
            return 0;
        }
    };
    if accepted == 0 {
        prepend_corked_inner(inner, &bytes);
        return 0;
    }

    if accepted < bytes.len() {
        prepend_corked_inner(inner, &bytes[accepted..]);
    }
    accepted
}

fn prepend_corked_inner(inner: &mut TcpInner, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if inner.corked_tx.is_empty() {
        inner.corked_tx.extend_from_slice(bytes);
        return;
    }

    let mut combined = Vec::with_capacity(bytes.len() + inner.corked_tx.len());
    combined.extend_from_slice(bytes);
    combined.extend(inner.corked_tx.iter().copied());
    inner.corked_tx = combined;
}

fn new_smoltcp_tcp_socket(
    recv_capacity: usize,
    send_capacity: usize,
    options: &SocketOptionSet,
) -> tcp::Socket<'static> {
    // R2d: clamp the actual ring backing (reported capacity is unchanged).
    let rx_buf = tcp::SocketBuffer::new(vec![0u8; tcp_backing_bytes(recv_capacity)]);
    let tx_buf = tcp::SocketBuffer::new(vec![0u8; tcp_backing_bytes(send_capacity)]);
    let mut socket = tcp::Socket::new(rx_buf, tx_buf);

    socket.set_nagle_enabled(!options.tcp.nodelay);
    socket.set_ack_delay(None);
    if options.socket.keep_alive {
        socket.set_keep_alive(Some(Duration::from_secs(options.tcp.keepidle as u64)));
    }
    if options.ip.ttl != 0 {
        socket.set_hop_limit(Some(options.ip.ttl));
    }
    socket
}

impl SmoltcpTcpSegment {
    fn from_reprs(ip_repr: IpRepr, tcp_repr: TcpRepr<'_>) -> Self {
        Self {
            ip_repr,
            tcp: SmoltcpTcpRepr {
                src_port: tcp_repr.src_port,
                dst_port: tcp_repr.dst_port,
                control: tcp_repr.control,
                seq_number: tcp_repr.seq_number,
                ack_number: tcp_repr.ack_number,
                window_len: tcp_repr.window_len,
                window_scale: tcp_repr.window_scale,
                max_seg_size: tcp_repr.max_seg_size,
                sack_permitted: tcp_repr.sack_permitted,
                sack_ranges: tcp_repr.sack_ranges,
                timestamp: tcp_repr.timestamp,
            },
            payload: tcp_repr.payload.to_vec(),
        }
    }

    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }

    pub fn emit_ipv4_packet(&self) -> Option<LoopbackIpPacket> {
        if self.ip_repr.next_header() != IpProtocol::Tcp {
            return None;
        }

        let tcp_repr = self.tcp.as_repr(&self.payload);
        let tcp_len = tcp_repr.buffer_len();
        let mut ip_repr = self.ip_repr.clone();
        ip_repr.set_payload_len(tcp_len);
        let ip_header_len = ip_repr.header_len();
        let mut bytes = vec![0u8; ip_header_len + tcp_len];
        let checksum_caps = ChecksumCapabilities::default();

        ip_repr.emit(&mut bytes[..ip_header_len], &checksum_caps);
        let mut tcp_packet = TcpPacket::new_unchecked(&mut bytes[ip_header_len..]);
        tcp_repr.emit(
            &mut tcp_packet,
            &ip_repr.src_addr(),
            &ip_repr.dst_addr(),
            &checksum_caps,
        );

        Some(LoopbackIpPacket::new(bytes))
    }

    pub fn parse_ipv4_packet(packet: &LoopbackIpPacket) -> Option<Self> {
        let checksum_caps = ChecksumCapabilities::default();
        if let Some(segment) = parse_ipv4_tcp_packet(packet, &checksum_caps) {
            return Some(segment);
        }

        let ipv6 = Ipv6Packet::new_checked(packet.as_bytes()).ok()?;
        let ipv6_repr = Ipv6Repr::parse(&ipv6).ok()?;
        if ipv6_repr.next_header != IpProtocol::Tcp {
            return None;
        }
        let tcp_packet = TcpPacket::new_checked(ipv6.payload()).ok()?;
        let src = IpAddress::Ipv6(ipv6_repr.src_addr);
        let dst = IpAddress::Ipv6(ipv6_repr.dst_addr);
        let tcp_repr = TcpRepr::parse(&tcp_packet, &src, &dst, &checksum_caps).ok()?;
        Some(Self::from_reprs(IpRepr::Ipv6(ipv6_repr), tcp_repr))
    }

    /// Read only the TCP four-tuple from a queued packet.
    ///
    /// Unlike `parse_ipv4_packet`, this does not copy the TCP payload.  It is
    /// therefore safe to use while the short loopback-queue selection lock is
    /// held.
    pub fn packet_endpoints(packet: &LoopbackIpPacket) -> Option<(IpEndpoint, IpEndpoint)> {
        if let Ok(ipv4) = Ipv4Packet::new_checked(packet.as_bytes()) {
            if ipv4.next_header() != IpProtocol::Tcp {
                return None;
            }
            let tcp = TcpPacket::new_checked(ipv4.payload()).ok()?;
            return Some((
                IpEndpoint::new(from_smoltcp_ipv4(ipv4.src_addr()), tcp.src_port()),
                IpEndpoint::new(from_smoltcp_ipv4(ipv4.dst_addr()), tcp.dst_port()),
            ));
        }

        let ipv6 = Ipv6Packet::new_checked(packet.as_bytes()).ok()?;
        if ipv6.next_header() != IpProtocol::Tcp {
            return None;
        }
        let tcp = TcpPacket::new_checked(ipv6.payload()).ok()?;
        Some((
            IpEndpoint::new_v6(from_smoltcp_ipv6(ipv6.src_addr()), tcp.src_port()),
            IpEndpoint::new_v6(from_smoltcp_ipv6(ipv6.dst_addr()), tcp.dst_port()),
        ))
    }

    pub fn src_endpoint(&self) -> Option<IpEndpoint> {
        match self.ip_repr.src_addr() {
            IpAddress::Ipv4(addr) => {
                Some(IpEndpoint::new(from_smoltcp_ipv4(addr), self.tcp.src_port))
            }
            IpAddress::Ipv6(addr) => Some(IpEndpoint::new_v6(
                from_smoltcp_ipv6(addr),
                self.tcp.src_port,
            )),
        }
    }

    pub fn dst_endpoint(&self) -> Option<IpEndpoint> {
        match self.ip_repr.dst_addr() {
            IpAddress::Ipv4(addr) => {
                Some(IpEndpoint::new(from_smoltcp_ipv4(addr), self.tcp.dst_port))
            }
            IpAddress::Ipv6(addr) => Some(IpEndpoint::new_v6(
                from_smoltcp_ipv6(addr),
                self.tcp.dst_port,
            )),
        }
    }
}

fn parse_ipv4_tcp_packet(
    packet: &LoopbackIpPacket,
    checksum_caps: &ChecksumCapabilities,
) -> Option<SmoltcpTcpSegment> {
    let ipv4 = Ipv4Packet::new_checked(packet.as_bytes()).ok()?;
    let ipv4_repr = Ipv4Repr::parse(&ipv4, checksum_caps).ok()?;
    if ipv4_repr.next_header != IpProtocol::Tcp {
        return None;
    }

    let tcp_packet = TcpPacket::new_checked(ipv4.payload()).ok()?;
    let src = IpAddress::Ipv4(ipv4_repr.src_addr);
    let dst = IpAddress::Ipv4(ipv4_repr.dst_addr);
    let tcp_repr = TcpRepr::parse(&tcp_packet, &src, &dst, checksum_caps).ok()?;
    Some(SmoltcpTcpSegment::from_reprs(
        IpRepr::Ipv4(ipv4_repr),
        tcp_repr,
    ))
}

impl SmoltcpTcpRepr {
    fn as_repr<'a>(&self, payload: &'a [u8]) -> TcpRepr<'a> {
        TcpRepr {
            src_port: self.src_port,
            dst_port: self.dst_port,
            control: self.control,
            seq_number: self.seq_number,
            ack_number: self.ack_number,
            window_len: self.window_len,
            window_scale: self.window_scale,
            max_seg_size: self.max_seg_size,
            sack_permitted: self.sack_permitted,
            sack_ranges: self.sack_ranges,
            timestamp: self.timestamp,
            payload,
        }
    }
}

#[derive(Clone, Copy)]
struct SocketProtocolObservation {
    state: tcp::State,
    can_send: bool,
    may_send: bool,
    is_active: bool,
    recv_fin_received: bool,
}

fn observe_socket(socket: &tcp::Socket<'_>) -> SocketProtocolObservation {
    SocketProtocolObservation {
        state: socket.state(),
        can_send: socket.can_send(),
        may_send: socket.may_send(),
        recv_fin_received: socket.recv_fin_received(),
        is_active: matches!(
            socket.state(),
            tcp::State::Established
                | tcp::State::CloseWait
                | tcp::State::FinWait1
                | tcp::State::FinWait2
        ),
    }
}

// Single-netns skeleton: one persistent Interface acting only as the
// `Context` provider (checksum caps + `now`). Lock order is CONTEXT_IFACE
// outer, `self.socket` inner at every call site. Per-netns Interfaces are a
// later phase (REFACTOR_PLAN_A_v2 P5).
static CONTEXT_IFACE: SpinMutex<Option<Interface>> = SpinMutex::new(None);

pub(crate) fn with_context<R>(f: impl FnOnce(&mut smoltcp::iface::Context) -> R) -> R {
    let mut slot = CONTEXT_IFACE.lock();
    let iface = slot.get_or_insert_with(|| {
        let mut device = Loopback::new(Medium::Ip);
        Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut device,
            smoltcp::time::Instant::ZERO,
        )
    });
    let cx = iface.context();
    cx.now = net_now_instant();
    f(cx)
}

fn to_smoltcp_endpoint(endpoint: IpEndpoint) -> SmoltcpIpEndpoint {
    let addr = match endpoint.family {
        AddressFamily::Inet6 => IpAddress::Ipv6(to_smoltcp_ipv6(endpoint.addr6)),
        _ => IpAddress::Ipv4(to_smoltcp_ipv4(endpoint.addr)),
    };
    SmoltcpIpEndpoint::new(addr, endpoint.port)
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
