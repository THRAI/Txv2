use tx_substrate::mutation::MutationError;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::structure::table::SocketTable;
use crate::net::structure::{ConnectionKey, SocketIdentity, SocketProtocol, TcpState};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TcpConnectionCleanupOutcome {
    pub local_withdrawn: bool,
    pub peer_withdrawn: bool,
    pub was_connected: bool,
}

pub fn step_tcp_connection_cleanup(
    socket: &Cap<SocketIdentity>,
    _guard: &Guard<'_>,
) -> StepOutcome<TcpConnectionCleanupOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    match cleanup_tcp_connection(socket) {
        Ok(outcome) => StepOutcome::Done(outcome),
        Err(errno) => StepOutcome::Err(errno),
    }
}

pub(crate) fn cleanup_tcp_connection(
    socket: &Cap<SocketIdentity>,
) -> Result<TcpConnectionCleanupOutcome, Errno> {
    let Some(payload) = socket.acquire_operational() else {
        return Err(Errno::ENOTCONN);
    };

    let SocketProtocol::Tcp(TcpState::Connected { local, remote }) = payload.protocol_snapshot()
    else {
        return Ok(TcpConnectionCleanupOutcome::default());
    };

    // R2c: withdraw from the socket's OWN netns table, not the global
    // initial-ns SOCKET_TABLE. The graceful-close path routed here for a
    // non-initial-netns connection was hitting the wrong table, leaving
    // the real ns entry (and its strong Cap → R2f) undeleted.
    let table = payload.socket_table();
    let local_withdrawn = withdraw_connection_key(table, ConnectionKey::new(local, remote));
    let peer_withdrawn = withdraw_connection_key(table, ConnectionKey::new(remote, local));

    payload.with_protocol_mut(|protocol| {
        if matches!(
            protocol,
            SocketProtocol::Tcp(TcpState::Connected {
                local: state_local,
                remote: state_remote,
            }) if *state_local == local && *state_remote == remote
        ) {
            *protocol = SocketProtocol::Tcp(TcpState::Closed);
        }
    });
    if let Some(raw_tcp) = payload.raw_tcp_socket() {
        raw_tcp.abort();
    }

    Ok(TcpConnectionCleanupOutcome {
        local_withdrawn,
        peer_withdrawn,
        was_connected: true,
    })
}

fn withdraw_connection_key(table: &SocketTable, key: ConnectionKey) -> bool {
    match table.withdraw_tcp_connection(key) {
        Ok(_) => true,
        Err(
            MutationError::Missing
            | MutationError::AlreadyPresent
            | MutationError::Full
            | MutationError::Busy,
        ) => false,
    }
}
