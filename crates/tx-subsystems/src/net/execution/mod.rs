//! Network net-only execution steps.

use crate::execution::WaitToken;
use crate::net::structure::{AcceptWireSet, RecvWireSet, SendWireSet, SocketIdentity, UrgentEvent};
use tx_substrate::step::{ByteProgress, NoProgress, StepOutcome as V3StepOutcome};

mod step_accept;
mod step_bind;
mod step_connect;
mod step_device_tx;
mod step_flush_pending_arp;
mod step_icmp_loopback;
mod step_listen;
mod step_loopback_pending;
mod step_packet_io;
mod step_poll;
mod step_process_network_events;
mod step_recv;
mod step_send;
mod step_shutdown;
mod step_socket_close;
mod step_socket_create;
mod step_socket_open_file;
mod step_socketpair;
mod step_tcp_backlog_cleanup;
mod step_tcp_backlog_poll;
mod step_tcp_cleanup;
mod step_tcp_close;
mod step_tcp_loopback;
mod step_udp_loopback;

pub use step_accept::{step_accept, SocketAcceptOutcome};
pub use step_bind::step_bind;
pub use step_connect::{step_connect, step_sctp_peeloff};
pub use step_device_tx::{
    step_process_device_tx_pending, step_process_device_tx_pending_at,
    step_process_device_tx_pending_in_namespace_at, DeviceTxBudget, DeviceTxOutcome,
    DEVICE_TX_BUDGET_DEFAULT,
};
pub use step_flush_pending_arp::{
    step_flush_pending_arp, ArpFlushOutcome, ARP_FLUSH_BUDGET_DEFAULT,
};
pub use step_icmp_loopback::{
    step_process_loopback_icmp, step_process_loopback_icmp_on_iface, LoopbackIcmpTransferOutcome,
};
pub use step_listen::step_listen;
pub use step_loopback_pending::{
    step_process_loopback_pending, step_process_loopback_pending_in_namespace,
    step_process_loopback_pending_zero, LoopbackPendingOutcome, LoopbackPollBudget,
    LOOPBACK_POLL_BUDGET_DEFAULT,
};
pub use step_packet_io::{step_packet_ingress_fanout, step_packet_send, PacketIngressOutcome};
pub use step_poll::{step_poll_ready, step_poll_wait_token};
pub use step_process_network_events::{
    step_process_network_events, step_process_network_events_at,
    step_process_network_events_in_namespace_at, step_process_network_tick,
    step_process_network_tick_in_namespace, step_process_network_tick_loopback,
    step_process_network_tick_loopback_in_namespace, NetworkBacklogTickOutcome, NetworkStepOutcome,
    NET_BACKLOG_SCAN_BUDGET, NET_EVENT_BUDGET,
};
pub use step_recv::{step_recv, step_recv_kernel_bytes};
pub use step_send::{
    step_send, step_send_kernel_bytes, step_send_sctp_message, step_send_sctp_seqpacket,
    step_send_to_kernel_bytes, step_send_to_kernel_bytes_with_poll_kick,
    step_send_to_unix_path_kernel_bytes,
};
pub use step_shutdown::{step_shutdown, ShutdownOutcome};
pub use step_socket_close::{step_sctp_shutdown_assoc, step_socket_close, SocketCloseOutcome};
pub use step_socket_create::{step_socket_create, step_socket_create_in_namespace};
pub use step_socket_open_file::{
    socket_open_file_from_identity, step_socket_open_file, step_socket_open_file_in_namespace,
    SocketOpenFileOutput,
};
pub use step_socketpair::step_unix_socketpair_connect;
pub use step_tcp_backlog_cleanup::{
    step_tcp_backlog_cleanup, TcpBacklogCleanupOutcome, TCP_BACKLOG_TIMEOUT_STAGING_MILLIS,
};
pub use step_tcp_backlog_poll::{
    step_tcp_backlog_poll_loopback, TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS,
    TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING,
};
pub use step_tcp_cleanup::{step_tcp_connection_cleanup, TcpConnectionCleanupOutcome};
pub use step_tcp_close::{step_tcp_close_staging, TcpCloseStagingOutcome};
pub use step_tcp_loopback::{
    step_process_loopback_tcp, step_tcp_loopback_handshake, step_tcp_loopback_handshake_on_iface,
    step_tcp_loopback_transfer, LoopbackTcpConnectOutcome, LoopbackTcpHandshakeStats,
    LoopbackTcpTransferOutcome,
};
pub use step_udp_loopback::{
    step_process_loopback_udp, step_process_loopback_udp_on_iface,
    step_send_udp_loopback_kernel_bytes, step_send_udp_loopback_kernel_bytes_on_iface,
    LoopbackUdpTransferOutcome,
};

pub const SOMAXCONN_STAGING: usize = 128;

pub type ByteStepOutcome<T> = V3StepOutcome<T, ByteProgress>;

pub(crate) fn yield_on_token<T>(token: WaitToken) -> crate::execution::StepOutcome<T> {
    V3StepOutcome::yield_on_wait_source(NoProgress, token.source_id(), token.interest())
}

pub(crate) fn yield_bytes_on_token<T>(
    progress: ByteProgress,
    token: WaitToken,
) -> ByteStepOutcome<T> {
    V3StepOutcome::yield_on_wait_source(progress, token.source_id(), token.interest())
}

pub fn socket_recv_wait_token(socket: &SocketIdentity) -> WaitToken {
    WaitToken::new(
        socket.wait_carriers.recv,
        RecvWireSet::HAS_DATA.bits() | RecvWireSet::BROKEN.bits(),
    )
}

