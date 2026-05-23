use smoltcp::time::Instant;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::structure::{SocketIdentity, SocketProtocol, TcpState};

pub use crate::net::structure::TCP_BACKLOG_TIMEOUT_STAGING_MILLIS;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TcpBacklogCleanupOutcome {
    pub scanned: usize,
    pub expired: usize,
    pub failed: usize,
    pub remaining_connecting: usize,
}

pub fn step_tcp_backlog_cleanup(
    listener: &Cap<SocketIdentity>,
    now: Instant,
    _guard: &Guard<'_>,
) -> StepOutcome<TcpBacklogCleanupOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    match cleanup_tcp_backlog_for_listener(listener, now) {
        Ok(outcome) => StepOutcome::Done(outcome),
        Err(errno) => StepOutcome::Err(errno),
    }
}

pub(super) fn cleanup_tcp_backlog_for_listener(
    listener: &Cap<SocketIdentity>,
    now: Instant,
) -> Result<TcpBacklogCleanupOutcome, Errno> {
    let Some(payload) = listener.acquire_operational() else {
        return Err(Errno::ENOTCONN);
    };

    if !matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Listening { .. })
    ) {
        return Err(Errno::EINVAL);
    }

    let (scanned, expired, failed, remaining_connecting) = payload.cleanup_tcp_backlog(now);
    Ok(TcpBacklogCleanupOutcome {
        scanned,
        expired,
        failed,
        remaining_connecting,
    })
}
