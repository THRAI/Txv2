use tx_substrate::zone::Cap;

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::execution::step_bind::table_error_to_errno;
use crate::net::namespace::initial_loopback_iface;
use crate::net::packet::{NetworkPublish, NetworkPublishTarget};
use crate::net::protocol::{LoopbackIface, PollContext};
use crate::net::structure::{
    ConnectionKey, IpEndpoint, Ipv4Address, SocketIdentity, SocketProtocol, TcpState,
};

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
    step_tcp_loopback_handshake_on_iface(client, initial_loopback_iface(), guard)
}

pub fn step_tcp_loopback_handshake_on_iface(
    client: &Cap<SocketIdentity>,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpConnectOutcome> {
    let Some(client_payload) = client.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let (local, remote) = match connecting_endpoints(&client_payload.protocol_snapshot()) {
        Ok(endpoints) => endpoints,
        Err(errno) => return StepOutcome::Err(errno),
    };
    if local.addr == Ipv4Address::UNSPECIFIED || local.port == 0 {
        return StepOutcome::Err(Errno::EADDRNOTAVAIL);
    }
    if remote.addr != Ipv4Address::LOOPBACK {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }

    let table = client_payload.socket_table();
    let Some(listener) = table.lookup_tcp_listener_addr(remote.addr, remote.port, guard) else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let Some(listener_payload) = listener.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let listener_matches_remote = matches!(
        listener_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Listening {
            local: listener_local,
            ..
        }) if listener_local == remote
            || (listener_local.addr == Ipv4Address::UNSPECIFIED
                && listener_local.port == remote.port)
    );
    if !listener_matches_remote {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    }

    let Some((child, handshake, mut publish_targets)) =
        establish_smoltcp_loopback_on_iface(client, local, remote, iface, guard)
    else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };

    let client_key = ConnectionKey::new(local, remote);
    if let Err(error) = table.insert_tcp_connection(client_key, client.clone()) {
        return StepOutcome::Err(table_error_to_errno(error));
    }

    client_payload.refresh_io_from_raw();
    if let Some(child_payload) = child.acquire_operational() {
        child_payload.refresh_io_from_raw();
    }
    publish_targets.push(NetworkPublishTarget::new(
        client.clone(),
        NetworkPublish {
            send_has_space: true,
            ..NetworkPublish::none()
        },
    ));

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
    step_process_loopback_tcp(source, max_bytes, initial_loopback_iface(), guard)
}

