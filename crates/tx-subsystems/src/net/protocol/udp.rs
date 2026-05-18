use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpProtocol, IpRepr, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr};

use crate::net::packet::LoopbackIpPacket;
use crate::net::structure::{IpEndpoint, Ipv4Address, SocketOptionSet};
use crate::sync::SpinMutex;

const MAX_UDP_PACKET_METADATA_CAPACITY: usize = 64;
const UDP_PACKET_CAPACITY_DIVISOR: usize = 1500;

/// Doc-named owner for the smoltcp UDP socket and its packet buffers.
pub struct RawUdpSocket {
    socket: SpinMutex<Box<udp::Socket<'static>>>,
    rx_datagrams: SpinMutex<VecDeque<UdpRxDatagram>>,
    tx_datagrams: SpinMutex<VecDeque<UdpTxDatagram>>,
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
        let recv_capacity = options.socket.recv_buf_size;
        let send_capacity = options.socket.send_buf_size;
        let recv_packet_capacity = packet_capacity_for_bytes(recv_capacity);
        let send_packet_capacity = packet_capacity_for_bytes(send_capacity);
        let rx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; recv_packet_capacity],
            vec![0u8; recv_capacity],
        );
        let tx_buf = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; send_packet_capacity],
            vec![0u8; send_capacity],
        );
        let mut socket = udp::Socket::new(rx_buf, tx_buf);

        if options.ip.ttl != 0 {
            socket.set_hop_limit(Some(options.ip.ttl));
        }

        Self {
            socket: SpinMutex::new(Box::new(socket)),
            rx_datagrams: SpinMutex::new(VecDeque::new()),
            tx_datagrams: SpinMutex::new(VecDeque::new()),
            recv_capacity,
            send_capacity,
            recv_packet_capacity,
            send_packet_capacity,
        }
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

        let mut rx = self.rx_datagrams.lock();
        let was_empty = rx.is_empty();
        let available = self.recv_capacity.saturating_sub(rx_payload_len(&rx));
        if payload.len() > available {
            return false;
        }

        rx.push_back(UdpRxDatagram { src, dst, payload });
        was_empty
    }

    pub fn recv_available(&self) -> usize {
        rx_payload_len(&self.rx_datagrams.lock())
    }

    pub fn recv_len(&self, len: usize, peek: bool) -> Option<(usize, bool)> {
        if len == 0 {
            return Some((0, false));
        }

        let mut rx = self.rx_datagrams.lock();
        let datagram = rx.front()?;
        let bytes = core::cmp::min(datagram.payload.len(), len);
        if !peek {
            let _ = rx.pop_front();
        }
        Some((bytes, !peek && rx.is_empty()))
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

        let mut rx = self.rx_datagrams.lock();
        let datagram = rx.front()?;
        let bytes = core::cmp::min(datagram.payload.len(), out.len());
        out[..bytes].copy_from_slice(&datagram.payload[..bytes]);
        let source = datagram.src;
        let destination = datagram.dst;
        let truncated = bytes < datagram.payload.len();
        if !peek {
            let _ = rx.pop_front();
        }
        Some(UdpRecvDrain {
            bytes,
            source,
            destination,
            truncated,
            became_empty: !peek && rx.is_empty(),
        })
    }

    pub fn send_available(&self) -> usize {
        self.send_capacity
            .saturating_sub(tx_payload_len(&self.tx_datagrams.lock()))
    }

    pub fn enqueue_tx_len(&self, len: usize) -> Option<(usize, bool)> {
        self.enqueue_tx_len_to(unspecified_endpoint(), len)
    }

    pub fn enqueue_tx_len_to(&self, dst: IpEndpoint, len: usize) -> Option<(usize, bool)> {
        let bytes = vec![0u8; len];
        self.enqueue_tx_datagram(dst, bytes)
    }

    pub fn enqueue_tx_datagram(&self, dst: IpEndpoint, payload: Vec<u8>) -> Option<(usize, bool)> {
        if payload.is_empty() {
            return Some((0, false));
        }

        let mut tx = self.tx_datagrams.lock();
        let available = self.send_capacity.saturating_sub(tx_payload_len(&tx));
        if payload.len() > available {
            return None;
        }

        let bytes = payload.len();
        tx.push_back(UdpTxDatagram { dst, payload });
        Some((bytes, tx_payload_len(&tx) == self.send_capacity))
    }

    pub fn enqueue_tx_bytes(&self, bytes: &[u8]) -> Option<(usize, bool)> {
        self.enqueue_tx_datagram(unspecified_endpoint(), bytes.to_vec())
    }

    pub fn enqueue_tx_bytes_to(&self, dst: IpEndpoint, bytes: &[u8]) -> Option<(usize, bool)> {
        self.enqueue_tx_datagram(dst, bytes.to_vec())
    }

    pub fn pop_tx_datagram(&self) -> Option<UdpTxDatagramDrain> {
        let mut tx = self.tx_datagrams.lock();
        let datagram = tx.pop_front()?;
        Some(UdpTxDatagramDrain {
            datagram,
            became_available: true,
        })
    }

    pub fn peek_tx_datagram(&self) -> Option<UdpTxDatagram> {
        self.tx_datagrams.lock().front().cloned()
    }

    pub fn commit_tx_datagram_sent(&self) -> Option<UdpTxDatagramDrain> {
        self.pop_tx_datagram()
    }

    pub fn recv_packet_capacity(&self) -> usize {
        self.recv_packet_capacity
    }

    pub fn send_packet_capacity(&self) -> usize {
        self.send_packet_capacity
    }

    pub fn can_recv(&self) -> bool {
        self.socket.lock().can_recv()
    }

    pub fn can_send(&self) -> bool {
        self.socket.lock().can_send()
    }

    pub fn close(&self) {
        self.socket.lock().close();
    }
}

