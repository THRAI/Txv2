use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome, WaitToken};
use crate::net::checks::require::require_socket_connect_target;
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::execution::yield_on_token;
use crate::net::structure::{
    IpEndpoint, Ipv4Address, KernelSockAddr, SendWireSet, SocketIdentity, SocketProtocol, TcpState,
    UdpInner,
};

pub fn step_connect(
    socket: &Cap<SocketIdentity>,
    remote: KernelSockAddr,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let witness = match require_socket_connect_target(socket, remote, guard) {
        Ok(witness) => witness,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let mut advanced = false;
    let blocked = payload.with_protocol_mut(|protocol| match protocol {
        SocketProtocol::Tcp(TcpState::Init) => {
            *protocol = SocketProtocol::Tcp(TcpState::Connecting {
                local: unspecified_endpoint(),
                remote: witness.remote,
            });
            advanced = true;
            true
        }
        SocketProtocol::Tcp(TcpState::Bound { local }) => {
            *protocol = SocketProtocol::Tcp(TcpState::Connecting {
                local: *local,
                remote: witness.remote,
            });
            advanced = true;
            true
        }
        SocketProtocol::Tcp(TcpState::Connecting { .. }) => true,
        SocketProtocol::Udp(UdpInner::Unbound) => {
            *protocol = SocketProtocol::Udp(UdpInner::Connected {
                local: unspecified_endpoint(),
                remote: witness.remote,
            });
            false
        }
        SocketProtocol::Udp(UdpInner::Bound { local }) => {
            *protocol = SocketProtocol::Udp(UdpInner::Connected {
                local: *local,
                remote: witness.remote,
            });
            false
        }
        SocketProtocol::Udp(UdpInner::Connected { local, .. }) => {
            *protocol = SocketProtocol::Udp(UdpInner::Connected {
                local: *local,
                remote: witness.remote,
            });
            false
        }
        SocketProtocol::RawIcmp(_) => false,
        _ => false,
    });

    if blocked {
        if advanced {
            net_delegate_kick_poll();
        }
        let wait = WaitToken::new(
            socket.wait_carriers.send,
            SendWireSet::SPACE.bits() | SendWireSet::BROKEN.bits(),
        );
        yield_on_token(wait)
    } else {
        net_delegate_kick_poll();
        StepOutcome::Done(())
    }
}

const fn unspecified_endpoint() -> IpEndpoint {
    IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0)
}
