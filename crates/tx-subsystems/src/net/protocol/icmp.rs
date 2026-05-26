use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::wire::{
    Icmpv4Packet, Icmpv4Repr, IpProtocol, IpRepr, Ipv4Address as SmoltcpIpv4Address, Ipv4Packet,
    Ipv4Repr,
};

use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::{Ipv4Address, SocketOptionSet};
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
pub struct RawIcmpTxDrain {
    pub packet: Icmpv4EchoPacket,
    pub became_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawIcmpRecvDrain {
    pub bytes: usize,
    pub source: Ipv4Address,
    pub destination: Ipv4Address,
    pub truncated: bool,
    pub became_empty: bool,
}

pub struct RawIcmpSocket {
    rx_queue: SpinMutex<VecDeque<Icmpv4EchoPacket>>,
    tx_queue: SpinMutex<VecDeque<Icmpv4EchoPacket>>,
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

impl RawIcmpSocket {
    pub fn new(options: &SocketOptionSet) -> Self {
        Self {
            rx_queue: SpinMutex::new(VecDeque::new()),
            tx_queue: SpinMutex::new(VecDeque::new()),
            recv_capacity: options.socket.recv_buf_size,
            send_capacity: options.socket.send_buf_size,
        }
    }

    pub fn recv_available(&self) -> usize {
        self.rx_queue
            .lock()
            .iter()
            .map(icmpv4_echo_message_len)
            .sum()
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

    pub fn ingest_rx_echo_reply(&self, packet: Icmpv4EchoPacket) -> bool {
        let bytes = icmpv4_echo_message_len(&packet);
        let mut rx = self.rx_queue.lock();
        let was_empty = rx.is_empty();
        let available = self.recv_capacity.saturating_sub(echo_queue_len(&rx));
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

        let mut rx = self.rx_queue.lock();
        let packet = rx.front()?;
        let bytes = core::cmp::min(icmpv4_echo_message_len(packet), len);
        if !peek {
            let _ = rx.pop_front();
        }
        Some((bytes, !peek && rx.is_empty()))
    }

    pub fn recv_echo_reply_bytes(&self, out: &mut [u8], peek: bool) -> Option<RawIcmpRecvDrain> {
        if out.is_empty() {
            return Some(RawIcmpRecvDrain {
                bytes: 0,
                source: Ipv4Address::UNSPECIFIED,
                destination: Ipv4Address::UNSPECIFIED,
                truncated: false,
                became_empty: false,
            });
        }

        let mut rx = self.rx_queue.lock();
        let packet = rx.front()?;
        let message = build_icmpv4_echo_reply_message(packet);
        let bytes = core::cmp::min(message.len(), out.len());
        out[..bytes].copy_from_slice(&message[..bytes]);
        let source = packet.src;
        let destination = packet.dst;
        let truncated = bytes < message.len();
        if !peek {
            let _ = rx.pop_front();
        }
        Some(RawIcmpRecvDrain {
            bytes,
            source,
            destination,
            truncated,
            became_empty: !peek && rx.is_empty(),
        })
    }
}

pub const ICMPV4_ECHO_HEADER_LEN: usize = 8;

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

fn echo_queue_len(queue: &VecDeque<Icmpv4EchoPacket>) -> usize {
    queue.iter().map(icmpv4_echo_message_len).sum()
}

fn to_smoltcp_ipv4(addr: Ipv4Address) -> SmoltcpIpv4Address {
    let [a, b, c, d] = addr.octets();
    SmoltcpIpv4Address::new(a, b, c, d)
}

fn from_smoltcp_ipv4(addr: SmoltcpIpv4Address) -> Ipv4Address {
    Ipv4Address::new(addr.octets())
}