impl UdpRxDatagram {
    pub fn parse_ipv4_packet(packet: &LoopbackIpPacket) -> Option<Self> {
        let checksum_caps = ChecksumCapabilities::default();
        let ipv4 = Ipv4Packet::new_checked(packet.as_bytes()).ok()?;
        let ipv4_repr = Ipv4Repr::parse(&ipv4, &checksum_caps).ok()?;
        if ipv4_repr.next_header != IpProtocol::Udp {
            return None;
        }

        let udp_packet = UdpPacket::new_checked(ipv4.payload()).ok()?;
        let src_addr = IpAddress::Ipv4(ipv4_repr.src_addr);
        let dst_addr = IpAddress::Ipv4(ipv4_repr.dst_addr);
        let udp_repr = UdpRepr::parse(&udp_packet, &src_addr, &dst_addr, &checksum_caps).ok()?;
        Some(Self {
            src: IpEndpoint::new(from_smoltcp_ipv4(ipv4_repr.src_addr), udp_repr.src_port),
            dst: IpEndpoint::new(from_smoltcp_ipv4(ipv4_repr.dst_addr), udp_repr.dst_port),
            payload: udp_packet.payload().to_vec(),
        })
    }
}

impl UdpTxDatagram {
    pub fn emit_ipv4_packet(&self, src: IpEndpoint) -> Option<LoopbackIpPacket> {
        if src.port == 0 || self.dst.port == 0 || self.payload.is_empty() {
            return None;
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
}

fn packet_capacity_for_bytes(bytes: usize) -> usize {
    let packet_count = bytes.div_ceil(UDP_PACKET_CAPACITY_DIVISOR);
    packet_count.clamp(1, MAX_UDP_PACKET_METADATA_CAPACITY)
}

fn rx_payload_len(datagrams: &VecDeque<UdpRxDatagram>) -> usize {
    datagrams
        .iter()
        .map(|datagram| datagram.payload.len())
        .sum()
}

fn tx_payload_len(datagrams: &VecDeque<UdpTxDatagram>) -> usize {
    datagrams
        .iter()
        .map(|datagram| datagram.payload.len())
        .sum()
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
