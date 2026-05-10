use tx_substrate::zone::Cap;

use crate::execution::{Guard, StepOutcome};
use crate::net::checks::require::require_socket_poll_target;
use crate::net::structure::{
    AcceptWireSet, PollMask, RecvWireSet, SendWireSet, SocketIdentity, SocketProtocol, TcpState,
    UdpInner,
};

pub fn step_poll_ready(socket: &Cap<SocketIdentity>, guard: &Guard<'_>) -> StepOutcome<PollMask> {
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
        SocketProtocol::Tcp(TcpState::Listening { .. })
            if witness.identity.readiness.accept_wq.peek() & AcceptWireSet::HAS_PENDING.bits()
                != 0 =>
        {
            mask |= PollMask::IN;
        }
        SocketProtocol::Tcp(TcpState::Connected { .. }) => {
            if witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0 {
                mask |= PollMask::IN;
            }
            if witness.identity.readiness.recv_wq.peek() & RecvWireSet::BROKEN.bits() != 0
                || payload.tcp_recv_closed_by_peer()
            {
                mask |= PollMask::IN | PollMask::RDHUP;
            }
            if witness.identity.readiness.send_wq.peek() & SendWireSet::SPACE.bits() != 0 {
                mask |= PollMask::OUT;
            }
        }
        SocketProtocol::Udp(UdpInner::Bound { .. } | UdpInner::Connected { .. }) => {
            if witness.identity.readiness.recv_wq.peek() & RecvWireSet::HAS_DATA.bits() != 0 {
                mask |= PollMask::IN;
            }
            mask |= PollMask::OUT;
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
        _ => {}
    });

    StepOutcome::Done(mask)
}
