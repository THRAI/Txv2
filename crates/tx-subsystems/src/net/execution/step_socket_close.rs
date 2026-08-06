use tx_substrate::zone::{Cap, PayloadCap};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::net::delegate::net_delegate_kick_poll;
use crate::net::structure::table::SocketTable;
use crate::net::structure::SocketPayload;
use crate::net::structure::{
    AcceptWireSet, ConnectionKey, IpEndpoint, RdsState, RecvWireSet, SendWireSet, SocketIdentity,
    SocketProtocol, TcpState, UdpInner, UnixDatagramState, UnixStreamState,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SocketCloseOutcome {
    pub payload_taken: bool,
    pub bindings_withdrawn: usize,
    pub tcp_flushed_bytes: usize,
    pub recv_woken: usize,
    pub send_woken: usize,
    pub accept_woken: usize,
}

pub fn step_socket_close(
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> StepOutcome<SocketCloseOutcome> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    let Some(payload) = socket.live_payload() else {
        return StepOutcome::Done(SocketCloseOutcome::default());
    };

    let mut bindings_withdrawn = 0;
    let tcp_flushed_bytes = 0;
    let mut peer_recv_woken = 0;
    let mut peer_send_woken = 0;
    let mut deferred_tcp_close = false;
    let table = payload.socket_table();
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Bound { local }) => {
            bindings_withdrawn += withdraw_ok(table.withdraw_tcp_bound(local));
        }
        SocketProtocol::Tcp(TcpState::Listening { local, .. }) => {
            bindings_withdrawn += withdraw_ok(table.withdraw_tcp_listener(local));
            bindings_withdrawn += withdraw_ok(table.withdraw_tcp_bound(local));
            // R2b: drain the accept backlog. Accept-ready children were
            // double-registered into the connections table at handshake
            // time; withdraw them so their strong Cap (and the ns it
            // pins) is released. Half-open children drop with the queue.
            for entry in payload.drain_backlog_for_close() {
                bindings_withdrawn += withdraw_ok(
                    table.withdraw_tcp_connection(ConnectionKey::new(entry.local, entry.peer)),
                );
            }
        }
        SocketProtocol::Tcp(TcpState::Connecting { local, remote }) => {
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_tcp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn += withdraw_tcp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Tcp(TcpState::Connected { .. }) => {
            // close(2) must not destroy a stream while its HTTP response is
            // still queued.  Keep the table-owned socket alive and let the
            // delegate drive data plus FIN to completion.
            deferred_tcp_close = true;
            if payload.request_tcp_close() {
                if let Some(raw_tcp) = payload.raw_tcp_socket() {
                    raw_tcp.close();
                }
                net_delegate_kick_poll();
            }
        }
        SocketProtocol::Tcp(TcpState::Init | TcpState::Closed) => {}
        SocketProtocol::Sctp(TcpState::Bound { local }) => {
            peer_recv_woken += notify_sctp_seqpacket_peers_closed(&payload, table, guard);
            bindings_withdrawn += withdraw_ok(table.withdraw_sctp_bound(local));
        }
        SocketProtocol::Sctp(TcpState::Listening { local, .. }) => {
            peer_recv_woken += notify_sctp_seqpacket_peers_closed(&payload, table, guard);
            bindings_withdrawn += withdraw_ok(table.withdraw_sctp_listener(local));
            bindings_withdrawn += withdraw_ok(table.withdraw_sctp_bound(local));
            // R2b: SCTP listeners share the same backlog; accept-ready
            // children were registered into the sctp connections table.
            for entry in payload.drain_backlog_for_close() {
                bindings_withdrawn += withdraw_ok(
                    table.withdraw_sctp_connection(ConnectionKey::new(entry.local, entry.peer)),
                );
            }
        }
        SocketProtocol::Sctp(TcpState::Connecting { local, remote })
        | SocketProtocol::Sctp(TcpState::Connected { local, remote }) => {
            if let Some(peer) =
                table.lookup_sctp_connection(ConnectionKey::new(remote, local), guard)
            {
                // Notify the peer's event subscription that the association is
                // going down (SCTP_SHUTDOWN_EVENT), queued ahead of the EOF mark.
                enqueue_sctp_shutdown_event(&peer);
                let peer_wakes = mark_sctp_peer_closed(&peer);
                peer_recv_woken += peer_wakes.recv_woken;
                peer_send_woken += peer_wakes.send_woken;
            } else {
                // No 1-to-1 connection peer: a peeled-off socket whose peer is a
                // 1-to-many client. Deliver the SHUTDOWN_COMP assoc_change to the
                // client's association.
                peer_recv_woken +=
                    notify_sctp_peer_assoc_closed(local, remote, 0, false, table, guard);
            }
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_sctp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn += withdraw_sctp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Sctp(TcpState::Init | TcpState::Closed) => {}
        SocketProtocol::Rds(RdsState::Bound { local }) => {
            bindings_withdrawn += withdraw_rds_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Rds(RdsState::Unbound | RdsState::Closed) => {}
        SocketProtocol::Udp(UdpInner::Bound { local }) => {
            bindings_withdrawn += withdraw_udp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Udp(UdpInner::Connected { local, remote }) => {
            bindings_withdrawn +=
                withdraw_ok(table.withdraw_udp_connection(ConnectionKey::new(local, remote)));
            bindings_withdrawn += withdraw_udp_bound_if_owner(table, socket, local, guard);
        }
        SocketProtocol::Udp(UdpInner::Unbound | UdpInner::Closed) => {}
        SocketProtocol::RawIcmp(_) => {
            bindings_withdrawn += withdraw_ok(table.withdraw_raw_icmp(socket.raw()));
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Bound { local }) => {
            bindings_withdrawn += withdraw_unix_binding_on_close(table, local);
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { local, .. }) => {
            if let Some(local) = local {
                bindings_withdrawn += withdraw_unix_binding_on_close(table, local);
            }
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { peer_raw }) => {
            if let Some(peer) = table.lookup_unix_peer(socket.raw(), guard) {
                mark_unix_peer_broken(&peer);
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_peer(peer.raw()));
            } else if peer_raw != 0 {
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_peer(peer_raw));
            }
            bindings_withdrawn += withdraw_ok(table.withdraw_unix_peer(socket.raw()));
        }
        SocketProtocol::UnixDatagram(UnixDatagramState::Unbound) => {}
        SocketProtocol::UnixStream(UnixStreamState::Bound { local }) => {
            bindings_withdrawn += withdraw_unix_binding_on_close(table, local);
        }
        SocketProtocol::UnixStream(UnixStreamState::Listening { local, .. }) => {
            bindings_withdrawn += withdraw_unix_binding_on_close(table, local);
            // R2b: UnixStream listeners share the backlog; accept-ready
            // children were registered as stream peers keyed by raw().
            for entry in payload.drain_backlog_for_close() {
                bindings_withdrawn +=
                    withdraw_ok(table.withdraw_unix_stream_peer(entry.child.raw()));
            }
        }
        SocketProtocol::UnixStream(UnixStreamState::Connected { peer_raw, .. }) => {
            if let Some(peer) = table.lookup_unix_stream_peer(socket.raw(), guard) {
                mark_unix_peer_broken(&peer);
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_stream_peer(peer.raw()));
            } else if peer_raw != 0 {
                bindings_withdrawn += withdraw_ok(table.withdraw_unix_stream_peer(peer_raw));
            }
            bindings_withdrawn += withdraw_ok(table.withdraw_unix_stream_peer(socket.raw()));
        }
        SocketProtocol::UnixStream(UnixStreamState::Init | UnixStreamState::Closed) => {}
        SocketProtocol::Packet(_) => {
            bindings_withdrawn += withdraw_ok(table.withdraw_packet_socket(socket.raw()));
        }
        SocketProtocol::NetlinkRoute(_) | SocketProtocol::NetlinkNetfilter(_) => {}
    }

    if deferred_tcp_close {
        let recv_woken = socket.readiness.fire_recv(RecvWireSet::BROKEN);
        let send_woken = socket.readiness.fire_send(SendWireSet::BROKEN);
        let accept_woken = socket.readiness.fire_accept(AcceptWireSet::BROKEN);
        return StepOutcome::Done(SocketCloseOutcome {
            payload_taken: false,
            bindings_withdrawn,
            tcp_flushed_bytes,
            recv_woken,
            send_woken,
            accept_woken,
        });
    }

    if let Some(raw_tcp) = payload.raw_tcp_socket() {
        raw_tcp.abort();
    }
    if let Some(raw_udp) = payload.raw_udp_socket() {
        raw_udp.close();
    }
    let payload_taken = socket.take_payload().is_some();
    let recv_woken = peer_recv_woken + socket.readiness.fire_recv(RecvWireSet::BROKEN);
    let send_woken = peer_send_woken + socket.readiness.fire_send(SendWireSet::BROKEN);
    let accept_woken = socket.readiness.fire_accept(AcceptWireSet::BROKEN);

    StepOutcome::Done(SocketCloseOutcome {
        payload_taken,
        bindings_withdrawn,
        tcp_flushed_bytes,
        recv_woken,
        send_woken,
        accept_woken,
    })
}