pub fn socket_send_wait_token(socket: &SocketIdentity) -> WaitToken {
    WaitToken::new(
        socket.wait_carriers.send,
        SendWireSet::SPACE.bits() | SendWireSet::BROKEN.bits() | SendWireSet::CONNECT_DONE.bits(),
    )
}

pub fn socket_accept_wait_token(socket: &SocketIdentity) -> WaitToken {
    WaitToken::new(
        socket.wait_carriers.accept,
        AcceptWireSet::HAS_PENDING.bits() | AcceptWireSet::BROKEN.bits(),
    )
}

pub fn socket_urgent_wait_token(socket: &SocketIdentity) -> WaitToken {
    WaitToken::new(socket.wait_carriers.urgent, UrgentEvent::URGENT.bits())
}

/// Build a `struct sctp_assoc_change` notification (20 bytes, native/LE) for the
/// given `sac_state` (0=COMM_UP, 1=COMM_LOST, 3=SHUTDOWN_COMP) and stream count.
/// `sn_type` is SCTP_ASSOC_CHANGE (0x8001).
pub fn sctp_assoc_change_bytes(state: u16, streams: u16, assoc_id: u32) -> alloc::vec::Vec<u8> {
    let mut b = alloc::vec![0u8; 20];
    b[0..2].copy_from_slice(&0x8001u16.to_le_bytes()); // sac_type = SCTP_ASSOC_CHANGE
    b[4..8].copy_from_slice(&20u32.to_le_bytes()); // sac_length
    b[8..10].copy_from_slice(&state.to_le_bytes()); // sac_state
    b[12..14].copy_from_slice(&streams.to_le_bytes()); // sac_outbound_streams
    b[14..16].copy_from_slice(&streams.to_le_bytes()); // sac_inbound_streams
    b[16..20].copy_from_slice(&assoc_id.to_le_bytes()); // sac_assoc_id
    b
}

/// Build a COMM_LOST `sctp_assoc_change` for an ungraceful ABORT. lksctp copies
/// the ABORT chunk that caused the loss into `sac_info[]`, so the total length is
/// `sizeof(struct sctp_assoc_change) + 4 = 24` (what `test_assoc_abort` asserts),
/// and `sac_state` is `SCTP_COMM_LOST` (1).
pub fn sctp_assoc_change_abort_bytes(streams: u16, assoc_id: u32) -> alloc::vec::Vec<u8> {
    let mut b = alloc::vec![0u8; 24];
    b[0..2].copy_from_slice(&0x8001u16.to_le_bytes()); // sac_type = SCTP_ASSOC_CHANGE
    b[4..8].copy_from_slice(&24u32.to_le_bytes()); // sac_length = 24 (20 + 4-byte sac_info)
    b[8..10].copy_from_slice(&1u16.to_le_bytes()); // sac_state = SCTP_COMM_LOST
    b[12..14].copy_from_slice(&streams.to_le_bytes()); // sac_outbound_streams
    b[14..16].copy_from_slice(&streams.to_le_bytes()); // sac_inbound_streams
    b[16..20].copy_from_slice(&assoc_id.to_le_bytes()); // sac_assoc_id
                                                        // sac_info[]: the 4-byte ABORT chunk header (type=6, flags=0, length=4).
    b[20] = 6;
    b[23] = 4;
    b
}

/// Build an `SCTP_SEND_FAILED` (0x8003) notification for one abandoned message
/// fragment. `struct sctp_send_failed` is a 48-byte header (ssf_type@0,
/// ssf_flags@2, ssf_length@4, ssf_error@8, ssf_info=sctp_sndrcvinfo@12,
/// ssf_assoc_id@44) followed by `ssf_data` (the fragment). `last` sets
/// `ssf_info.sinfo_flags` bit 0 (SCTP_DATA_LAST_FRAG @ offset 12+4=16), which the
/// lksctp ttl tests loop on to find the final fragment.
pub fn sctp_send_failed_bytes(
    data: &[u8],
    stream: u16,
    ppid: u32,
    assoc_id: u32,
    last: bool,
) -> alloc::vec::Vec<u8> {
    let total = 48 + data.len();
    let mut b = alloc::vec![0u8; total];
    b[0..2].copy_from_slice(&0x8003u16.to_le_bytes()); // ssf_type = SCTP_SEND_FAILED
    b[4..8].copy_from_slice(&(total as u32).to_le_bytes()); // ssf_length
                                                            // ssf_info (sctp_sndrcvinfo) @12: sinfo_stream@12, sinfo_flags@16, sinfo_ppid@20.
    b[12..14].copy_from_slice(&stream.to_le_bytes());
    if last {
        b[16..18].copy_from_slice(&0x0001u16.to_le_bytes()); // SCTP_DATA_LAST_FRAG
    }
    b[20..24].copy_from_slice(&ppid.to_le_bytes());
    b[44..48].copy_from_slice(&assoc_id.to_le_bytes()); // ssf_assoc_id
    b[48..total].copy_from_slice(data);
    b
}

/// Build a `struct sctp_shutdown_event` notification (12 bytes, native/LE).
/// `sn_type` is SCTP_SHUTDOWN_EVENT (0x8005).
pub fn sctp_shutdown_event_bytes() -> alloc::vec::Vec<u8> {
    let mut b = alloc::vec![0u8; 12];
    b[0..2].copy_from_slice(&0x8005u16.to_le_bytes()); // sse_type = SCTP_SHUTDOWN_EVENT
    b[4..8].copy_from_slice(&12u32.to_le_bytes()); // sse_length
    b
}
