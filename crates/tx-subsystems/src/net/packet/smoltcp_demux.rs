use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, Ipv6Packet, TcpPacket, UdpPacket,
};

use crate::net::packet::{
    LoopbackIpPacket, PacketDispatch, RxFrame, TcpPacketEvent, TcpPacketFlags, UdpPacketEvent,
};
use crate::net::protocol::{parse_icmpv4_payload, SmoltcpTcpSegment};
use crate::net::structure::{IpEndpoint, Ipv4Address, Ipv6Address};

pub fn demux_rx_frame_with_smoltcp(frame: &RxFrame) -> PacketDispatch {
    let ethernet = match EthernetFrame::new_checked(frame.as_bytes()) {
        Ok(frame) => frame,
        Err(_) => return PacketDispatch::Malformed,
    };

    match ethernet.ethertype() {
        EthernetProtocol::Ipv4 => demux_ipv4(ethernet.payload()),
        // P2-S7 (§6-2-A): IPv6 TCP/UDP frames reach the same event path —
        // the segment/datagram parsers have been dual-family since P1-S4.
        // ICMPv6/NDISC stay control-plane work (P4); static NDISC covers
        // neighbour resolution until then.
        EthernetProtocol::Ipv6 => demux_ipv6(ethernet.payload()),
        EthernetProtocol::Arp | EthernetProtocol::Unknown(_) => PacketDispatch::Unsupported,
    }
}

fn demux_ipv6(packet: &[u8]) -> PacketDispatch {
    let ipv6 = match Ipv6Packet::new_checked(packet) {
        Ok(packet) => packet,
        Err(_) => return PacketDispatch::Malformed,
    };

    match ipv6.next_header() {
        IpProtocol::Tcp => demux_tcp_v6(&ipv6, packet),
        IpProtocol::Udp => demux_udp_v6(&ipv6),
        _ => PacketDispatch::Unsupported,
    }
}

fn demux_tcp_v6(ipv6: &Ipv6Packet<&[u8]>, ip_bytes: &[u8]) -> PacketDispatch {
    let packet = match TcpPacket::new_checked(ipv6.payload()) {
        Ok(packet) => packet,
        Err(_) => return PacketDispatch::Malformed,
    };
    let src = IpEndpoint::new_v6(local_ipv6(ipv6.src_addr()), packet.src_port());
    let dst = IpEndpoint::new_v6(local_ipv6(ipv6.dst_addr()), packet.dst_port());

    // Same full-segment contract as the v4 arm (the parser despite its
    // name handles both families).
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

fn demux_udp_v6(ipv6: &Ipv6Packet<&[u8]>) -> PacketDispatch {
    let packet = match UdpPacket::new_checked(ipv6.payload()) {
        Ok(packet) => packet,
        Err(_) => return PacketDispatch::Malformed,
    };
    let src = IpEndpoint::new_v6(local_ipv6(ipv6.src_addr()), packet.src_port());
    let dst = IpEndpoint::new_v6(local_ipv6(ipv6.dst_addr()), packet.dst_port());

    PacketDispatch::Udp(UdpPacketEvent::new(src, dst, packet.payload().to_vec()))
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

fn local_ipv6(addr: smoltcp::wire::Ipv6Address) -> Ipv6Address {
    Ipv6Address::new(addr.octets())
}
