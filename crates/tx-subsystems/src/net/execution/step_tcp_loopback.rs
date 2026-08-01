use tx_substrate::index::IndexError;
use tx_substrate::step::NoProgress;
use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::execution::step_bind::table_error_to_errno;
use crate::net::namespace::initial_loopback_iface;
use crate::net::packet::NetworkPublishTarget;
use crate::net::protocol::{LoopbackIface, PollContext};
use crate::net::structure::{
    ConnectionKey, IpEndpoint, SocketIdentity, SocketProtocol, TcpConnectDisposition, TcpState,
};

use super::step_connect::fail_indexed_tcp_connect_attempt;

struct TcpHandshakeDriver<'a>(&'a crate::net::structure::SocketOperationalEvidence);

impl Drop for TcpHandshakeDriver<'_> {
    fn drop(&mut self) {
        self.0.release_tcp_handshake_driver();
    }
}

const TCP_LOOPBACK_TRANSFER_PACKET_PASSES: usize = 64;

#[derive(Clone)]
pub struct LoopbackTcpConnectOutcome {
    pub child: Cap<SocketIdentity>,
    pub wakes_fired: usize,
    pub handshake: LoopbackTcpHandshakeStats,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopbackTcpHandshakeStats {
    pub packets_seen: usize,
    pub tx_packets: usize,
    pub sockets_touched: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoopbackTcpTransferOutcome {
    pub bytes_moved: usize,
    pub packets_seen: usize,
    pub tx_packets: usize,
    pub sockets_touched: usize,
    pub source_recv_broken: bool,
    pub source_send_broken: bool,
    pub peer_recv_broken: bool,
    pub peer_send_broken: bool,
    pub source_wake_fired: bool,
    pub peer_wake_fired: bool,
}

pub fn step_tcp_loopback_handshake(
    client: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpConnectOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_tcp_loopback_handshake_on_iface(client, initial_loopback_iface(), guard)
}

pub fn step_tcp_loopback_handshake_on_iface(
    client: &Cap<SocketIdentity>,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpConnectOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Some(client_payload) = client.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let Some(attempt) = client_payload.active_tcp_connect_attempt() else {
        return StepOutcome::Err(Errno::EINVAL);
    };
    let local = attempt.local();
    let remote = attempt.remote();
    if !remote.is_loopback() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }
    let fail = |error| {
        let _ = fail_indexed_tcp_connect_attempt(client, &client_payload, attempt, error);
        StepOutcome::Err(error)
    };
    if local.is_unspecified() || local.port == 0 {
        return fail(Errno::EADDRNOTAVAIL);
    }

    // step_connect wakes the delegate and the blocking syscall also attempts
    // an inline loopback drive. On SMP those are two legitimate callers, but
    // smoltcp egress is consumptive: without per-flow ownership one CPU can
    // consume SYN-ACK/ACK and make the other report a false refusal.
    if !client_payload.try_claim_tcp_handshake_driver() {
        return StepOutcome::Continue {
            progress: NoProgress,
        };
    }
    let _driver = TcpHandshakeDriver(&client_payload);

    let table = client_payload.socket_table();
    let Some(listener) = table.lookup_tcp_listener_dual_stack_endpoint(remote, guard) else {
        return fail(Errno::ECONNREFUSED);
    };
    let Some(listener_payload) = listener.acquire_operational() else {
        return fail(Errno::ECONNREFUSED);
    };
    if !listener_accepts_incoming(&listener_payload, remote) {
        return fail(Errno::ECONNREFUSED);
    }

    let (child, handshake, publish_targets) =
        match establish_smoltcp_loopback_on_iface(client, attempt, iface, guard) {
            Ok(established) => established,
            Err(error) => {
                return fail(error);
            }
        };

    let wakes_fired = publish_targets
        .into_iter()
        .map(|target| target.publish())
        .sum();

    StepOutcome::Done(LoopbackTcpConnectOutcome {
        child,
        wakes_fired,
        handshake,
    })
}

pub fn step_tcp_loopback_transfer(
    source: &Cap<SocketIdentity>,
    max_bytes: usize,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpTransferOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_loopback_tcp(source, max_bytes, initial_loopback_iface(), guard)
}

pub fn step_process_loopback_tcp(
    source: &Cap<SocketIdentity>,
    max_bytes: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpTransferOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    if max_bytes == 0 {
        return StepOutcome::Done(LoopbackTcpTransferOutcome::default());
    }

    let Some(source_payload) = source.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let (local, remote) = match connected_endpoints(&source_payload.protocol_snapshot()) {
        Ok(endpoints) => endpoints,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let peer_key = ConnectionKey::new(remote, local);
    let Some(peer) = source_payload
        .socket_table()
        .lookup_tcp_connection(peer_key, guard)
    else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    if !matches!(
        peer_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: peer_local,
            remote: peer_remote,
        }) if peer_local == remote && peer_remote == local
    ) {
        return StepOutcome::Err(Errno::ENOTCONN);
    }

