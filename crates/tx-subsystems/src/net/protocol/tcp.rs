use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::iface::{Config, Interface};
use smoltcp::phy::{ChecksumCapabilities, Loopback, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Duration;
use smoltcp::wire::{
    HardwareAddress, IpAddress, IpEndpoint as SmoltcpIpEndpoint, IpProtocol, IpRepr, Ipv4Packet,
    Ipv4Repr, TcpControl, TcpPacket, TcpRepr, TcpSeqNumber, TcpTimestampRepr,
};

use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::{IpEndpoint, Ipv4Address, SocketOptionSet};
use crate::sync::SpinMutex;

/// Doc-named owner for the smoltcp TCP socket and its backing buffers.
pub struct RawTcpSocket {
    socket: SpinMutex<Box<tcp::Socket<'static>>>,
    protocol_state: SpinMutex<RawTcpProtocolState>,
    last_syn_ack: SpinMutex<Option<SmoltcpTcpSegment>>,
    rx_buffer: SpinMutex<VecDeque<u8>>,
    tx_buffer: SpinMutex<VecDeque<u8>>,
    recv_capacity: usize,
    send_capacity: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RawTcpProtocolState {
    pub has_connected: bool,
    pub is_recv_shut: bool,
    pub is_rst_closed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawTcpSocketError {
    InvalidEndpoint,
    InvalidState,
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
    pub send_writable: bool,
    pub recv_closed: bool,
    pub send_closed: bool,
    pub broken: bool,
}

impl RawTcpSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        let recv_capacity = options.socket.recv_buf_size;
        let send_capacity = options.socket.send_buf_size;
        let rx_buf = tcp::SocketBuffer::new(vec![0u8; recv_capacity]);
        let tx_buf = tcp::SocketBuffer::new(vec![0u8; send_capacity]);
        let mut socket = tcp::Socket::new(rx_buf, tx_buf);

        socket.set_nagle_enabled(!options.tcp.nodelay);
        socket.set_ack_delay(None);
        if options.socket.keep_alive {
            socket.set_keep_alive(Some(Duration::from_secs(options.tcp.keepidle as u64)));
        }
        if options.ip.ttl != 0 {
            socket.set_hop_limit(Some(options.ip.ttl));
        }

        Self {
            socket: SpinMutex::new(Box::new(socket)),
            protocol_state: SpinMutex::new(RawTcpProtocolState::default()),
            last_syn_ack: SpinMutex::new(None),
            rx_buffer: SpinMutex::new(VecDeque::new()),
            tx_buffer: SpinMutex::new(VecDeque::new()),
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

    pub fn ingest_rx_bytes(&self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }

        let mut rx = self.rx_buffer.lock();
        let was_empty = rx.is_empty();
        let available = self.recv_capacity.saturating_sub(rx.len());
        let accepted = core::cmp::min(available, bytes.len());
        rx.extend(bytes.iter().copied().take(accepted));
        was_empty && accepted > 0
    }

    pub fn recv_available(&self) -> usize {
        self.rx_buffer.lock().len()
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if len == 0 {
            return Some((0, false));
        }

        let mut rx = self.rx_buffer.lock();
        if rx.is_empty() {
            return None;
        }

        let bytes = core::cmp::min(rx.len(), len);
        if !peek {
            for _ in 0..bytes {
                let _ = rx.pop_front();
            }
        }
        Some((bytes, !peek && rx.is_empty()))
    }

    pub fn recv_bytes(&self, out: &mut [u8], peek: bool) -> Option<(usize, bool)> {
        if out.is_empty() {
            return Some((0, false));
        }

        let mut rx = self.rx_buffer.lock();
        if rx.is_empty() {
            return None;
        }

        let bytes = core::cmp::min(rx.len(), out.len());
        if peek {
            for (dst, src) in out.iter_mut().take(bytes).zip(rx.iter()) {
                *dst = *src;
            }
        } else {
            for dst in out.iter_mut().take(bytes) {
                if let Some(byte) = rx.pop_front() {
                    *dst = byte;
                }
            }
        }
        Some((bytes, !peek && rx.is_empty()))
    }

    pub fn send_available(&self) -> usize {
        let staged_available = self
            .send_capacity
            .saturating_sub(self.tx_buffer.lock().len());
        let protocol_available = {
            let socket = self.socket.lock();
            if socket.may_send() {
                socket.send_capacity().saturating_sub(socket.send_queue())
            } else {
                0
            }
        };
        core::cmp::min(staged_available, protocol_available)
    }

    pub fn send_queued(&self) -> usize {
        self.tx_buffer.lock().len()
    }

    pub fn enqueue_tx_len(&self, len: usize) -> Option<(usize, bool)> {
        if len == 0 {
            return Some((0, false));
        }

        let available = self.send_available();
        if available == 0 {
            return None;
        }

        let bytes = core::cmp::min(available, len);
        let accepted = self.enqueue_protocol_tx_bytes(&vec![0; bytes]).ok()?;
        if accepted == 0 {
            return None;
        }
        self.tx_buffer
            .lock()
            .extend(core::iter::repeat_n(0, accepted));
        Some((accepted, self.send_available() == 0))
    }

    pub fn enqueue_tx_bytes(&self, bytes: &[u8]) -> Option<(usize, bool)> {
        if bytes.is_empty() {
            return Some((0, false));
        }

        let available = self.send_available();
        if available == 0 {
            return None;
        }

        let requested = core::cmp::min(available, bytes.len());
        let accepted = self.enqueue_protocol_tx_bytes(&bytes[..requested]).ok()?;
        if accepted == 0 {
            return None;
        }
        self.tx_buffer
            .lock()
            .extend(bytes.iter().copied().take(accepted));
        Some((accepted, self.send_available() == 0))
    }

    pub fn ack_tx_bytes(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return false;
        }

        let had_no_space = self.send_available() == 0;
        let mut tx = self.tx_buffer.lock();
        let released = core::cmp::min(bytes, tx.len());
        for _ in 0..released {
            let _ = tx.pop_front();
        }
        drop(tx);
        had_no_space && released > 0 && self.send_available() > 0
    }

    pub fn dequeue_tx_bytes(&self, max_len: usize) -> Option<(Vec<u8>, bool)> {
        if max_len == 0 {
            return None;
        }

        let had_no_space = self.send_available() == 0;
        let mut tx = self.tx_buffer.lock();
        if tx.is_empty() {
            return None;
        }

        let bytes = core::cmp::min(tx.len(), max_len);
        let mut drained = Vec::with_capacity(bytes);
        for _ in 0..bytes {
            if let Some(byte) = tx.pop_front() {
                drained.push(byte);
            }
        }

        Some((drained, had_no_space))
    }

    pub fn can_recv(&self) -> bool {
        self.socket.lock().can_recv()
    }

    pub fn can_send(&self) -> bool {
        self.socket.lock().can_send()
    }

    pub fn may_recv(&self) -> bool {
        self.socket.lock().may_recv()
    }

    pub fn may_send(&self) -> bool {
        self.socket.lock().may_send()
    }

    pub fn is_recv_closed(&self) -> bool {
        self.protocol_state.lock().is_recv_shut
    }

    pub fn is_send_closed(&self) -> bool {
        !self.socket.lock().may_send()
    }

    pub fn close(&self) {
        self.socket.lock().close();
    }

    pub fn abort(&self) {
        self.socket.lock().abort();
    }

    pub fn mark_recv_closed_by_peer(&self) {
        self.protocol_state.lock().is_recv_shut = true;
    }

    pub fn listen_endpoint(&self, local: IpEndpoint) -> Result<(), RawTcpSocketError> {
        let endpoint = to_smoltcp_endpoint(local);
        self.socket
            .lock()
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
            self.socket
                .lock()
                .connect(cx, to_smoltcp_endpoint(remote), to_smoltcp_endpoint(local))
                .map_err(|error| match error {
                    tcp::ConnectError::InvalidState => RawTcpSocketError::InvalidState,
                    tcp::ConnectError::Unaddressable => RawTcpSocketError::InvalidEndpoint,
                })
        })
    }

    pub fn protocol_state(&self) -> tcp::State {
        self.socket.lock().state()
    }

    pub fn protocol_runtime_state(&self) -> RawTcpProtocolState {
        *self.protocol_state.lock()
    }

    pub fn dispatch_segment(&self) -> Option<SmoltcpTcpSegment> {
        with_context(|cx| {
            let mut socket = self.socket.lock();
            let mut segment = None;
            let result = socket.dispatch(cx, |_, (ip_repr, tcp_repr)| {
                segment = Some(SmoltcpTcpSegment::from_reprs(ip_repr, tcp_repr));
                Ok::<(), ()>(())
            });
            if result.is_err() {
                return None;
            }
            let segment = segment?;
            self.remember_syn_ack(&segment);
            Some(segment)
        })
    }

    pub fn retransmit_syn_ack_segment(&self) -> Option<SmoltcpTcpSegment> {
        self.last_syn_ack.lock().clone()
    }

    pub fn process_segment(&self, segment: &SmoltcpTcpSegment) -> SmoltcpTcpProcessPublish {
        with_context(|cx| {
            let mut socket = self.socket.lock();
            let before = observe_socket(&socket);
            let tcp_repr = segment.tcp.as_repr(&segment.payload);
            let _reply = if socket.accepts(cx, &segment.ip_repr, &tcp_repr) {
                socket.process(cx, &segment.ip_repr, &tcp_repr)
            } else {
                None
            };
            let after = observe_socket(&socket);
            drop(socket);

            let mut protocol_state = self.protocol_state.lock();
            let mut publish = SmoltcpTcpProcessPublish::default();
            if !protocol_state.has_connected && after.is_active {
                protocol_state.has_connected = true;
                publish.connected = true;
            }
            if before.can_send != after.can_send && after.can_send {
                publish.send_writable = true;
            }
            if matches!(segment.tcp.control, TcpControl::Fin) && !protocol_state.is_recv_shut {
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

    pub fn drain_protocol_recv_to_staging(&self) -> bool {
        let mut became_readable = false;
        let mut scratch = [0u8; 1024];

        loop {
            let read = {
                let mut socket = self.socket.lock();
                if !socket.can_recv() {
                    0
                } else {
                    socket.recv_slice(&mut scratch).unwrap_or_default()
                }
            };
            if read == 0 {
                break;
            }
            became_readable |= self.ingest_rx_bytes(&scratch[..read]);
        }

        became_readable
    }

    fn enqueue_protocol_tx_bytes(&self, bytes: &[u8]) -> Result<usize, tcp::SendError> {
        self.socket.lock().send_slice(bytes)
    }

    fn remember_syn_ack(&self, segment: &SmoltcpTcpSegment) {
        if segment.tcp.control == TcpControl::Syn && segment.tcp.ack_number.is_some() {
            *self.last_syn_ack.lock() = Some(segment.clone());
        }
    }
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
        let ipv4 = Ipv4Packet::new_checked(packet.as_bytes()).ok()?;
        let ipv4_repr = Ipv4Repr::parse(&ipv4, &checksum_caps).ok()?;
        if ipv4_repr.next_header != IpProtocol::Tcp {
            return None;
        }

        let tcp_packet = TcpPacket::new_checked(ipv4.payload()).ok()?;
        let src = IpAddress::Ipv4(ipv4_repr.src_addr);
        let dst = IpAddress::Ipv4(ipv4_repr.dst_addr);
        let tcp_repr = TcpRepr::parse(&tcp_packet, &src, &dst, &checksum_caps).ok()?;
        Some(Self::from_reprs(IpRepr::Ipv4(ipv4_repr), tcp_repr))
    }

    pub fn src_endpoint(&self) -> Option<IpEndpoint> {
        match self.ip_repr.src_addr() {
            IpAddress::Ipv4(addr) => {
                Some(IpEndpoint::new(from_smoltcp_ipv4(addr), self.tcp.src_port))
            }
        }
    }

    pub fn dst_endpoint(&self) -> Option<IpEndpoint> {
        match self.ip_repr.dst_addr() {
            IpAddress::Ipv4(addr) => {
                Some(IpEndpoint::new(from_smoltcp_ipv4(addr), self.tcp.dst_port))
            }
        }
    }
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
}

fn observe_socket(socket: &tcp::Socket<'_>) -> SocketProtocolObservation {
    SocketProtocolObservation {
        state: socket.state(),
        can_send: socket.can_send(),
        may_send: socket.may_send(),
        is_active: matches!(
            socket.state(),
            tcp::State::Established
                | tcp::State::CloseWait
                | tcp::State::FinWait1
                | tcp::State::FinWait2
        ),
    }
}

fn with_context<R>(f: impl FnOnce(&mut smoltcp::iface::Context) -> R) -> R {
    let mut device = Loopback::new(Medium::Ip);
    let mut iface = Interface::new(
        Config::new(HardwareAddress::Ip),
        &mut device,
        smoltcp::time::Instant::ZERO,
    );
    f(iface.context())
}

fn to_smoltcp_endpoint(endpoint: IpEndpoint) -> SmoltcpIpEndpoint {
    SmoltcpIpEndpoint::new(
        IpAddress::Ipv4(to_smoltcp_ipv4(endpoint.addr)),
        endpoint.port,
    )
}

fn to_smoltcp_ipv4(addr: Ipv4Address) -> smoltcp::wire::Ipv4Address {
    let [a, b, c, d] = addr.octets();
    smoltcp::wire::Ipv4Address::new(a, b, c, d)
}

fn from_smoltcp_ipv4(addr: smoltcp::wire::Ipv4Address) -> Ipv4Address {
    Ipv4Address::new(addr.octets())
}