pub fn step_process_loopback_tcp(
    source: &Cap<SocketIdentity>,
    max_bytes: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpTransferOutcome> {
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

    let Some(peer_raw) = peer_payload.raw_tcp_socket() else {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    };

    let peer_space = peer_raw
        .recv_capacity()
        .saturating_sub(peer_raw.recv_available());
    let transfer_limit = core::cmp::min(max_bytes, peer_space);
    if transfer_limit == 0 {
        return StepOutcome::Done(LoopbackTcpTransferOutcome::default());
    }

    let mut ctx =
        PollContext::new_with_table(smoltcp::time::Instant::ZERO, source_payload.socket_table());
    let mut publish_targets = alloc::vec::Vec::new();
    let mut bytes_moved = 0;
    let mut peer_wake_fired = false;
    let mut egress_packets = 0;
    for _ in 0..4 {
        let Some(source_publish) = ctx.poll_egress_one(source, iface, guard) else {
            break;
        };
        egress_packets += 1;
        publish_targets.push(source_publish);

        let ingress = ctx.poll_ingress(iface, guard, 1);
        bytes_moved += ingress.bytes_moved;
        peer_wake_fired |= publishes_recv_data_for(&ingress.publishes, &peer);
        publish_targets.extend(ingress.publishes);

        if let Some(peer_publish) = ctx.poll_egress_one(&peer, iface, guard) {
            egress_packets += 1;
            publish_targets.push(peer_publish);
            let ack_ingress = ctx.poll_ingress(iface, guard, 1);
            publish_targets.extend(ack_ingress.publishes);
        }

        if bytes_moved >= transfer_limit {
            break;
        }
    }

    let source_recv_broken = publishes_recv_broken_for(&publish_targets, source);
    let source_send_broken = publishes_send_broken_for(&publish_targets, source);
    let peer_recv_broken = publishes_recv_broken_for(&publish_targets, &peer);
    let peer_send_broken = publishes_send_broken_for(&publish_targets, &peer);
    if bytes_moved == 0 && egress_packets == 0 && !publish_targets_have_work(&publish_targets) {
        return StepOutcome::Done(LoopbackTcpTransferOutcome::default());
    }

    let drain = source_payload.take_tcp_tx_bytes(bytes_moved);
    source_payload.refresh_io_from_raw();
    peer_payload.refresh_io_from_raw();
    let source_wake_fired = drain
        .as_ref()
        .map(|drain| drain.became_available)
        .unwrap_or(false);
    publish_targets.extend(send_space_publish_for_drain(source, source_wake_fired));
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
    became_available: bool,
) -> Option<NetworkPublishTarget> {
    if became_available {
        Some(NetworkPublishTarget::new(
            socket.clone(),
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
    local: IpEndpoint,
    remote: IpEndpoint,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> Option<(
    Cap<SocketIdentity>,
    LoopbackTcpHandshakeStats,
    alloc::vec::Vec<NetworkPublishTarget>,
)> {
    let client_payload = client.acquire_operational()?;
    let client_raw = client_payload.raw_tcp_socket()?;

    client_raw.connect_endpoint(local, remote).ok()?;

    let mut ctx =
        PollContext::new_with_table(smoltcp::time::Instant::ZERO, client_payload.socket_table());
    let mut publishes = alloc::vec::Vec::new();

    publishes.push(ctx.poll_egress_one(client, iface, guard)?);
    let syn_ingress = ctx.poll_ingress(iface, guard, 1);
    publishes.extend(syn_ingress.publishes);
    let child = syn_ingress.created_children.into_iter().next()?;
    let child_payload = child.acquire_operational()?;
    let child_raw = child_payload.raw_tcp_socket()?;

    drive_loopback_packet_to_socket(&mut ctx, &child, client, iface, guard, &mut publishes)?;

    publishes.push(ctx.poll_egress_one(client, iface, guard)?);
    let ack_ingress = ctx.poll_ingress(iface, guard, 1);
    publishes.extend(ack_ingress.publishes);

    if client_raw.protocol_state() != smoltcp::socket::tcp::State::Established {
        return None;
    }
    if child_raw.protocol_state() != smoltcp::socket::tcp::State::Established {
        return None;
    }

    let outcome = ctx.poll_ingress_to_socket(iface, &child, guard, 0);
    Some((
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

fn connecting_endpoints(protocol: &SocketProtocol) -> Result<(IpEndpoint, IpEndpoint), Errno> {
    match protocol {
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => Ok((*local, *remote)),
        SocketProtocol::Tcp(TcpState::Connected { .. }) => Err(Errno::EISCONN),
        SocketProtocol::Tcp(_) => Err(Errno::EINVAL),
        SocketProtocol::UnixDatagram
        | SocketProtocol::UnixStream
        | SocketProtocol::Udp(_)
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => Err(Errno::EOPNOTSUPP),
    }
}

fn connected_endpoints(protocol: &SocketProtocol) -> Result<(IpEndpoint, IpEndpoint), Errno> {
    match protocol {
        SocketProtocol::Tcp(TcpState::Connected { local, remote }) => Ok((*local, *remote)),
        SocketProtocol::Tcp(_) => Err(Errno::ENOTCONN),
        SocketProtocol::UnixDatagram
        | SocketProtocol::UnixStream
        | SocketProtocol::Udp(_)
        | SocketProtocol::RawIcmp(_)
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => Err(Errno::EOPNOTSUPP),
    }
}
