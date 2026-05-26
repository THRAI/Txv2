use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::checks::require::require_socket_write_target;
use crate::net::namespace::initial_loopback_iface;
use crate::net::protocol::{LoopbackIface, PollContext, UDP_IPV4_MAX_PAYLOAD_BYTES};
use crate::net::structure::{
    IpEndpoint, SendRecvFlags, SendWireSet, SocketIdentity, SocketProtocol, UdpInner,
};

use super::step_send::send_flags_error;
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_loopback_udp_on_iface(source, budget, initial_loopback_iface(), guard)
}

pub fn step_process_loopback_udp_on_iface(
    source: &Cap<SocketIdentity>,
    budget: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackUdpTransferOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let witness = match require_socket_write_target(socket, flags, guard) {
        Ok(witness) => witness,
        Err(errno) => return tx_substrate::step::StepOutcome::Err(errno),
    };
    debug_assert_eq!(witness.identity.raw(), socket.raw());

    let Some(source_payload) = socket.acquire_operational() else {
        return tx_substrate::step::StepOutcome::Err(Errno::ENOTCONN);
    };
    if let Some(errno) = send_flags_error(witness.flags) {
        return tx_substrate::step::StepOutcome::Err(errno);
    }
    if source_payload.shutdown_wr() {
        return tx_substrate::step::StepOutcome::Err(Errno::EPIPE);
    }
    let total_payload_len = source_payload.udp_corked_send_len() + bytes.len();
    if total_payload_len > UDP_IPV4_MAX_PAYLOAD_BYTES {
        return tx_substrate::step::StepOutcome::Err(Errno::EMSGSIZE);
    }
    if bytes.is_empty() {
        return tx_substrate::step::StepOutcome::Done(0);
    }

    let (local, destination) =
        match udp_loopback_endpoints(&source_payload.protocol_snapshot(), dst) {
            Some(endpoints) => endpoints,
            None => return tx_substrate::step::StepOutcome::Err(Errno::EDESTADDRREQ),
        };
    if !is_loopback_destination(destination)
        || !(local.is_unspecified() || local.same_family(destination) && local.is_loopback())
    {
        return tx_substrate::step::StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    if total_payload_len + udp_packet_overhead(destination) > usize::from(iface.mtu()) {
        return tx_substrate::step::StepOutcome::Err(Errno::EMSGSIZE);
    }
    let reserve =
        match source_payload.reserve_send_bytes_to_with_flags(Some(destination), bytes, flags) {
            Ok(Some(reserve)) => reserve,
            Ok(None) => {
                socket.readiness.clear_send(SendWireSet::SPACE);
                return yield_bytes_on_token(
                    tx_substrate::step::ByteProgress::EMPTY,
                    socket_send_wait_token(socket),
                );
            }
            Err(errno) => return tx_substrate::step::StepOutcome::Err(errno),
        };
    if reserve.became_full {
        socket.readiness.clear_send(SendWireSet::SPACE);
    }
    if flags.contains(SendRecvFlags::MSG_MORE) {
        return tx_substrate::step::StepOutcome::Done(reserve.bytes);
    }

    let source = loopback_udp_source(local, destination, iface);
    if source.port == 0 || destination.port == 0 {
        return tx_substrate::step::StepOutcome::Err(Errno::EINVAL);
    }

    let Some(drain) = source_payload.commit_udp_tx_datagram_sent() else {
        return tx_substrate::step::StepOutcome::Done(reserve.bytes);
    };
    let Some(target) =
        source_payload
            .socket_table()
            .lookup_udp_ingress(source, drain.datagram.dst, guard)
    else {
        return tx_substrate::step::StepOutcome::Done(reserve.bytes);
    };
    let Some(target_payload) = target.acquire_operational() else {
        return tx_substrate::step::StepOutcome::Done(reserve.bytes);
    };

    if target_payload.record_recv_payload(source, drain.datagram.dst, drain.datagram.payload) {
        target
            .readiness
            .fire_recv(crate::net::structure::RecvWireSet::HAS_DATA);
    }
    if drain.became_available {
        socket.readiness.fire_send(SendWireSet::SPACE);
    }
    tx_substrate::step::StepOutcome::Done(reserve.bytes)
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
    let addr = if local.is_unspecified() && is_loopback_destination(destination) {
        IpEndpoint::loopback_for_family(destination.family, local.port).ip_addr()
    } else {
        local.ip_addr()
    };
    let _ = iface;
    IpEndpoint::from_ip(addr, local.port)
}

fn is_loopback_destination(endpoint: IpEndpoint) -> bool {
    endpoint.is_loopback()
}

fn udp_packet_overhead(endpoint: IpEndpoint) -> usize {
    const IPV4_UDP_OVERHEAD: usize = 20 + 8;
    const IPV6_UDP_OVERHEAD: usize = 40 + 8;
    if endpoint.family == crate::net::structure::AddressFamily::Inet6 {
        IPV6_UDP_OVERHEAD
    } else {
        IPV4_UDP_OVERHEAD
    }
}
