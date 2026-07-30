//! Protocol adapter staging layer.

mod ether;
mod icmp;
mod loopback;
mod poll_context;
mod smoltcp_adapter;
mod tcp;
mod udp;

pub use ether::{
    decide_ipv4_route, decide_ipv6_route, ArpEntry, ArpFlushOutcome, ArpPendingEntry,
    ArpSnapshotEntry, ArpSnapshotState, ArpStats, EtherIface, EtherPacketSource, EtherPacketTxSink,
    Ipv4RouteDecision, Ipv6RouteDecision, NdiscEntry, NdiscSnapshotEntry, NetStats,
    NetStatsSnapshot, ARP_CACHE_TTL, ARP_REQUEST_RETRY_DELAY, ARP_REQUEST_RETRY_LIMIT,
};
pub use icmp::{
    build_icmpv4_echo_reply, build_icmpv4_echo_reply_message, build_icmpv4_echo_request,
    build_icmpv4_echo_request_message, build_icmpv6_echo_reply_message,
    build_icmpv6_echo_request_message, build_icmpv6_echo_request_packet,
    icmpv4_echo_message_len,
    parse_icmpv4_echo_payload_unchecked, parse_icmpv4_from_ipv4_bytes,
    parse_icmpv4_loopback_packet, parse_icmpv4_payload, parse_icmpv6_payload_unchecked,
    parse_raw_icmpv4_echo_payload_unchecked, Icmpv4EchoPacket, Icmpv4Event, Icmpv6EchoPacket,
    Icmpv6Event, RawIcmpSocket, RawIcmpTxDrain, RawIpAddress, RawIpv6Packet,
};
pub use loopback::{loopback_iface, IfaceCommon, LoopbackIface};
pub use poll_context::{PollContext, PollContextOutcome};
pub(crate) use poll_context::{
    is_first_syn, listener_accepts_incoming, promote_connected_stream_and_publish_accept,
    TcpConnectedPromotion,
};
pub use smoltcp_adapter::{
    SmoltcpAdapter, SmoltcpAdapterConfig, SmoltcpPacketSource, SmoltcpPacketTxSink,
};
#[cfg(test)]
pub(crate) use tcp::TCP_CONNECT_TIMEOUT;
pub use tcp::{RawTcpSocket, SmoltcpTcpSegment, TCP_CORK_AUTO_FLUSH_BYTES};
pub use udp::{RawUdpSocket, UdpRxDatagram, UdpTxDatagram, UDP_IPV4_MAX_PAYLOAD_BYTES};
