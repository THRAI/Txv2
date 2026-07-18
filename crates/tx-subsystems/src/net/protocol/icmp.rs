use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    Icmpv4Packet, Icmpv4Repr, Icmpv6Packet, Icmpv6Repr, IpProtocol, IpRepr,
    Ipv4Address as SmoltcpIpv4Address, Ipv4Packet, Ipv4Repr, Ipv6Address as SmoltcpIpv6Address,
    Ipv6Repr,
};

use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::{Ipv4Address, Ipv6Address, ProtocolNumber, SocketOptionSet};
use crate::sync::SpinMutex;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Icmpv4EchoPacket {
    pub src: Ipv4Address,
    pub dst: Ipv4Address,
    pub ident: u16,
    pub seq_no: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Icmpv4Event {
    EchoRequest(Icmpv4EchoPacket),
    EchoReply(Icmpv4EchoPacket),
    Unsupported,
    Malformed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Icmpv6EchoPacket {
    pub src: Ipv6Address,
    pub dst: Ipv6Address,
    pub ident: u16,
    pub seq_no: u16,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Icmpv6Event {
    EchoRequest(Icmpv6EchoPacket),
    EchoReply(Icmpv6EchoPacket),
    Unsupported,
    Malformed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawIcmpTxDrain {
    pub packet: Icmpv4EchoPacket,
    pub became_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawIcmpRecvDrain {
    pub bytes: usize,
    pub source: RawIpAddress,
    pub destination: RawIpAddress,
    pub truncated: bool,
    pub became_empty: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawIpAddress {
    V4(Ipv4Address),
    V6(Ipv6Address),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawIpv6Packet {
    pub src: Ipv6Address,
    pub dst: Ipv6Address,
    pub next_header: ProtocolNumber,
    pub payload: Vec<u8>,
}

pub struct RawIcmpSocket {
    rx_queue: SpinMutex<VecDeque<Icmpv4EchoPacket>>,
    rx_ipv6_queue: SpinMutex<VecDeque<RawIpv6Packet>>,
    tx_queue: SpinMutex<VecDeque<Icmpv4EchoPacket>>,
    /// External v6 echo requests bound for the device-TX lane (mirror of the
    /// v4 `tx_queue`; loopback v6 echoes are answered inline and never queue).
    tx6_queue: SpinMutex<VecDeque<Icmpv6EchoPacket>>,
    recv_capacity: usize,
    send_capacity: usize,
}

impl Icmpv4EchoPacket {
    pub fn reply_packet(&self) -> Self {
        Self {
            src: self.dst,
            dst: self.src,
            ident: self.ident,
            seq_no: self.seq_no,
            payload: self.payload.clone(),
        }
    }
}

impl Icmpv6EchoPacket {
    pub fn reply_packet(&self) -> Self {
        Self {
            src: self.dst,
            dst: self.src,
            ident: self.ident,
            seq_no: self.seq_no,
            payload: self.payload.clone(),
        }
    }
}

impl RawIcmpSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        Self {
            rx_queue: SpinMutex::new(VecDeque::new()),
            rx_ipv6_queue: SpinMutex::new(VecDeque::new()),
            tx_queue: SpinMutex::new(VecDeque::new()),
            tx6_queue: SpinMutex::new(VecDeque::new()),
            recv_capacity: options.socket.recv_buf_size,
            send_capacity: options.socket.send_buf_size,
        }
    }

    pub fn recv_available(&self) -> usize {
        let echo_bytes: usize = self
            .rx_queue
            .lock()
            .iter()
            .map(icmpv4_echo_raw_packet_len)
            .sum();
        let raw_ipv6_bytes: usize = self
            .rx_ipv6_queue
            .lock()
            .iter()
            .map(|packet| packet.payload.len())
            .sum();
        echo_bytes + raw_ipv6_bytes
    }

    pub fn recv_capacity(&self) -> usize {
        self.recv_capacity
    }

    pub fn send_available(&self) -> usize {
        self.send_capacity.saturating_sub(
            self.tx_queue
                .lock()
                .iter()
                .map(icmpv4_echo_message_len)
                .sum(),
        )
    }

    pub fn enqueue_tx_echo(&self, packet: Icmpv4EchoPacket) -> Option<(usize, bool)> {
        let bytes = icmpv4_echo_message_len(&packet);
        let mut tx = self.tx_queue.lock();
        let available = self.send_capacity.saturating_sub(echo_queue_len(&tx));
        if bytes > available {
            return None;
        }

        tx.push_back(packet);
        Some((bytes, echo_queue_len(&tx) == self.send_capacity))
    }

    pub fn pop_tx_echo(&self) -> Option<RawIcmpTxDrain> {
        let mut tx = self.tx_queue.lock();
        let had_no_space = echo_queue_len(&tx) == self.send_capacity;
        let packet = tx.pop_front()?;
        Some(RawIcmpTxDrain {
            packet,
            became_available: had_no_space,
        })
    }

    pub fn peek_tx_echo(&self) -> Option<Icmpv4EchoPacket> {
        self.tx_queue.lock().front().cloned()
    }

    pub fn commit_tx_echo_sent(&self) -> Option<RawIcmpTxDrain> {
        self.pop_tx_echo()
    }

    // External v6 echo TX queue (mirror of the v4 `tx_echo` family above).
    pub fn enqueue_tx6_echo(&self, packet: Icmpv6EchoPacket) -> Option<(usize, bool)> {
        let bytes = icmpv6_echo_message_len(&packet);
        let mut tx = self.tx6_queue.lock();
        let queued: usize = tx.iter().map(icmpv6_echo_message_len).sum();
        let available = self.send_capacity.saturating_sub(queued);
        if bytes > available {
            return None;
        }
        tx.push_back(packet);
        let queued_after: usize = tx.iter().map(icmpv6_echo_message_len).sum();
        Some((bytes, queued_after == self.send_capacity))
    }

    pub fn peek_tx6_echo(&self) -> Option<Icmpv6EchoPacket> {
        self.tx6_queue.lock().front().cloned()
    }

    /// Pop the head after the sink accepted it; returns `became_available`.
    pub fn commit_tx6_echo_sent(&self) -> Option<bool> {
        let mut tx = self.tx6_queue.lock();
        let had_no_space =
            tx.iter().map(icmpv6_echo_message_len).sum::<usize>() == self.send_capacity;
        tx.pop_front()?;
        Some(had_no_space)
    }

    pub fn ingest_rx_echo_reply(&self, packet: Icmpv4EchoPacket) -> bool {
        let bytes = icmpv4_echo_raw_packet_len(&packet);
        let mut rx = self.rx_queue.lock();
        let was_empty = rx.is_empty();
        let available = self
            .recv_capacity
            .saturating_sub(rx.iter().map(icmpv4_echo_raw_packet_len).sum::<usize>());
        if bytes > available {
            return false;
        }

        rx.push_back(packet);
        was_empty
    }

    pub fn ingest_rx_ipv6_packet(&self, packet: RawIpv6Packet) -> bool {
        let bytes = packet.payload.len();
        let echo_bytes = self
            .rx_queue
            .lock()
            .iter()
            .map(icmpv4_echo_raw_packet_len)
            .sum::<usize>();
        let mut rx = self.rx_ipv6_queue.lock();
        let was_empty = echo_bytes == 0 && rx.is_empty();
        let available = self
            .recv_capacity
            .saturating_sub(echo_bytes + raw_ipv6_queue_len(&rx));
        if bytes > available {
            return false;
        }

        rx.push_back(packet);
        was_empty
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if len == 0 {
            return Some((0, false));
        }

        {
            let mut rx = self.rx_queue.lock();
            if let Some(packet) = rx.front() {
                let bytes = core::cmp::min(icmpv4_echo_raw_packet_len(packet), len);
                if !peek {
                    let _ = rx.pop_front();
                }
                let became_empty = !peek && rx.is_empty() && self.rx_ipv6_queue.lock().is_empty();
                return Some((bytes, became_empty));
            }
        }

        let mut rx = self.rx_ipv6_queue.lock();
        let packet = rx.front()?;
        let bytes = core::cmp::min(packet.payload.len(), len);
        if !peek {
            let _ = rx.pop_front();
        }
        let raw_ipv6_empty = rx.is_empty();
        drop(rx);
        Some((
            bytes,
            !peek && raw_ipv6_empty && self.rx_queue.lock().is_empty(),
        ))
    }

    pub fn recv_bytes(&self, out: &mut [u8], peek: bool) -> Option<RawIcmpRecvDrain> {
        if out.is_empty() {
            return Some(RawIcmpRecvDrain {
                bytes: 0,
                source: RawIpAddress::V4(Ipv4Address::UNSPECIFIED),
                destination: RawIpAddress::V4(Ipv4Address::UNSPECIFIED),
                truncated: false,
                became_empty: false,
            });
        }

        {
            let mut rx = self.rx_queue.lock();
            if let Some(packet) = rx.front() {
                let raw_packet = build_icmpv4_echo_reply(packet);
                let packet_bytes = raw_packet.as_bytes();
                let bytes = core::cmp::min(packet_bytes.len(), out.len());
                out[..bytes].copy_from_slice(&packet_bytes[..bytes]);
                let source = packet.src;
                let destination = packet.dst;
                let truncated = bytes < packet_bytes.len();
                if !peek {
                    let _ = rx.pop_front();
                }
                let became_empty = !peek && rx.is_empty() && self.rx_ipv6_queue.lock().is_empty();
                return Some(RawIcmpRecvDrain {
                    bytes,
                    source: RawIpAddress::V4(source),
                    destination: RawIpAddress::V4(destination),
                    truncated,
                    became_empty,
                });
            }
        }

        let mut rx = self.rx_ipv6_queue.lock();
        let packet = rx.front()?;
        let bytes = core::cmp::min(packet.payload.len(), out.len());
        out[..bytes].copy_from_slice(&packet.payload[..bytes]);
        let source = packet.src;
        let destination = packet.dst;
        let truncated = bytes < packet.payload.len();
        if !peek {
            let _ = rx.pop_front();
        }
        let raw_ipv6_empty = rx.is_empty();
        drop(rx);

        Some(RawIcmpRecvDrain {
            bytes,
            source: RawIpAddress::V6(source),
            destination: RawIpAddress::V6(destination),
            truncated,
            became_empty: !peek && raw_ipv6_empty && self.rx_queue.lock().is_empty(),
        })
    }
}

pub const ICMPV4_ECHO_HEADER_LEN: usize = 8;
const IPV4_HEADER_LEN: usize = 20;

pub fn parse_icmpv4_from_ipv4_bytes(packet: &[u8]) -> Icmpv4Event {
    let checksum = ChecksumCapabilities::default();
    let ipv4 = match Ipv4Packet::new_checked(packet) {
        Ok(ipv4) => ipv4,
        Err(_) => return Icmpv4Event::Malformed,
    };
    let ipv4_repr = match Ipv4Repr::parse(&ipv4, &checksum) {
        Ok(repr) => repr,
        Err(_) => return Icmpv4Event::Malformed,
    };
    if ipv4_repr.next_header != IpProtocol::Icmp {
        return Icmpv4Event::Unsupported;
    }
    parse_icmpv4_payload(
        from_smoltcp_ipv4(ipv4_repr.src_addr),
        from_smoltcp_ipv4(ipv4_repr.dst_addr),
        ipv4.payload(),
    )
}

pub fn parse_icmpv4_loopback_packet(packet: &LoopbackIpPacket) -> Icmpv4Event {
    parse_icmpv4_from_ipv4_bytes(packet.as_bytes())
}

pub fn parse_icmpv4_payload(src: Ipv4Address, dst: Ipv4Address, payload: &[u8]) -> Icmpv4Event {
    let packet = match Icmpv4Packet::new_checked(payload) {
        Ok(packet) => packet,
        Err(_) => return Icmpv4Event::Malformed,
    };
    let repr = match Icmpv4Repr::parse(&packet, &ChecksumCapabilities::default()) {
        Ok(repr) => repr,
        Err(_) => return Icmpv4Event::Malformed,
    };

    match repr {
        Icmpv4Repr::EchoRequest {
            ident,
            seq_no,
            data,
        } => Icmpv4Event::EchoRequest(Icmpv4EchoPacket {
            src,
            dst,
            ident,
            seq_no,
            payload: data.to_vec(),
        }),
        Icmpv4Repr::EchoReply {
            ident,
            seq_no,
            data,
        } => Icmpv4Event::EchoReply(Icmpv4EchoPacket {
            src,
            dst,
            ident,
            seq_no,
            payload: data.to_vec(),
        }),
        _ => Icmpv4Event::Unsupported,
    }
}

pub fn parse_icmpv4_echo_payload_unchecked(
    src: Ipv4Address,
    dst: Ipv4Address,
    payload: &[u8],
) -> Icmpv4Event {
    parse_icmpv4_echo_payload_unchecked_inner(src, dst, payload, false)
}

pub fn parse_raw_icmpv4_echo_payload_unchecked(
    src: Ipv4Address,
    dst: Ipv4Address,
    payload: &[u8],
) -> Icmpv4Event {
    parse_icmpv4_echo_payload_unchecked_inner(src, dst, payload, true)
}

fn parse_icmpv4_echo_payload_unchecked_inner(
    src: Ipv4Address,
    dst: Ipv4Address,
    payload: &[u8],
    allow_nonzero_code: bool,
) -> Icmpv4Event {
    if payload.len() < ICMPV4_ECHO_HEADER_LEN {
        return Icmpv4Event::Malformed;
    }
    if payload[1] != 0 && !allow_nonzero_code {
        return Icmpv4Event::Unsupported;
    }

    let ident = u16::from_be_bytes([payload[4], payload[5]]);
    let seq_no = u16::from_be_bytes([payload[6], payload[7]]);
    let packet = Icmpv4EchoPacket {
        src,
        dst,
        ident,
        seq_no,
        payload: payload[ICMPV4_ECHO_HEADER_LEN..].to_vec(),
    };
    match payload[0] {
        8 => Icmpv4Event::EchoRequest(packet),
        0 => Icmpv4Event::EchoReply(packet),
        _ => Icmpv4Event::Unsupported,
    }
}

pub const ICMPV6_ECHO_HEADER_LEN: usize = 8;

pub fn parse_icmpv6_payload_unchecked(
    src: Ipv6Address,
    dst: Ipv6Address,
    payload: &[u8],
) -> Icmpv6Event {
    if payload.len() < ICMPV6_ECHO_HEADER_LEN {
        return Icmpv6Event::Malformed;
    }

    let ident = u16::from_be_bytes([payload[4], payload[5]]);
    let seq_no = u16::from_be_bytes([payload[6], payload[7]]);
    let packet = Icmpv6EchoPacket {
        src,
        dst,
        ident,
        seq_no,
        payload: payload[ICMPV6_ECHO_HEADER_LEN..].to_vec(),
    };
    match payload[0] {
        128 => Icmpv6Event::EchoRequest(packet),
        129 => Icmpv6Event::EchoReply(packet),
        _ => Icmpv6Event::Unsupported,
    }
}

pub fn build_icmpv6_echo_request_message(packet: &Icmpv6EchoPacket) -> Vec<u8> {
    build_icmpv6_echo_message(packet, true)
}

/// Full IPv6 packet (header + ICMPv6 echo request) for the device-TX lane —
/// mirror of [`build_icmpv4_echo_request`].
pub fn build_icmpv6_echo_request_packet(packet: &Icmpv6EchoPacket) -> LoopbackIpPacket {
    let icmp_bytes = build_icmpv6_echo_request_message(packet);
    let ip_repr = IpRepr::Ipv6(Ipv6Repr {
        src_addr: to_smoltcp_ipv6(packet.src),
        dst_addr: to_smoltcp_ipv6(packet.dst),
        next_header: IpProtocol::Icmpv6,
        payload_len: icmp_bytes.len(),
        hop_limit: 64,
    });
    let ip_header_len = ip_repr.header_len();
    let checksum = ChecksumCapabilities::default();
    let mut bytes = vec![0u8; ip_header_len + icmp_bytes.len()];
    ip_repr.emit(&mut bytes[..ip_header_len], &checksum);
    bytes[ip_header_len..].copy_from_slice(&icmp_bytes);
    LoopbackIpPacket::new(bytes)
}

/// ICMPv6 echo message length (8-byte echo header + payload) — TX-queue
/// accounting, mirror of `icmpv4_echo_message_len`.
fn icmpv6_echo_message_len(packet: &Icmpv6EchoPacket) -> usize {
    8 + packet.payload.len()
}

pub fn build_icmpv6_echo_reply_message(packet: &Icmpv6EchoPacket) -> Vec<u8> {
    build_icmpv6_echo_message(packet, false)
}

pub fn build_icmpv4_echo_request(packet: &Icmpv4EchoPacket) -> LoopbackIpPacket {
    build_icmpv4_echo_packet(packet, true)
}

pub fn build_icmpv4_echo_reply(packet: &Icmpv4EchoPacket) -> LoopbackIpPacket {
    build_icmpv4_echo_packet(packet, false)
}

pub fn build_icmpv4_echo_request_message(packet: &Icmpv4EchoPacket) -> Vec<u8> {
    build_icmpv4_echo_message(packet, true)
}

pub fn build_icmpv4_echo_reply_message(packet: &Icmpv4EchoPacket) -> Vec<u8> {
    build_icmpv4_echo_message(packet, false)
}

pub fn icmpv4_echo_message_len(packet: &Icmpv4EchoPacket) -> usize {
    ICMPV4_ECHO_HEADER_LEN + packet.payload.len()
}

fn icmpv4_echo_raw_packet_len(packet: &Icmpv4EchoPacket) -> usize {
    IPV4_HEADER_LEN + icmpv4_echo_message_len(packet)
}

fn build_icmpv4_echo_packet(packet: &Icmpv4EchoPacket, request: bool) -> LoopbackIpPacket {
    let icmp_bytes = build_icmpv4_echo_message(packet, request);
    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: to_smoltcp_ipv4(packet.src),
        dst_addr: to_smoltcp_ipv4(packet.dst),
        next_header: IpProtocol::Icmp,
        payload_len: icmp_bytes.len(),
        hop_limit: 64,
    });
    let ip_header_len = ip_repr.header_len();
    let checksum = ChecksumCapabilities::default();
    let mut bytes = vec![0u8; ip_header_len + icmp_bytes.len()];
    ip_repr.emit(&mut bytes[..ip_header_len], &checksum);
    bytes[ip_header_len..].copy_from_slice(&icmp_bytes);
    LoopbackIpPacket::new(bytes)
}

fn build_icmpv4_echo_message(packet: &Icmpv4EchoPacket, request: bool) -> Vec<u8> {
    let icmp_repr = if request {
        Icmpv4Repr::EchoRequest {
            ident: packet.ident,
            seq_no: packet.seq_no,
            data: &packet.payload,
        }
    } else {
        Icmpv4Repr::EchoReply {
            ident: packet.ident,
            seq_no: packet.seq_no,
            data: &packet.payload,
        }
    };
    let mut bytes = vec![0u8; icmp_repr.buffer_len()];
    let mut icmp_packet = Icmpv4Packet::new_unchecked(&mut bytes);
    icmp_repr.emit(&mut icmp_packet, &ChecksumCapabilities::default());
    bytes
}

fn build_icmpv6_echo_message(packet: &Icmpv6EchoPacket, request: bool) -> Vec<u8> {
    let icmp_repr = if request {
        Icmpv6Repr::EchoRequest {
            ident: packet.ident,
            seq_no: packet.seq_no,
            data: &packet.payload,
        }
    } else {
        Icmpv6Repr::EchoReply {
            ident: packet.ident,
            seq_no: packet.seq_no,
            data: &packet.payload,
        }
    };
    let mut bytes = vec![0u8; icmp_repr.buffer_len()];
    let mut icmp_packet = Icmpv6Packet::new_unchecked(&mut bytes);
    icmp_repr.emit(
        &to_smoltcp_ipv6(packet.src),
        &to_smoltcp_ipv6(packet.dst),
        &mut icmp_packet,
        &ChecksumCapabilities::default(),
    );
    bytes
}

fn echo_queue_len(queue: &VecDeque<Icmpv4EchoPacket>) -> usize {
    queue.iter().map(icmpv4_echo_message_len).sum()
}

fn raw_ipv6_queue_len(queue: &VecDeque<RawIpv6Packet>) -> usize {
    queue.iter().map(|packet| packet.payload.len()).sum()
}

fn to_smoltcp_ipv4(addr: Ipv4Address) -> SmoltcpIpv4Address {
    let [a, b, c, d] = addr.octets();
    SmoltcpIpv4Address::new(a, b, c, d)
}

fn from_smoltcp_ipv4(addr: SmoltcpIpv4Address) -> Ipv4Address {
    Ipv4Address::new(addr.octets())
}

fn to_smoltcp_ipv6(addr: Ipv6Address) -> SmoltcpIpv6Address {
    SmoltcpIpv6Address::from(addr.octets())
}
