//! Protocol adapter staging layer.

mod ether;
mod icmp;
mod loopback;
mod poll_context;
mod smoltcp_adapter;
mod tcp;
mod udp;

pub use ether::{
    decide_ipv4_route, ArpEntry, ArpFlushOutcome, ArpPendingEntry, ArpSnapshotEntry,
    ArpSnapshotState, ArpStats, EtherIface, EtherPacketSource, EtherPacketTxSink,
    Ipv4RouteDecision, NetStats, NetStatsSnapshot, ARP_CACHE_TTL, ARP_REQUEST_RETRY_DELAY,
    ARP_REQUEST_RETRY_LIMIT,
};
pub use icmp::{
    build_icmpv4_echo_reply, build_icmpv4_echo_reply_message, build_icmpv4_echo_request,
    build_icmpv4_echo_request_message, icmpv4_echo_message_len,
    parse_icmpv4_echo_payload_unchecked, parse_icmpv4_from_ipv4_bytes,
    parse_icmpv4_loopback_packet, parse_icmpv4_payload, parse_raw_icmpv4_echo_payload_unchecked,
    Icmpv4EchoPacket, Icmpv4Event, RawIcmpSocket, RawIcmpTxDrain, RawIpAddress, RawIpv6Packet,
};
pub use loopback::{loopback_iface, IfaceCommon, LoopbackIface};
pub use poll_context::{PollContext, PollContextOutcome};
pub use smoltcp_adapter::{
    SmoltcpAdapter, SmoltcpAdapterConfig, SmoltcpPacketSource, SmoltcpPacketTxSink,
};
pub use tcp::{RawTcpSocket, SmoltcpTcpSegment, TCP_CORK_AUTO_FLUSH_BYTES};
pub use udp::{RawUdpSocket, UdpRxDatagram, UdpTxDatagram, UDP_IPV4_MAX_PAYLOAD_BYTES};
