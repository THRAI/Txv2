use smoltcp::time::Instant;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::protocol::LoopbackIface;
use crate::net::structure::{
    SocketIdentity, SocketProtocol, TcpBacklogEntry, TcpBacklogRetransmitOutcome, TcpState,
};

pub use crate::net::structure::{
    TCP_BACKLOG_RETRANSMIT_BACKOFF_MILLIS, TCP_BACKLOG_RETRANSMIT_LIMIT_STAGING,
};

pub fn step_tcp_backlog_poll_loopback(
    listener: &Cap<SocketIdentity>,
    now: Instant,
    iface: &LoopbackIface,
    _guard: &Guard<'_>,
) -> StepOutcome<TcpBacklogRetransmitOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    match poll_tcp_backlog_for_listener_loopback(listener, now, iface) {
        Ok(outcome) => StepOutcome::Done(outcome),
        Err(errno) => StepOutcome::Err(errno),
    }
}

pub(super) fn poll_tcp_backlog_for_listener_loopback(
    listener: &Cap<SocketIdentity>,
    now: Instant,
    iface: &LoopbackIface,
) -> Result<TcpBacklogRetransmitOutcome, Errno> {
    let Some(payload) = listener.acquire_operational() else {
        return Err(Errno::ENOTCONN);
    };

    if !matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Listening { .. })
    ) {
        return Err(Errno::EINVAL);
    }

    Ok(payload
        .poll_tcp_backlog_retransmit(now, |entry| retransmit_syn_ack_to_loopback(entry, iface)))
}

fn retransmit_syn_ack_to_loopback(entry: &TcpBacklogEntry, iface: &LoopbackIface) -> bool {
    let Some(child_payload) = entry.child.acquire_operational() else {
        return false;
    };
    let Some(raw_tcp) = child_payload.raw_tcp_socket() else {
        return false;
    };

    // The SYN-ACK retransmit timer lives in smoltcp now (P0 unfroze it).
    // `dispatch_segment` emitting nothing means the RTO simply has not
    // expired yet — that is NOT a failed connection, so return true to keep
    // the backlog entry (false would drop the half-open connection). A
    // child that smoltcp has given up on turns Closed and is culled by
    // `connecting_entry_failed` before this closure runs.
    let Some(segment) = raw_tcp.dispatch_segment() else {
        return true;
    };
    let Some(packet) = segment.emit_ipv4_packet() else {
        return true;
    };

    iface.dispatch_ip(packet)
}