/// Release a delegate-owned connected TCP socket only after every queued byte
/// and the FIN exchange have completed.
pub(super) fn finalize_tcp_close_if_complete(
    socket: &Cap<SocketIdentity>,
    guard: &Guard<'_>,
) -> bool {
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    if !payload.tcp_close_requested() {
        return false;
    }
    let SocketProtocol::Tcp(TcpState::Connected { local, remote }) = payload.protocol_snapshot()
    else {
        return false;
    };
    let Some(raw_tcp) = payload.raw_tcp_socket() else {
        return false;
    };
    if raw_tcp.send_queued() != 0
        || !matches!(
            raw_tcp.protocol_state(),
            smoltcp::socket::tcp::State::Closed | smoltcp::socket::tcp::State::TimeWait
        )
    {
        return false;
    }

    let table = payload.socket_table();
    let _ = table.withdraw_tcp_connection(ConnectionKey::new(local, remote));
    let _ = withdraw_tcp_bound_if_owner(table, socket, local, guard);
    let _ = socket.take_payload();
    socket.readiness.fire_recv(RecvWireSet::BROKEN);
    socket.readiness.fire_send(SendWireSet::BROKEN);
    socket.readiness.fire_accept(AcceptWireSet::BROKEN);
    true
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PeerCloseWakes {
    recv_woken: usize,
    send_woken: usize,
}

fn mark_sctp_peer_closed(peer: &Cap<SocketIdentity>) -> PeerCloseWakes {
    let recv_woken = peer.readiness.fire_recv(RecvWireSet::BROKEN);
    let send_woken = peer.readiness.fire_send(SendWireSet::BROKEN);
    PeerCloseWakes {
        recv_woken,
        send_woken,
    }
}

/// When a 1-to-many (SEQPACKET) socket closes, deliver SCTP_SHUTDOWN_COMP to each
/// peer association whose socket subscribed to association events. Returns the
/// number of peer recv waiters woken.
fn notify_sctp_seqpacket_peers_closed(
    payload: &PayloadCap<SocketPayload>,
    table: &SocketTable,
    guard: &Guard<'_>,
) -> usize {
    let mut woken = 0;
    // This socket's local address, used to compute the source address each peer
    // observed for it (a wildcard bind resolves to the peer's loopback address).
    let local = match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Bound { local })
        | SocketProtocol::Sctp(TcpState::Listening { local, .. })
        | SocketProtocol::Sctp(TcpState::Connecting { local, .. })
        | SocketProtocol::Sctp(TcpState::Connected { local, .. }) => local,
        _ => return woken,
    };
    for assoc in payload.sctp_peers() {
        woken +=
            notify_sctp_peer_assoc_closed(local, assoc.peer, assoc.assoc_id, false, table, guard);
    }
    woken
}

