use tx_substrate::wake::mailbox::{MailboxEvent, TaskMailbox};
use tx_substrate::zone::Cap;
use tx_substrate::{index::IndexError, step::NoProgress};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::namespace::initial_loopback_iface;
use crate::net::packet::{NetworkPublish, NetworkPublishTarget};
use crate::net::protocol::{
    promote_connected_stream_and_publish_accept, LoopbackIface, PollContext,
};
use crate::net::structure::table::SocketTable;
use crate::net::structure::{ConnectionKey, IpEndpoint, SocketIdentity, SocketProtocol, TcpState};

const TCP_LOOPBACK_TRANSFER_PACKET_PASSES: usize = 64;
const TCP_LOOPBACK_HANDSHAKE_PACKET_PASSES: usize = 8;

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
    /// Payload bytes admitted in either direction while driving the pair.
    pub bytes_moved: usize,
    /// Subset of `bytes_moved` delivered from the requested source to its peer.
    pub source_bytes_moved: usize,
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

pub fn step_tcp_loopback_handshake_with_post<F>(
    client: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
    post: F,
) -> StepOutcome<LoopbackTcpConnectOutcome>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_tcp_loopback_handshake_on_iface_with_post::<F>(
        client,
        initial_loopback_iface(),
        guard,
        post,
    )
}

pub fn step_tcp_loopback_handshake(
    client: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpConnectOutcome> {
    step_tcp_loopback_handshake_with_post(client, guard, |mailbox, event| mailbox.post(event))
}

pub fn step_tcp_loopback_handshake_on_iface_with_post<F>(
    client: &Cap<SocketIdentity>,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
    mut post: F,
) -> StepOutcome<LoopbackTcpConnectOutcome>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Some(client_payload) = client.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let (local, remote) = match connecting_endpoints(&client_payload.protocol_snapshot()) {
        Ok(endpoints) => endpoints,
        Err(errno) => return StepOutcome::Err(errno),
    };
    if local.is_unspecified() || local.port == 0 {
        return StepOutcome::Err(Errno::EADDRNOTAVAIL);
    }
    if !remote.is_loopback() {
        return StepOutcome::Err(Errno::EOPNOTSUPP);
    }

    let table = client_payload.socket_table();
    let Some(listener) = table.lookup_tcp_listener_dual_stack_endpoint(remote, guard) else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let Some(listener_payload) = listener.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    if !listener_accepts_incoming(&listener_payload, remote) {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    }

    let client_key = ConnectionKey::new(local, remote);
    let inserted_client = match ensure_connection_entry(table, client_key, client, guard) {
        Ok(inserted) => inserted,
        Err(errno) => return StepOutcome::Err(errno),
    };

    let drive = establish_smoltcp_loopback_on_iface(client, local, remote, iface, guard);
    let (child, handshake, mut publish_targets) = match drive {
        LoopbackHandshakeDrive::Complete {
            child,
            stats,
            publishes,
        } => (child, stats, publishes),
        LoopbackHandshakeDrive::Pending { publishes } => {
            for target in publishes {
                target.publish();
            }
            // Keep the reserved client tuple and protocol state: the next
            // delegate pass resumes from SynSent/SynReceived instead of
            // restarting connect() and converting InvalidState to refusal.
            net_delegate_kick_poll();
            return StepOutcome::Continue {
                progress: NoProgress,
            };
        }
        LoopbackHandshakeDrive::Failed => {
            if inserted_client {
                let _ = table.withdraw_tcp_connection(client_key);
            }
            return StepOutcome::Err(Errno::ECONNREFUSED);
        }
    };

    let Some(child_payload) = child.acquire_operational() else {
        if inserted_client {
            let _ = table.withdraw_tcp_connection(client_key);
        }
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    if let Some(accept_publish) =
        promote_connected_stream_and_publish_accept(table, &child, &child_payload, guard)
    {
        publish_targets.push(accept_publish);
    }
    // The client tuple was installed before driving packets, so this commits
    // only its public protocol state and is idempotent.
    let Some(client_payload) = client.acquire_operational() else {
        return StepOutcome::Err(Errno::ECONNREFUSED);
    };
    let _ = promote_connected_stream_and_publish_accept(table, client, &client_payload, guard);
    if !matches!(
        client_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: current_local,
            remote: current_remote,
        }) if current_local == local && current_remote == remote
    ) || !matches!(
        child_payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected {
            local: current_local,
            remote: current_remote,
        }) if current_local == remote && current_remote == local
    ) {
        net_delegate_kick_poll();
        return StepOutcome::Continue {
            progress: NoProgress,
        };
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
        .map(|target| target.publish_with_post(&mut post))
        .sum();

    StepOutcome::Done(LoopbackTcpConnectOutcome {
        child,
        wakes_fired,
        handshake,
    })
}

