use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome, WaitToken};
use crate::net::checks::require::require_socket_poll_target;
use crate::net::execution::{
    socket_accept_wait_token, socket_recv_wait_token, socket_send_wait_token,
};
use crate::net::structure::{
    AcceptWireSet, PollMask, RdsState, RecvWireSet, SendWireSet, SocketIdentity, SocketProtocol,
    TcpState, UdpInner, UnixDatagramState, UnixStreamState,
};

pub fn step_poll_ready(socket: &Cap<SocketIdentity>, guard: &Guard<'_>) -> StepOutcome<PollMask> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_poll_target(socket, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Done(PollMask::HUP | PollMask::ERR);
    };

    let mut mask = PollMask::empty();
    if payload.shutdown_rd() {
        mask |= PollMask::RDHUP;
        mask |= PollMask::HUP;
    }
    if payload.shutdown_wr() {
        mask |= PollMask::ERR;
    }

    payload.with_protocol(|protocol| match protocol {
        SocketProtocol::Tcp(TcpState::Listening { .. }) => {
            let io = payload.io_snapshot();
            if io.accept_pending > 0
                || witness.identity.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits()
                    != 0
            {
                mask |= PollMask::IN;
            }
        }
        SocketProtocol::Sctp(TcpState::Listening { .. }) => {
            let io = payload.io_snapshot();
            if io.accept_pending > 0
                || witness.identity.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits()
                    != 0
            {
                mask |= PollMask::IN;
            }
        }
        SocketProtocol::UnixStream(UnixStreamState::Listening { .. }) => {
            let io = payload.io_snapshot();
            if io.accept_pending > 0
                || witness.identity.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits()
                    != 0
            {
                mask |= PollMask::IN;
            }
        }
        SocketProtocol::Tcp(TcpState::Connected { .. }) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if witness.identity.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0
                || payload.tcp_recv_closed_by_peer()
            {
                mask |= PollMask::IN | PollMask::RDHUP;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::UnixStream(UnixStreamState::Connected { .. }) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if witness.identity.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0 {
                mask |= PollMask::IN | PollMask::RDHUP;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::Sctp(TcpState::Connected { .. }) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if witness.identity.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0 {
                mask |= PollMask::IN | PollMask::RDHUP;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::Udp(UdpInner::Bound { .. } | UdpInner::Connected { .. }) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::UnixDatagram(
            UnixDatagramState::Bound { .. }
            | UnixDatagramState::Connected { .. }
            | UnixDatagramState::ConnectedPair { .. },
        ) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::RawIcmp(_) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::Rds(RdsState::Bound { .. }) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => {
            let io = payload.io_snapshot();
            if io.recv_len > 0
                || witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0
            {
                mask |= PollMask::IN;
            }
            if io.send_space > 0 {
                mask |= PollMask::OUT;
            }
        }
        _ => {}
    });

    if witness.identity.readiness.send_wq.peek() & SendWireSet::BROKEN.bits() != 0 {
        mask |= PollMask::OUT | PollMask::ERR;
    }

    StepOutcome::Done(mask)
}

pub fn step_poll_wait_token(
    socket: &Cap<SocketIdentity>,
    interests: PollMask,
    guard: &Guard<'_>,
) -> StepOutcome<Option<WaitToken>> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_poll_target(socket, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Done(None);
    };

    let token = payload.with_protocol(|protocol| match protocol {
        SocketProtocol::Tcp(TcpState::Listening { .. }) if interests.intersects(PollMask::IN) => {
            Some(socket_accept_wait_token(&witness.identity))
        }
        SocketProtocol::Sctp(TcpState::Listening { .. }) if interests.intersects(PollMask::IN) => {
            Some(socket_accept_wait_token(&witness.identity))
        }
        SocketProtocol::UnixStream(UnixStreamState::Listening { .. })
            if interests.intersects(PollMask::IN) =>
        {
            Some(socket_accept_wait_token(&witness.identity))
        }
        SocketProtocol::Tcp(TcpState::Connected { .. }) if interests.intersects(PollMask::IN) => {
            Some(socket_recv_wait_token(&witness.identity))
        }
        SocketProtocol::Sctp(TcpState::Connected { .. }) if interests.intersects(PollMask::IN) => {
            Some(socket_recv_wait_token(&witness.identity))
        }
        SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
            if interests.intersects(PollMask::IN) =>
        {
            Some(socket_recv_wait_token(&witness.identity))
        }
        SocketProtocol::Udp(UdpInner::Bound { .. } | UdpInner::Connected { .. })
        | SocketProtocol::UnixDatagram(
            UnixDatagramState::Bound { .. }
            | UnixDatagramState::Connected { .. }
            | UnixDatagramState::ConnectedPair { .. },
        )
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::Rds(RdsState::Bound { .. })
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_)
            if interests.intersects(PollMask::IN) =>
        {
            Some(socket_recv_wait_token(&witness.identity))
        }
        SocketProtocol::Tcp(TcpState::Connected { .. })
        | SocketProtocol::Sctp(TcpState::Connected { .. })
        | SocketProtocol::UnixStream(UnixStreamState::Connected { .. })
        | SocketProtocol::Udp(UdpInner::Bound { .. } | UdpInner::Connected { .. })
        | SocketProtocol::UnixDatagram(
            UnixDatagramState::Bound { .. }
            | UnixDatagramState::Connected { .. }
            | UnixDatagramState::ConnectedPair { .. },
        )
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::Rds(RdsState::Bound { .. })
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_)
            if interests.intersects(PollMask::OUT) =>
        {
            Some(socket_send_wait_token(&witness.identity))
        }
        _ => None,
    });
    StepOutcome::Done(token)
}