    // Flow control is smoltcp's job now: the peer's advertised window
    // derives from its rx ring, so dispatch stops by itself when full.
    let transfer_limit = max_bytes;
    let Some((source_generation, (source_had_no_send_space, source_recv_before))) = source_payload
        .process_current_tcp_flow(local, remote, |raw| {
            (raw.send_available() == 0, raw.recv_available())
        })
    else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let Some((peer_generation, peer_recv_before)) =
        peer_payload.process_current_tcp_flow(remote, local, |raw| raw.recv_available())
    else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };

    let mut ctx = PollContext::new_with_table(
        crate::net::clock::net_now_instant(),
        source_payload.socket_table(),
    );
    let mut publish_targets = alloc::vec::Vec::new();
    let mut bytes_moved = 0;
    let mut peer_wake_fired = false;
    let mut egress_packets = 0;
    while egress_packets < TCP_LOOPBACK_TRANSFER_PACKET_PASSES {
        let mut dispatched = false;
        if let Some(source_publish) =
            ctx.poll_tcp_egress_one_for_flow(source, source_generation, local, remote, iface, guard)
        {
            dispatched = true;
            egress_packets += 1;
            publish_targets.push(source_publish);

            let ingress = ctx.poll_ingress_to_socket(iface, &peer, guard, 1);
            bytes_moved += ingress.bytes_moved;
            peer_wake_fired |= publishes_recv_data_for(&ingress.publishes, &peer);
            publish_targets.extend(ingress.publishes);
        }
        // max_bytes is a packet-granular soft limit because smoltcp chooses
        // segment size. Check it before polling the reverse endpoint so one
        // iteration cannot add a second segment after the budget is reached.
        if bytes_moved >= transfer_limit || egress_packets >= TCP_LOOPBACK_TRANSFER_PACKET_PASSES {
            break;
        }
        if let Some(peer_publish) =
            ctx.poll_tcp_egress_one_for_flow(&peer, peer_generation, remote, local, iface, guard)
        {
            dispatched = true;
            egress_packets += 1;
            publish_targets.push(peer_publish);
            let peer_ingress = ctx.poll_ingress_to_socket(iface, source, guard, 1);
            bytes_moved += peer_ingress.bytes_moved;
            publish_targets.extend(peer_ingress.publishes);
        }
        if !dispatched || bytes_moved >= transfer_limit {
            break;
        }
    }

    // smoltcp may promote bytes already held by its assembler into the recv
    // ring while dispatching a window update. Such progress is not attributable
    // to the payload length of the packet currently being ingressed, so retain
    // the stronger whole-step ring delta as a floor for transfer accounting.
    let source_recv_after = source_payload
        .with_tcp_flow_generation(source_generation, local, remote, |raw| raw.recv_available())
        .unwrap_or(source_recv_before);
    let peer_recv_after = peer_payload
        .with_tcp_flow_generation(peer_generation, remote, local, |raw| raw.recv_available())
        .unwrap_or(peer_recv_before);
    let buffered_progress = source_recv_after
        .saturating_sub(source_recv_before)
        .saturating_add(peer_recv_after.saturating_sub(peer_recv_before));
    bytes_moved = bytes_moved.max(buffered_progress);

    let source_recv_broken = publishes_recv_broken_for(&publish_targets, source);
    let source_send_broken = publishes_send_broken_for(&publish_targets, source);
    let peer_recv_broken = publishes_recv_broken_for(&publish_targets, &peer);
    let peer_send_broken = publishes_send_broken_for(&publish_targets, &peer);
    if bytes_moved == 0 && egress_packets == 0 && !publish_targets_have_work(&publish_targets) {
        return StepOutcome::Done(LoopbackTcpTransferOutcome::default());
    }

    // Send space opens when the transfer's ACKs release smoltcp tx ring
    // bytes — derive the wake from the ring, no shadow drain to account.
    let source_wake_fired = source_had_no_send_space
        && source_payload
            .with_tcp_flow_generation(source_generation, local, remote, |raw| {
                raw.send_available() > 0
            })
            .unwrap_or(false);
    publish_targets.extend(send_space_publish_for_drain(
        source,
        source_generation,
        source_wake_fired,
    ));
    for target in publish_targets {
        target.publish();
    }
    let stats = ctx.poll_ingress_to_socket(iface, source, guard, 0);

    StepOutcome::Done(LoopbackTcpTransferOutcome {
        bytes_moved,
        packets_seen: stats.packets_seen,
        tx_packets: stats.tx_packets.max(egress_packets),
        sockets_touched: stats.sockets_touched,
        source_recv_broken,
        source_send_broken,
        peer_recv_broken,
        peer_send_broken,
        source_wake_fired,
        peer_wake_fired,
    })
}

