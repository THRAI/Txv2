use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::structure::{
    SocketIdentity, SocketKind, SocketProtocol, UnixDatagramState, UnixStreamState,
};

pub fn step_unix_socketpair_connect(
    first: &Cap<SocketIdentity>,
    second: &Cap<SocketIdentity>,
    _guard: &Guard<'_>,
) -> StepOutcome<()> {
    let Some(first_payload) = first.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let Some(second_payload) = second.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if first.kind != second.kind {
        return StepOutcome::Err(Errno::EINVAL);
    }

    match first.kind {
        SocketKind::UnixStream => {
            if !matches!(
                first_payload.protocol_snapshot(),
                SocketProtocol::UnixStream(UnixStreamState::Init)
            ) || !matches!(
                second_payload.protocol_snapshot(),
                SocketProtocol::UnixStream(UnixStreamState::Init)
            ) {
                return StepOutcome::Err(Errno::EINVAL);
            }
            if let Err(outcome) = insert_peer_pair(first, second, &first_payload) {
                return outcome;
            }
            first_payload.with_protocol_mut(|protocol| {
                *protocol = SocketProtocol::UnixStream(UnixStreamState::Connected {
                    local: None,
                    peer_raw: second.raw(),
                });
            });
            second_payload.with_protocol_mut(|protocol| {
                *protocol = SocketProtocol::UnixStream(UnixStreamState::Connected {
                    local: None,
                    peer_raw: first.raw(),
                });
            });
            StepOutcome::Done(())
        }
        SocketKind::UnixDatagram => {
            if !matches!(
                first_payload.protocol_snapshot(),
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound)
            ) || !matches!(
                second_payload.protocol_snapshot(),
                SocketProtocol::UnixDatagram(UnixDatagramState::Unbound)
            ) {
                return StepOutcome::Err(Errno::EINVAL);
            }
            if let Err(outcome) = insert_peer_pair(first, second, &first_payload) {
                return outcome;
            }
            first_payload.with_protocol_mut(|protocol| {
                *protocol = SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair {
                    peer_raw: second.raw(),
                });
            });
            second_payload.with_protocol_mut(|protocol| {
                *protocol = SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair {
                    peer_raw: first.raw(),
                });
            });
            StepOutcome::Done(())
        }
        _ => StepOutcome::Err(Errno::EOPNOTSUPP),
    }
}

fn insert_peer_pair(
    first: &Cap<SocketIdentity>,
    second: &Cap<SocketIdentity>,
    first_payload: &crate::net::structure::SocketOperationalEvidence,
) -> Result<(), StepOutcome<()>> {
    let table = first_payload.socket_table();
    if table.insert_unix_peer(first.raw(), second.clone()).is_err() {
        return Err(StepOutcome::Err(Errno::ENOMEM));
    }
    if table.insert_unix_peer(second.raw(), first.clone()).is_err() {
        let _ = table.withdraw_unix_peer(first.raw());
        return Err(StepOutcome::Err(Errno::ENOMEM));
    }
    Ok(())
}
