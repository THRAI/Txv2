use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, TcpPacket, UdpPacket,
};

use crate::net::packet::{
    LoopbackIpPacket, PacketDispatch, RxFrame, TcpPacketEvent, TcpPacketFlags, UdpPacketEvent,
};
use crate::net::protocol::{parse_icmpv4_payload, SmoltcpTcpSegment};
use crate::net::structure::{IpEndpoint, Ipv4Address};

pub fn demux_rx_frame_with_smoltcp(frame: &RxFrame) -> PacketDispatch {
    let ethernet = match EthernetFrame::new_checked(frame.as_bytes()) {
        Ok(frame) => frame,
        Err(_) => return PacketDispatch::Malformed,
    };

    match ethernet.ethertype() {
        EthernetProtocol::Ipv4 => demux_ipv4(ethernet.payload()),
        EthernetProtocol::Arp | EthernetProtocol::Ipv6 | EthernetProtocol::Unknown(_) => {
            PacketDispatch::Unsupported
        }
    }
}

fn demux_ipv4(packet: &[u8]) -> PacketDispatch {
    let ipv4 = match Ipv4Packet::new_checked(packet) {
        Ok(packet) => packet,
        Err(_) => return PacketDispatch::Malformed,
    };

    match ipv4.next_header() {
        IpProtocol::Tcp => demux_tcp(&ipv4, packet),
        IpProtocol::Udp => demux_udp(&ipv4),
        IpProtocol::Icmp => PacketDispatch::Icmp(parse_icmpv4_payload(
            local_ipv4(ipv4.src_addr()),
            local_ipv4(ipv4.dst_addr()),
            ipv4.payload(),
        )),
        _ => PacketDispatch::Unsupported,
    }
}

fn demux_tcp(ipv4: &Ipv4Packet<&[u8]>, ip_bytes: &[u8]) -> PacketDispatch {
    let packet = match TcpPacket::new_checked(ipv4.payload()) {
        Ok(packet) => packet,
        Err(_) => return PacketDispatch::Malformed,
    };
    let src = IpEndpoint::new(local_ipv4(ipv4.src_addr()), packet.src_port());
    let dst = IpEndpoint::new(local_ipv4(ipv4.dst_addr()), packet.dst_port());

    // Full segment (seq/ack/window, checksum-verified) so established
    // connections feed smoltcp `process_segment` instead of the old
    // bare-byte rx bypass.
    let segment = SmoltcpTcpSegment::parse_ipv4_packet(&LoopbackIpPacket::new(ip_bytes.to_vec()));

    PacketDispatch::Tcp(
        TcpPacketEvent::new(
            src,
            dst,
            TcpPacketFlags {
                syn: packet.syn(),
                ack: packet.ack(),
                rst: packet.rst(),
            },
            packet.payload().to_vec(),
            packet.urg(),
        )
        .with_segment(segment),
    )
}

fn demux_udp(ipv4: &Ipv4Packet<&[u8]>) -> PacketDispatch {
    let packet = match UdpPacket::new_checked(ipv4.payload()) {
        Ok(packet) => packet,
        Err(_) => return PacketDispatch::Malformed,
    };
    let src = IpEndpoint::new(local_ipv4(ipv4.src_addr()), packet.src_port());
    let dst = IpEndpoint::new(local_ipv4(ipv4.dst_addr()), packet.dst_port());

    PacketDispatch::Udp(UdpPacketEvent::new(src, dst, packet.payload().to_vec()))
}

fn local_ipv4(addr: smoltcp::wire::Ipv4Address) -> Ipv4Address {
    Ipv4Address::new(addr.octets())
}