fn publishes_recv_data_for(
    publishes: &[NetworkPublishTarget],
    socket: &Cap<SocketIdentity>,
) -> bool {
    publishes
        .iter()
        .any(|target| target.socket.raw() == socket.raw() && target.publish.recv_has_data)
}

fn publishes_recv_broken_for(
    publishes: &[NetworkPublishTarget],
    socket: &Cap<SocketIdentity>,
) -> bool {
    publishes
        .iter()
        .any(|target| target.socket.raw() == socket.raw() && target.publish.recv_broken)
}

fn publishes_send_broken_for(
    publishes: &[NetworkPublishTarget],
    socket: &Cap<SocketIdentity>,
) -> bool {
    publishes
        .iter()
        .any(|target| target.socket.raw() == socket.raw() && target.publish.send_broken)
}

fn publish_targets_have_work(publishes: &[NetworkPublishTarget]) -> bool {
    publishes.iter().any(|target| target.publish.has_any())
}

fn send_space_publish_for_drain(
    socket: &Cap<SocketIdentity>,
    generation: crate::net::structure::TcpStateGeneration,
    became_available: bool,
) -> Option<NetworkPublishTarget> {
    if became_available {
        Some(NetworkPublishTarget::new_tcp(
            socket.clone(),
            generation,
            crate::net::packet::NetworkPublish {
                send_has_space: true,
                ..crate::net::packet::NetworkPublish::none()
            },
        ))
    } else {
        None
    }
}