/// Deliver the teardown notification(s) for a single 1-to-many association to its
/// peer socket: SCTP_SHUTDOWN_EVENT and/or SHUTDOWN_COMP (assoc_change) per the
/// peer's subscription, each carrying the source (the peer's view of this
/// socket's local address) and the peer's own association id. Returns the number
/// of tasks woken. `local` is this socket's local address; `peer_endpoint` is the
/// association's peer; `fallback_assoc_id` is used if the peer has no matching
/// association recorded. When `abort` is set the teardown was an ungraceful
/// SCTP_ABORT: the peer gets a single COMM_LOST assoc_change (24 bytes) and no
/// SHUTDOWN_EVENT, matching Linux/lksctp.
fn notify_sctp_peer_assoc_closed(
    local: IpEndpoint,
    peer_endpoint: IpEndpoint,
    fallback_assoc_id: u32,
    abort: bool,
    table: &SocketTable,
    guard: &Guard<'_>,
) -> usize {
    let Some(peer) = table
        .lookup_sctp_listener_dual_stack_endpoint(peer_endpoint, guard)
        .or_else(|| table.lookup_sctp_bound(peer_endpoint, guard))
    else {
        return 0;
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return 0;
    };
    let wants_shutdown = peer_payload.with_options(|o| o.sctp.event_shutdown());
    let wants_assoc_change = peer_payload.with_options(|o| o.sctp.event_assoc_change());
    if !wants_shutdown && !wants_assoc_change {
        return 0;
    }
    let source = if local.is_unspecified() {
        IpEndpoint::from_ip(peer_endpoint.ip_addr(), local.port)
    } else {
        local
    };
    let peer_assoc_id = peer_payload
        .sctp_peers()
        .into_iter()
        .find(|a| a.peer == source)
        .map_or(fallback_assoc_id, |a| a.assoc_id);
    let mut fired = false;
    // SCTP_SHUTDOWN_EVENT is delivered when the peer receives SHUTDOWN;
    // SHUTDOWN_COMP (an assoc_change) when the association is fully torn down. A
    // 1-to-many socket may subscribe to either or both. An ungraceful ABORT has
    // no graceful SHUTDOWN phase, so it emits no SHUTDOWN_EVENT.
    if wants_shutdown && !abort {
        let bytes = crate::net::execution::sctp_shutdown_event_bytes();
        fired |= peer_payload
            .record_sctp_message(bytes, true, 0, 0, Some(source))
            .is_some();
    }
    if wants_assoc_change {
        let streams = peer_payload.with_options(|o| o.sctp.initmsg_num_ostreams);
        let bytes = if abort {
            crate::net::execution::sctp_assoc_change_abort_bytes(streams, peer_assoc_id)
        } else {
            crate::net::execution::sctp_assoc_change_bytes(
                3, /* SHUTDOWN_COMP */
                streams,
                peer_assoc_id,
            )
        };
        fired |= peer_payload
            .record_sctp_message(bytes, true, 0, 0, Some(source))
            .is_some();
    }
    if fired {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA)
    } else {
        0
    }
}

