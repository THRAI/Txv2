use alloc::vec::Vec;

use crate::net::protocol::Icmpv4Event;
use crate::net::structure::IpEndpoint;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PacketDispatch {
    Tcp(TcpPacketEvent),
    Udp(UdpPacketEvent),
    Icmp(Icmpv4Event),
    Unsupported,
    Malformed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TcpPacketEvent {
    pub src: IpEndpoint,
    pub dst: IpEndpoint,
    pub flags: TcpPacketFlags,
    pub payload: Vec<u8>,
    pub urgent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UdpPacketEvent {
    pub src: IpEndpoint,
    pub dst: IpEndpoint,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TcpPacketFlags {
    pub syn: bool,
    pub ack: bool,
    pub rst: bool,
}

impl TcpPacketEvent {
    pub fn new(
        src: IpEndpoint,
        dst: IpEndpoint,
        flags: TcpPacketFlags,
        payload: Vec<u8>,
        urgent: bool,
    ) -> Self {
        Self {
            src,
            dst,
            flags,
            payload,
            urgent,
        }
    }

    pub fn with_payload_len(
        src: IpEndpoint,
        dst: IpEndpoint,
        flags: TcpPacketFlags,
        payload_len: usize,
        urgent: bool,
    ) -> Self {
        Self::new(src, dst, flags, alloc::vec![0u8; payload_len], urgent)
    }

    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }
}

impl UdpPacketEvent {
    pub fn new(src: IpEndpoint, dst: IpEndpoint, payload: Vec<u8>) -> Self {
        Self { src, dst, payload }
    }

    pub fn with_payload_len(src: IpEndpoint, dst: IpEndpoint, payload_len: usize) -> Self {
        Self::new(src, dst, alloc::vec![0u8; payload_len])
    }

    pub fn payload_len(&self) -> usize {
        self.payload.len()
    }
}