pub fn step_tcp_loopback_handshake_on_iface(
    client: &Cap<SocketIdentity>,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpConnectOutcome> {
    step_tcp_loopback_handshake_on_iface_with_post(client, iface, guard, |mailbox, event| {
        mailbox.post(event)
    })
}

pub fn step_tcp_loopback_transfer_with_post<F>(
    source: &Cap<SocketIdentity>,
    max_bytes: usize,
    guard: &Guard<'_>,
    post: F,
) -> StepOutcome<LoopbackTcpTransferOutcome>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    step_process_loopback_tcp_with_post::<F>(
        source,
        max_bytes,
        initial_loopback_iface(),
        guard,
        post,
    )
}

pub fn step_tcp_loopback_transfer(
    source: &Cap<SocketIdentity>,
    max_bytes: usize,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpTransferOutcome> {
    step_tcp_loopback_transfer_with_post(source, max_bytes, guard, |mailbox, event| {
        mailbox.post(event)
    })
}

pub fn step_process_loopback_tcp_with_post<F>(
    source: &Cap<SocketIdentity>,
    max_bytes: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
    mut post: F,
) -> StepOutcome<LoopbackTcpTransferOutcome>
where
    F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
{
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
    let mut source_bytes_moved = 0;
    let mut peer_wake_fired = false;
    let mut egress_packets = 0;
    let mut packets_seen = 0;
    for _ in 0..TCP_LOOPBACK_TRANSFER_PACKET_PASSES {
        let mut cycle_progress = false;

        // A delegate running on another CPU may already have dispatched this
        // flow's segment into the shared loopback queue. Drain that exact flow
        // before asking smoltcp for more egress. Generic queue-head polling is
        // incorrect here: under concurrent flows it can consume an unrelated
        // connection's packet and falsely account that progress to `source`.
        let ingress = ctx.poll_ingress_to_socket(iface, &peer, guard, 1);
        cycle_progress |= ingress.packets_seen > packets_seen;
        packets_seen = ingress.packets_seen;
        bytes_moved += ingress.bytes_moved;
        source_bytes_moved += ingress.bytes_moved;
        peer_wake_fired |= publishes_recv_data_for(&ingress.publishes, &peer);
        publish_targets.extend(ingress.publishes);

        if source_bytes_moved >= transfer_limit {
            break;
        }

        if let Some(source_publish) = ctx.poll_egress_one(source, iface, guard) {
            cycle_progress = true;
            egress_packets += 1;
            publish_targets.push(source_publish);

            let ingress = ctx.poll_ingress_to_socket(iface, &peer, guard, 1);
            cycle_progress |= ingress.packets_seen > packets_seen;
            packets_seen = ingress.packets_seen;
            bytes_moved += ingress.bytes_moved;
            source_bytes_moved += ingress.bytes_moved;
            peer_wake_fired |= publishes_recv_data_for(&ingress.publishes, &peer);
            publish_targets.extend(ingress.publishes);
        }

        if let Some(peer_publish) = ctx.poll_egress_one(&peer, iface, guard) {
            cycle_progress = true;
            egress_packets += 1;
            publish_targets.push(peer_publish);
            let ack_ingress = ctx.poll_ingress_to_socket(iface, source, guard, 1);
            cycle_progress |= ack_ingress.packets_seen > packets_seen;
            packets_seen = ack_ingress.packets_seen;
            bytes_moved += ack_ingress.bytes_moved;
            publish_targets.extend(ack_ingress.publishes);
        }

        if source_bytes_moved >= transfer_limit {
            break;
        }
        if !cycle_progress {
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
        target.publish_with_post(&mut post);
    }
    StepOutcome::Done(LoopbackTcpTransferOutcome {
        bytes_moved,
        source_bytes_moved,
        packets_seen,
        tx_packets: egress_packets,
        sockets_touched: ctx.sockets_touched(),
        source_recv_broken,
        source_send_broken,
        peer_recv_broken,
        peer_send_broken,
        source_wake_fired,
        peer_wake_fired,
    })
}

pub fn step_process_loopback_tcp(
    source: &Cap<SocketIdentity>,
    max_bytes: usize,
    iface: &LoopbackIface,
    guard: &Guard<'_>,
) -> StepOutcome<LoopbackTcpTransferOutcome> {
    step_process_loopback_tcp_with_post(source, max_bytes, iface, guard, |mailbox, event| {
        mailbox.post(event)
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
) -> LoopbackHandshakeDrive {
    let Some(client_payload) = client.acquire_operational() else {
        return LoopbackHandshakeDrive::Failed;
    };
    let Some(client_raw) = client_payload.raw_tcp_socket() else {
        return LoopbackHandshakeDrive::Failed;
    };

    match client_raw.protocol_state() {
        smoltcp::socket::tcp::State::Closed => {
            if client_raw.connect_endpoint(local, remote).is_err() {
                return LoopbackHandshakeDrive::Failed;
            }
        }
        smoltcp::socket::tcp::State::SynSent
        | smoltcp::socket::tcp::State::SynReceived
        | smoltcp::socket::tcp::State::Established => {}
        _ => return LoopbackHandshakeDrive::Failed,
    }

    let mut ctx =
        PollContext::new_with_table(smoltcp::time::Instant::ZERO, client_payload.socket_table());
    let mut publishes = alloc::vec::Vec::new();
    let table = client_payload.socket_table();
    let listener = table.lookup_tcp_listener_dual_stack_endpoint(remote, guard);
    let mut child = listener
        .as_ref()
        .and_then(|listener| listener.acquire_operational())
        .and_then(|payload| payload.connecting_child(remote, local));

    for _ in 0..TCP_LOOPBACK_HANDSHAKE_PACKET_PASSES {
        let mut progressed = false;

        if let Some(publish) = ctx.poll_egress_one(client, iface, guard) {
            publishes.push(publish);
            progressed = true;
        }
        let before_packets = ctx_packets_seen(&ctx);
        let syn_ingress = ctx.poll_tcp_ingress_for_flow(iface, local, remote, guard, 1);
        progressed |= syn_ingress.packets_seen > before_packets;
        publishes.extend(syn_ingress.publishes);
        if child.is_none() {
            child = syn_ingress.created_children.into_iter().next().or_else(|| {
                listener
                    .as_ref()
                    .and_then(|listener| listener.acquire_operational())
                    .and_then(|payload| payload.connecting_child(remote, local))
            });
        }

        let Some(server_child) = child.as_ref() else {
            if !progressed {
                break;
            }
            continue;
        };
        let Some(server_payload) = server_child.acquire_operational() else {
            return LoopbackHandshakeDrive::Failed;
        };
        let Some(server_raw) = server_payload.raw_tcp_socket() else {
            return LoopbackHandshakeDrive::Failed;
        };

        if let Some(publish) = ctx.poll_egress_one(server_child, iface, guard) {
            publishes.push(publish);
            progressed = true;
        }
        let before_packets = ctx_packets_seen(&ctx);
        let client_ingress = ctx.poll_ingress_to_socket(iface, client, guard, 1);
        progressed |= client_ingress.packets_seen > before_packets;
        publishes.extend(client_ingress.publishes);

        if let Some(publish) = ctx.poll_egress_one(client, iface, guard) {
            publishes.push(publish);
            progressed = true;
        }
        let before_packets = ctx_packets_seen(&ctx);
        let child_ingress = ctx.poll_ingress_to_socket(iface, server_child, guard, 1);
        progressed |= child_ingress.packets_seen > before_packets;
        publishes.extend(child_ingress.publishes);

        if client_raw.protocol_state() == smoltcp::socket::tcp::State::Established
            && server_raw.protocol_state() == smoltcp::socket::tcp::State::Established
        {
            return LoopbackHandshakeDrive::Complete {
                child: server_child.clone(),
                stats: LoopbackTcpHandshakeStats {
                    packets_seen: child_ingress.packets_seen,
                    tx_packets: child_ingress.tx_packets,
                    sockets_touched: child_ingress.sockets_touched,
                },
                publishes,
            };
        }
        if !progressed {
            break;
        }
    }

    LoopbackHandshakeDrive::Pending { publishes }
}

enum LoopbackHandshakeDrive {
    Complete {
        child: Cap<SocketIdentity>,
        stats: LoopbackTcpHandshakeStats,
        publishes: alloc::vec::Vec<NetworkPublishTarget>,
    },
    Pending {
        publishes: alloc::vec::Vec<NetworkPublishTarget>,
    },
    Failed,
}

fn ensure_connection_entry(
    table: &SocketTable,
    key: ConnectionKey,
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> Result<bool, Errno> {
    match table.insert_tcp_connection(key, socket.clone()) {
        Ok(()) => Ok(true),
        Err(IndexError::Duplicate) => table
            .lookup_tcp_connection(key, guard)
            .filter(|existing| existing.raw() == socket.raw())
            .map(|_| false)
            .ok_or(Errno::EADDRINUSE),
        Err(IndexError::Full) => Err(Errno::ENOMEM),
        Err(IndexError::Busy) => Err(Errno::EAGAIN),
        Err(IndexError::Missing) => Err(Errno::EINVAL),
    }
}

fn ctx_packets_seen(ctx: &PollContext) -> usize {
    ctx.packets_seen()
}

fn connecting_endpoints(protocol: &SocketProtocol) -> Result<(IpEndpoint, IpEndpoint), Errno> {
    match protocol {
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => Ok((*local, *remote)),
        SocketProtocol::Tcp(TcpState::Connected { .. }) => Err(Errno::EISCONN),
        SocketProtocol::Tcp(_) => Err(Errno::EINVAL),
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