fn establish_smoltcp_loopback_on_iface(
    client: &Cap<SocketIdentity>,
    attempt: crate::net::structure::TcpConnectAttempt,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> Result<
    (
        Cap<SocketIdentity>,
        LoopbackTcpHandshakeStats,
        alloc::vec::Vec<NetworkPublishTarget>,
    ),
    Errno,
> {
    let client_payload = client.acquire_operational().ok_or(Errno::ENOTCONN)?;
    let client_raw = client_payload.raw_tcp_socket().ok_or(Errno::EOPNOTSUPP)?;
    let table = client_payload.socket_table();
    client_payload
        .transact_tcp_connect_attempt(Some(attempt), attempt.generation(), |local, remote| {
            let key = ConnectionKey::new(local, remote);
            let indexed = match table.insert_tcp_connection(key, client.clone()) {
                Ok(()) => Ok(()),
                Err(IndexError::Duplicate)
                    if table
                        .lookup_tcp_connection(key, guard)
                        .is_some_and(|owner| owner.raw() == client.raw()) =>
                {
                    Ok(())
                }
                Err(error) => Err(table_error_to_errno(error)),
            };
            let result = indexed.and_then(|()| {
                client_raw
                    .connect_endpoint_for_attempt(attempt)
                    .map_err(|_| Errno::EINVAL)
            });
            (result, TcpConnectDisposition::KeepConnecting)
        })
        .ok_or(Errno::ECANCELED)??;

    let mut ctx = PollContext::new_with_table(
        crate::net::clock::net_now_instant(),
        client_payload.socket_table(),
    );
    let mut publishes = alloc::vec::Vec::new();
    let generation = attempt.generation();
    let local = attempt.local();
    let remote = attempt.remote();

    let Some(client_syn) =
        ctx.poll_tcp_egress_one_for_flow(client, generation, local, remote, iface, guard)
    else {
        return Err(Errno::ECONNREFUSED);
    };
    publishes.push(client_syn);
    let syn_ingress = ctx.poll_tcp_ingress_for_flow(iface, local, remote, guard, 1);
    publishes.extend(syn_ingress.publishes);
    let Some(child) = syn_ingress.created_children.into_iter().next() else {
        return Err(Errno::ECONNREFUSED);
    };
    let Some(child_payload) = child.acquire_operational() else {
        return Err(Errno::ECONNREFUSED);
    };
    let Some(child_raw) = child_payload.raw_tcp_socket() else {
        return Err(Errno::ECONNREFUSED);
    };

    if drive_loopback_packet_to_socket(&mut ctx, &child, client, iface, guard, &mut publishes)
        .is_none()
    {
        return Err(Errno::ECONNREFUSED);
    }

    let Some(client_ack) =
        ctx.poll_tcp_egress_one_for_flow(client, generation, local, remote, iface, guard)
    else {
        return Err(Errno::ECONNREFUSED);
    };
    publishes.push(client_ack);
    let ack_ingress = ctx.poll_ingress_to_socket(iface, &child, guard, 1);
    publishes.extend(ack_ingress.publishes);

    if client_payload
        .with_tcp_flow_generation(generation, local, remote, |raw| raw.protocol_state())
        != Some(smoltcp::socket::tcp::State::Established)
    {
        return Err(Errno::ECONNREFUSED);
    }
    if child_raw.protocol_state() != smoltcp::socket::tcp::State::Established {
        return Err(Errno::ECONNREFUSED);
    }

    let outcome = ctx.poll_ingress_to_socket(iface, &child, guard, 0);
    Ok((
        child,
        LoopbackTcpHandshakeStats {
            packets_seen: outcome.packets_seen,
            tx_packets: outcome.tx_packets,
            sockets_touched: outcome.sockets_touched,
        },
        publishes,
    ))
}

fn drive_loopback_packet_to_socket(
    ctx: &mut PollContext,
    source: &Cap<SocketIdentity>,
    target: &Cap<SocketIdentity>,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
    publishes: &mut alloc::vec::Vec<NetworkPublishTarget>,
) -> Option<()> {
    publishes.push(ctx.poll_egress_one(source, iface, guard)?);
    let outcome = ctx.poll_ingress_to_socket(iface, target, guard, 1);
    publishes.extend(outcome.publishes);
    Some(())
}

fn connected_endpoints(protocol: &SocketProtocol) -> Result<(IpEndpoint, IpEndpoint), Errno> {
    match protocol {
        SocketProtocol::Tcp(TcpState::Connected { local, remote }) => Ok((*local, *remote)),
        SocketProtocol::Tcp(_) => Err(Errno::ENOTCONN),
        SocketProtocol::UnixDatagram(_)
        | SocketProtocol::UnixStream(_)
        | SocketProtocol::Udp(_)
        | SocketProtocol::Sctp(_)
        | SocketProtocol::Rds(_)
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => Err(Errno::EOPNOTSUPP),
    }
}

fn listener_accepts_incoming(
    listener_payload: &crate::net::structure::SocketOperationalEvidence,
    dst: IpEndpoint,
) -> bool {
    let protocol = listener_payload.protocol_snapshot();
    let v6only = listener_payload.with_options(|options| options.ip.ipv6_v6only);
    matches!(
        protocol,
        SocketProtocol::Tcp(TcpState::Listening {
            local: listener_local,
            ..
        }) if listener_local == dst
            || (listener_local.same_family(dst)
                && listener_local.is_unspecified()
                && listener_local.port == dst.port)
            || (!v6only
                && listener_local.family == crate::net::structure::AddressFamily::Inet6
                && listener_local.is_unspecified()
                && dst.family == crate::net::structure::AddressFamily::Inet
                && dst.is_loopback()
                && listener_local.port == dst.port)
    )
}