/// SCTP_EOF/SCTP_ABORT on a 1-to-many (SEQPACKET) socket: tear down the single
/// association named by `assoc_id` — notify its peer (SHUTDOWN_EVENT/COMP for a
/// graceful EOF, or a single COMM_LOST for an ungraceful `abort`) and drop the
/// association from this socket. A no-op if no such association exists.
pub fn step_sctp_shutdown_assoc(
    socket: &Cap<SocketIdentity>,
    assoc_id: u32,
    abort: bool,
    guard: &Guard<'_>,
) -> StepOutcome<()> {
    let Some(payload) = socket.acquire_operational() else {
        return StepOutcome::Err(Errno::ENOTCONN);
    };
    let local = match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Bound { local })
        | SocketProtocol::Sctp(TcpState::Listening { local, .. })
        | SocketProtocol::Sctp(TcpState::Connecting { local, .. })
        | SocketProtocol::Sctp(TcpState::Connected { local, .. }) => local,
        _ => return StepOutcome::Err(Errno::EINVAL),
    };
    let Some(peer_endpoint) = payload.sctp_peer_addr_by_assoc(assoc_id) else {
        return StepOutcome::Done(());
    };
    let table = payload.socket_table();
    notify_sctp_peer_assoc_closed(local, peer_endpoint, assoc_id, abort, table, guard);
    payload.sctp_remove_assoc(assoc_id);
    StepOutcome::Done(())
}

/// Queue an SCTP_SHUTDOWN_EVENT notification on `peer`'s receive queue if it
/// subscribed to shutdown events, so its next recvmsg surfaces the teardown.
fn enqueue_sctp_shutdown_event(peer: &Cap<SocketIdentity>) {
    let Some(payload) = peer.acquire_operational() else {
        return;
    };
    if !payload.with_options(|o| o.sctp.event_shutdown()) {
        return;
    }
    let bytes = crate::net::execution::sctp_shutdown_event_bytes();
    if payload
        .record_sctp_message(bytes, true, 0, 0, None)
        .is_some()
    {
        peer.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
}

fn withdraw_ok<T>(result: Result<T, tx_substrate::mutation::MutationError>) -> usize {
    usize::from(result.is_ok())
}

fn withdraw_unix_binding_on_close(
    table: &SocketTable,
    local: crate::net::structure::UnixSocketPath,
) -> usize {
    if local.is_abstract() {
        withdraw_ok(table.unlink_unix_path(local))
    } else {
        // Pathname sockets follow Linux persist-until-unlink semantics: close
        // withdraws the live binding but the path node survives until an
        // explicit unlink(2). Cross-test name collisions are prevented by
        // cwd-normalizing the path key at bind/connect time (see helpers.rs),
        // not by releasing the node here (that breaks bind/close/unlink tests).
        withdraw_ok(table.withdraw_unix_bound(local))
    }
}

fn withdraw_udp_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_udp_bound_exact(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_udp_bound(local))
}

fn withdraw_tcp_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_tcp_bound(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_tcp_bound(local))
}

fn withdraw_sctp_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_sctp_bound(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_sctp_bound(local))
}

fn withdraw_rds_bound_if_owner(
    table: &SocketTable,
    socket: &Cap<SocketIdentity>,
    local: crate::net::structure::IpEndpoint,
    guard: &Guard<'_>,
) -> usize {
    let Some(bound) = table.lookup_rds_bound(local, guard) else {
        return 0;
    };
    if bound.raw() != socket.raw() {
        return 0;
    }
    withdraw_ok(table.withdraw_rds_bound(local))
}

fn mark_unix_peer_broken(peer: &Cap<SocketIdentity>) {
    peer.readiness.fire_recv(RecvWireSet::BROKEN);
    peer.readiness.fire_send(SendWireSet::BROKEN);
}
