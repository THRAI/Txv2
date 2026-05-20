use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_write_target;
use crate::net::namespace::initial_loopback_iface;
use crate::net::protocol::{LoopbackIface, PollContext};
use crate::net::structure::{
    IpEndpoint, Ipv4Address, SendRecvFlags, SendWireSet, SocketIdentity, SocketProtocol, UdpInner,
};

use super::{socket_send_wait_token, yield_bytes_on_token, ByteStepOutcome};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopbackUdpTransferOutcome {
    pub tx_packets: usize,
    pub packets_seen: usize,
    pub sockets_touched: usize,
    pub bytes_moved: usize,
    pub source_wake_fired: bool,
    pub peer_wake_fired: bool,
}

pub fn step_process_loopback_udp(
    source: &Cap<SocketIdentity>,
    budget: usize,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackUdpTransferOutcome> {
    step_process_loopback_udp_on_iface(source, budget, initial_loopback_iface(), guard)
}

pub fn step_process_loopback_udp_on_iface(
    source: &Cap<SocketIdentity>,
    budget: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackUdpTransferOutcome> {
    let Some(source_payload) = source.acquire_operational() else {
        return StepOutcome::Done(LoopbackUdpTransferOutcome::default());
    };

    if budget > 0 {
        let mut ctx = PollContext::new_with_table(
            smoltcp::time::Instant::ZERO,
            source_payload.socket_table(),
        );
        if let Some(outcome) = ctx.poll_udp_loopback_direct_one(source, iface, guard) {
            let mut source_wake_fired = false;
            let mut peer_wake_fired = false;
            for publish in outcome.publishes {
                source_wake_fired |= publish.publish.send_has_space;
                peer_wake_fired |= publish.publish.recv_has_data;
                publish.publish();
            }
            return StepOutcome::Done(LoopbackUdpTransferOutcome {
                tx_packets: outcome.tx_packets,
                packets_seen: outcome.packets_seen,
                sockets_touched: outcome.sockets_touched,
                bytes_moved: outcome.bytes_moved,
                source_wake_fired,
                peer_wake_fired,
            });
        }
    }
    let mut ctx =
        PollContext::new_with_table(smoltcp::time::Instant::ZERO, source_payload.socket_table());
    let mut source_wake_fired = false;

    if let Some(publish) = ctx.poll_udp_egress_one(source, iface, guard) {
        source_wake_fired = publish.publish.send_has_space;
        publish.publish();
    }

    let ingress = ctx.poll_udp_ingress(iface, guard, budget);
    let mut peer_wake_fired = false;
    for publish in ingress.publishes {
        peer_wake_fired |= publish.publish.recv_has_data;
        publish.publish();
    }

    StepOutcome::Done(LoopbackUdpTransferOutcome {
        tx_packets: ingress.tx_packets,
        packets_seen: ingress.packets_seen,
        sockets_touched: ingress.sockets_touched,
        bytes_moved: ingress.bytes_moved,
        source_wake_fired,
        peer_wake_fired,
    })
}

pub fn step_send_udp_loopback_kernel_bytes(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    step_send_udp_loopback_kernel_bytes_on_iface(
        socket,
        dst,
        bytes,
        flags,
        initial_loopback_iface(),
        guard,
    )
}

pub fn step_send_udp_loopback_kernel_bytes_on_iface(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
    bytes: &[u8],
    flags: SendRecvFlags,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> ByteStepOutcome<usize> {
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return tx_substrate::step::StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(source_payload) = socket.acquire_operational() else {
        return tx_substrate::step::StepOutcome::Err(Errno::ENOTCONN);
    };
    if source_payload.shutdown_wr() {
        return tx_substrate::step::StepOutcome::Err(Errno::EPIPE);
    }
    if bytes.is_empty() {
        return tx_substrate::step::StepOutcome::Done(0);
    }

    let (local, destination) =
        match udp_loopback_endpoints(&source_payload.protocol_snapshot(), dst) {
            Some(endpoints) => endpoints,
            None => return tx_substrate::step::StepOutcome::Err(Errno::EDESTADDRREQ),
        };
    if destination.addr != iface.local_ipv4()
        || !(local.addr == Ipv4Address::UNSPECIFIED || local.addr == iface.local_ipv4())
    {
        return tx_substrate::step::StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    if bytes.len()
        > source_payload
            .raw_udp_socket()
            .map(|raw_udp| raw_udp.send_available())
            .unwrap_or(0)
    {
        socket.readiness.clear_send(SendWireSet::SPACE);
        return yield_bytes_on_token(
            tx_substrate::step::ByteProgress::EMPTY,
            socket_send_wait_token(socket),
        );
    }

    let source = loopback_udp_source(local, destination, iface);
    if source.port == 0 || destination.port == 0 || bytes.len() + 28 > usize::from(iface.mtu()) {
        return tx_substrate::step::StepOutcome::Err(Errno::EINVAL);
    }

    let Some(target) = source_payload
        .socket_table()
        .lookup_udp_ingress(source, destination, guard)
    else {
        return tx_substrate::step::StepOutcome::Done(bytes.len());
    };
    let Some(target_payload) = target.acquire_operational() else {
        return tx_substrate::step::StepOutcome::Done(bytes.len());
    };

    if target_payload.record_recv_payload(source, destination, bytes.to_vec()) {
        target
            .readiness
            .fire_recv(crate::net::structure::RecvWireSet::HAS_DATA);
    }
    tx_substrate::step::StepOutcome::Done(bytes.len())
}

fn udp_loopback_endpoints(
    protocol: &SocketProtocol,
    dst: Option<IpEndpoint>,
) -> Option<(IpEndpoint, IpEndpoint)> {
    match protocol {
        SocketProtocol::Udp(UdpInner::Bound { local }) => dst.map(|dst| (*local, dst)),
        SocketProtocol::Udp(UdpInner::Connected { local, remote }) => {
            Some((*local, dst.unwrap_or(*remote)))
        }
        _ => None,
    }
}

fn loopback_udp_source(
    local: IpEndpoint,
    destination: IpEndpoint,
    iface: &LoopbackIface,
) -> IpEndpoint {
    let addr = if local.addr == Ipv4Address::UNSPECIFIED && destination.addr == iface.local_ipv4() {
        iface.local_ipv4()
    } else {
        local.addr
    };
    IpEndpoint::new(addr, local.port)
}
