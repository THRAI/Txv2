use super::*;

use tx_services::time::{timekeeper_clock, ClockRead, TimekeeperClock};

pub(super) fn connect_sockaddr_for_local_stack(
    kind: SocketKind,
    remote: KernelSockAddr,
) -> KernelSockAddr {
    if kind != SocketKind::Tcp && kind != SocketKind::Udp && kind != SocketKind::Sctp {
        return remote;
    }
    match remote {
        KernelSockAddr::V4(sockaddr) if sockaddr.addr == Ipv4Address::UNSPECIFIED => {
            KernelSockAddr::V4(SockAddrIn::new(sockaddr.port, Ipv4Address::LOOPBACK))
        }
        KernelSockAddr::V6(sockaddr) if sockaddr.addr == Ipv6Address::UNSPECIFIED => {
            KernelSockAddr::V6(SockAddrIn6::new(sockaddr.port, Ipv6Address::LOOPBACK))
        }
        _ => remote,
    }
}

pub(super) fn maybe_autobind_connect_client(
    socket: &Cap<SocketIdentity>,
    remote: KernelSockAddr,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::Tcp
        && socket.kind != SocketKind::Udp
        && socket.kind != SocketKind::Sctp
    {
        return Ok(());
    }
    if matches!(remote, KernelSockAddr::Unspec) {
        return Ok(());
    }

    let remote_endpoint = remote.as_ip_endpoint();
    let local_endpoint_base = connect_autobind_local_base(socket, remote_endpoint);

    for port in super::ephemeral_port_candidates() {
        let local_endpoint = IpEndpoint::from_ip(local_endpoint_base.ip_addr(), port);
        if ephemeral_port_in_use(socket, port) {
            continue;
        }
        if matches!(socket.kind, SocketKind::Tcp | SocketKind::Sctp)
            && tcp_connect_tuple_in_use(socket, local_endpoint, remote_endpoint)
        {
            continue;
        }
        let local = sockaddr_from_endpoint(local_endpoint);
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_bind(socket, local, &guard)
        };
        match outcome {
            StepOutcome::Done(()) => return Ok(()),
            StepOutcome::Err(Errno::EADDRINUSE) => continue,
            StepOutcome::Err(Errno::EINVAL) => return Ok(()),
            StepOutcome::Err(errno) => return Err(errno),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => return Err(Errno::EIO),
        }
    }
    Err(Errno::EADDRINUSE)
}

fn connect_autobind_local_base(
    socket: &Cap<SocketIdentity>,
    remote_endpoint: IpEndpoint,
) -> IpEndpoint {
    if remote_endpoint.is_loopback() || remote_endpoint.is_unspecified() {
        return IpEndpoint::loopback_for_family(remote_endpoint.family, 0);
    }
    if !matches!(socket.kind, SocketKind::Tcp | SocketKind::Sctp) {
        return IpEndpoint::unspecified_for_family(remote_endpoint.family, 0);
    }
    let Some(payload) = socket.acquire_operational() else {
        return IpEndpoint::unspecified_for_family(remote_endpoint.family, 0);
    };
    match remote_endpoint.ip_addr() {
        tx_subsystems::net::structure::IpAddress::V4(dst) => payload
            .net_namespace()
            .best_ipv4_route(dst)
            .and_then(|route| {
                route.preferred_src.or_else(|| {
                    payload
                        .net_namespace()
                        .link_snapshot()
                        .into_iter()
                        .find(|link| {
                            link.name == route.oif_name
                                && link.is_up
                                && !link.is_loopback
                                && link.ipv4_addr.is_some()
                        })
                        .and_then(|link| link.ipv4_addr)
                })
            })
            .map_or_else(
                || IpEndpoint::unspecified_for_family(remote_endpoint.family, 0),
                |src| IpEndpoint::new(src, 0),
            ),
        // V5-3: routed like the V4 arm above (was "first up non-loopback link
        // with any v6 address", FIB-blind). No route to `dst` => unspecified
        // local => `step_connect` returns EADDRNOTAVAIL immediately, which is
        // what keeps an unreachable global v6 destination from hanging.
        tx_subsystems::net::structure::IpAddress::V6(dst) => payload
            .net_namespace()
            .preferred_ipv6_source(dst)
            .map_or_else(
                || IpEndpoint::unspecified_for_family(remote_endpoint.family, 0),
                |src| IpEndpoint::new_v6(src, 0),
            ),
    }
}

pub(super) fn tcp_connect_tuple_in_use(
    socket: &Cap<SocketIdentity>,
    local: IpEndpoint,
    remote: IpEndpoint,
) -> bool {
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    let table = payload.socket_table();
    let guard = tx_substrate::epoch::guard();
    match socket.kind {
        SocketKind::Sctp => {
            table
                .lookup_sctp_connection(ConnectionKey::new(local, remote), &guard)
                .is_some()
                || table
                    .lookup_sctp_connection(ConnectionKey::new(remote, local), &guard)
                    .is_some()
        }
        _ => {
            table
                .lookup_tcp_connection(ConnectionKey::new(local, remote), &guard)
                .is_some()
                || table
                    .lookup_tcp_connection(ConnectionKey::new(remote, local), &guard)
                    .is_some()
        }
    }
}

pub(super) fn maybe_autobind_udp_sendto(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::Udp {
        return Ok(());
    }
    let Some(payload) = socket.acquire_operational() else {
        return Err(Errno::ENOTCONN);
    };
    if !matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Udp(UdpInner::Unbound)
    ) {
        return Ok(());
    }

    let family = dst
        .map(|endpoint| endpoint.family)
        .or_else(|| socket.acquire_operational().map(|payload| payload.family()))
        .unwrap_or(socket.family);
    let local_endpoint_base = if dst.is_some_and(|endpoint| endpoint.is_loopback()) {
        IpEndpoint::loopback_for_family(family, 0)
    } else {
        IpEndpoint::unspecified_for_family(family, 0)
    };

    for port in super::ephemeral_port_candidates() {
        let local =
            sockaddr_from_endpoint(IpEndpoint::from_ip(local_endpoint_base.ip_addr(), port));
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_bind(socket, local, &guard)
        };
        match outcome {
            StepOutcome::Done(()) => return Ok(()),
            StepOutcome::Err(Errno::EADDRINUSE) => continue,
            StepOutcome::Err(Errno::EINVAL) => return Ok(()),
            StepOutcome::Err(errno) => return Err(errno),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => return Err(Errno::EIO),
        }
    }
    Err(Errno::EADDRINUSE)
}

pub(super) fn drive_tcp_loopback_after_connect(
    socket: &Cap<SocketIdentity>,
) -> Result<bool, Errno> {
    let Some(payload) = socket.acquire_operational() else {
        return Ok(false);
    };
    if !matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connecting { .. })
    ) {
        return Ok(false);
    }
    let guard = tx_substrate::epoch::guard();
    match step_tcp_loopback_handshake(socket, &guard) {
        StepOutcome::Done(_) => Ok(true),
        StepOutcome::Err(Errno::EOPNOTSUPP) => Ok(false),
        StepOutcome::Err(errno) => Err(errno),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Ok(false),
    }
}

pub(super) fn bind_with_ephemeral_port(
    socket: &Cap<SocketIdentity>,
    addr: KernelSockAddr,
) -> SyscallResult {
    let requested = addr.as_ip_endpoint();
    if requested.port != 0 {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_bind(socket, addr, &guard)
        };
        return step_unit_result(outcome);
    }

    for port in super::ephemeral_port_candidates() {
        if ephemeral_port_in_use(socket, port) {
            continue;
        }
        let local = sockaddr_from_endpoint(IpEndpoint::from_ip(requested.ip_addr(), port));
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_bind(socket, local, &guard)
        };
        match outcome {
            StepOutcome::Done(()) => return SyscallResult::Return(0),
            StepOutcome::Err(Errno::EADDRINUSE) => continue,
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
        }
    }
    SyscallResult::Error(errno_to_i32(Errno::EADDRINUSE))
}

pub(super) fn ephemeral_port_in_use(socket: &Cap<SocketIdentity>, port: u16) -> bool {
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    let table = payload.socket_table();
    let guard = tx_substrate::epoch::guard();
    let family = payload.family();

    match socket.kind {
        SocketKind::Tcp => table
            .snapshot_tcp_bound(&guard)
            .into_iter()
            .chain(table.snapshot_tcp_listeners(&guard))
            .any(|existing| {
                existing.raw() != socket.raw() && occupies_tcp_port(&existing, family, port)
            }),
        SocketKind::Sctp => table
            .snapshot_sctp_bound(&guard)
            .into_iter()
            .chain(table.snapshot_sctp_listeners(&guard))
            .any(|existing| {
                existing.raw() != socket.raw() && occupies_tcp_port(&existing, family, port)
            }),
        SocketKind::Udp => table
            .snapshot_udp_bound(&guard)
            .into_iter()
            .any(|existing| {
                existing.raw() != socket.raw()
                    && socket_local_endpoint(&existing)
                        .is_ok_and(|local| local.family == family && local.port == port)
            }),
        _ => false,
    }
}

fn occupies_tcp_port(socket: &Cap<SocketIdentity>, family: AddressFamily, port: u16) -> bool {
    let Ok(local) = socket_local_endpoint(socket) else {
        return false;
    };
    if local.port != port {
        return false;
    }
    if local.family == family {
        return true;
    }
    if family != AddressFamily::Inet
        || local.family != AddressFamily::Inet6
        || !local.is_unspecified()
    {
        return false;
    }
    socket
        .acquire_operational()
        .is_some_and(|payload| !payload.with_options(|options| options.ip.ipv6_v6only))
}

pub(super) fn drive_tcp_loopback_after_sendto(socket: &Cap<SocketIdentity>, written: usize) {
    if written == 0 {
        return;
    }
    let Some(payload) = socket.acquire_operational() else {
        return;
    };
    if !matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { .. })
    ) {
        return;
    }
    let guard = tx_substrate::epoch::guard();
    let _ = step_tcp_loopback_transfer(socket, written, &guard);
    socket
        .readiness
        .clear_send(tx_subsystems::net::structure::SendWireSet::SPACE);
}

pub(super) fn drive_udp_loopback_after_sendto(socket: &Cap<SocketIdentity>, written: usize) -> bool {
    if written == 0 {
        return false;
    }
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    if !matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Udp(UdpInner::Bound { .. } | UdpInner::Connected { .. })
    ) {
        return false;
    }
    let guard = tx_substrate::epoch::guard();
    matches!(
        step_process_loopback_udp(socket, 8, &guard),
        StepOutcome::Done(outcome) if outcome.bytes_moved > 0 || outcome.tx_packets > 0
    )
}

pub(super) fn drive_loopback_after_sendto(socket: &Cap<SocketIdentity>, written: usize) -> bool {
    drive_tcp_loopback_after_sendto(socket, written);
    drive_udp_loopback_after_sendto(socket, written)
}

pub(super) async fn yield_after_sendto_if_needed(socket: &Cap<SocketIdentity>) {
    if socket.kind != SocketKind::Udp {
        tx_reactor::yield_now().await;
    }
}

pub(super) async fn finish_sendto_progress(
    ctx: &SyscallCtx<'_>,
    socket: &Cap<SocketIdentity>,
    written: usize,
    flags: SendRecvFlags,
) {
    if flags.contains(SendRecvFlags::MSG_MORE) {
        return;
    }
    let _ = drive_loopback_after_sendto(socket, written);
    yield_after_sendto_if_needed(socket).await;
}

pub(crate) fn drive_loopback_pending() {
    let guard = tx_substrate::epoch::guard();
    let _ = tx_subsystems::net::execution::step_process_loopback_pending_zero(
        tx_subsystems::net::protocol::loopback_iface(),
        tx_subsystems::net::execution::LOOPBACK_POLL_BUDGET_DEFAULT,
        &guard,
    );
}

pub(super) fn recv_ready_mask(mask: PollMask) -> bool {
    mask.intersects(PollMask::IN | PollMask::ERR | PollMask::HUP | PollMask::RDHUP)
}

pub(super) fn recv_staging_len(socket: &Cap<SocketIdentity>, requested: usize) -> usize {
    // Socket recv stages onto the heap, so cap at the socket I/O size
    // (`SOCKET_IO_MAX_INLINE`, 64 KiB) rather than the TTY 4 KiB inline limit.
    // The rebase that dropped the socket I/O lane left this at 4 KiB, which
    // throttled a streaming TCP receiver to ~4 KiB/syscall (iperf3's server did
    // ~320 recvs for one 1.25 MB test → ~0.86 Mbit/s under TCG). Still
    // `min(requested, queued)`-bounded, so small reads stay small.
    let capped = requested.min(SOCKET_IO_MAX_INLINE);
    let ready = recv_queued_len(socket);
    if ready == 0 {
        capped
    } else {
        capped.min(ready)
    }
}

pub(super) fn recv_queued_len(socket: &Cap<SocketIdentity>) -> usize {
    let Some(payload) = socket.acquire_operational() else {
        return 0;
    };
    payload.io_snapshot().recv_len
}

pub(super) fn socket_recv_should_yield_after_success(
    socket: &Cap<SocketIdentity>,
    bytes: usize,
) -> bool {
    bytes > 0 && matches!(socket.kind, SocketKind::Tcp | SocketKind::Sctp)
}

pub(super) fn sendto_can_drive_loopback_inline(
    socket: &Cap<SocketIdentity>,
    dst: Option<IpEndpoint>,
) -> bool {
    if socket.kind != SocketKind::Udp {
        return false;
    }
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    match payload.protocol_snapshot() {
        SocketProtocol::Udp(UdpInner::Bound { local }) => {
            dst.is_some_and(|dst| local_allows_loopback_inline(local) && dst.is_loopback())
        }
        SocketProtocol::Udp(UdpInner::Connected { local, remote }) => {
            local_allows_loopback_inline(local) && remote.is_loopback()
        }
        _ => false,
    }
}

pub(super) fn local_allows_loopback_inline(local: IpEndpoint) -> bool {
    local.is_unspecified() || local.is_loopback()
}

pub(super) fn tcp_effective_maxseg(socket: &Cap<SocketIdentity>) -> i32 {
    let route_mss = tcp_route_maxseg(socket);
    let configured = socket
        .acquire_operational()
        .map(|payload| payload.with_options(|opts| opts.tcp.maxseg))
        .unwrap_or(0);
    let maxseg = if configured == 0 {
        route_mss
    } else {
        configured.min(route_mss)
    };
    i32::from(maxseg)
}

pub(super) fn tcp_route_maxseg(socket: &Cap<SocketIdentity>) -> u16 {
    let mtu = match socket_peer_endpoint(socket) {
        Ok(peer) if peer.is_loopback() => loopback_iface().mtu(),
        _ => match socket_local_endpoint(socket) {
            Ok(local) if local.is_loopback() => loopback_iface().mtu(),
            _ => VIRTIO_NET_DEFAULT_MTU,
        },
    };
    mtu.saturating_sub(IPV4_TCP_HEADER_BYTES).max(1)
}

fn sockaddr_from_endpoint(endpoint: IpEndpoint) -> KernelSockAddr {
    match endpoint.family {
        AddressFamily::Inet6 => KernelSockAddr::V6(SockAddrIn6::new(endpoint.port, endpoint.addr6)),
        _ => KernelSockAddr::V4(SockAddrIn::new(endpoint.port, endpoint.addr)),
    }
}

pub(super) fn resolve_socket_fd<'a>(
    ctx: &SyscallCtx<'a>,
    fd: i32,
) -> Result<(Cap<OpenFile>, Cap<SocketIdentity>), Errno> {
    if fd < 0 {
        return Err(Errno::EBADF);
    }
    let file = resolve_fd(&ctx.process, fd as u32).ok_or(Errno::EBADF)?;
    let socket = socket_identity_from_file(&file)?;
    Ok((file, socket))
}

pub(crate) fn socket_identity_from_file(
    file: &Cap<OpenFile>,
) -> Result<Cap<SocketIdentity>, Errno> {
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Socket { identity },
            } => Ok(identity.clone()),
            _ if open_file_is_path_only(file) => Err(Errno::EBADF),
            _ => Err(Errno::ENOTSOCK),
        },
        // An `open_tree(2)` fd is path-only (like `O_PATH`), so a socket syscall
        // on it returns EBADF; `fsopen`/`fspick` mount-API fds are readable, so
        // they fall through to ENOTSOCK. Matches `accept03`'s expectations.
        OpenFileBacking::MountApi { file }
            if file.kind() == tx_subsystems::mount::MountApiFileKind::OpenTree =>
        {
            Err(Errno::EBADF)
        }
        _ => Err(Errno::ENOTSOCK),
    }
}

pub(super) fn open_file_is_path_only(file: &OpenFile) -> bool {
    let flags = file.flags();
    !flags.read && !flags.write
}

pub(crate) fn socket_poll_mask_from_file(
    file: &Cap<OpenFile>,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> Option<Result<PollMask, Errno>> {
    let socket = match socket_identity_from_file(file) {
        Ok(socket) => socket,
        Err(Errno::ENOTSOCK) => return None,
        Err(errno) => return Some(Err(errno)),
    };
    Some(match step_poll_ready(&socket, guard) {
        StepOutcome::Done(mask) => Ok(mask),
        StepOutcome::Err(errno) => Err(errno),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Ok(PollMask::empty()),
    })
}

pub(crate) fn socket_poll_wait_token_from_file(
    file: &Cap<OpenFile>,
    interests: PollMask,
    guard: &tx_substrate::epoch::Guard<'_>,
) -> Option<Result<Option<tx_subsystems::execution::WaitToken>, Errno>> {
    let socket = match socket_identity_from_file(file) {
        Ok(socket) => socket,
        Err(Errno::ENOTSOCK) => return None,
        Err(errno) => return Some(Err(errno)),
    };
    Some(match step_poll_wait_token(&socket, interests, guard) {
        StepOutcome::Done(token) => Ok(token),
        StepOutcome::Err(errno) => Err(errno),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Ok(None),
    })
}

pub(super) fn read_sockaddr_in<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len: u64,
) -> Result<KernelSockAddr, Errno> {
    if sockaddr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if sockaddr_len > i32::MAX as u64 {
        return Err(Errno::EINVAL);
    }
    if sockaddr_len < 2 {
        return Err(Errno::EINVAL);
    }

    let copy_len = core::cmp::min(sockaddr_len, SOCKADDR_IN6_BYTES as u64) as usize;
    let mut bytes = [0u8; SOCKADDR_IN6_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes[..copy_len], sockaddr_ptr)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    match family {
        AF_INET => {
            if sockaddr_len < SOCKADDR_IN_BYTES as u64 {
                return Err(Errno::EINVAL);
            }
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let addr = Ipv4Address::new([bytes[4], bytes[5], bytes[6], bytes[7]]);
            Ok(KernelSockAddr::V4(SockAddrIn::new(port, addr)))
        }
        AF_INET6 => {
            if sockaddr_len < SOCKADDR_IN6_BYTES as u64 {
                return Err(Errno::EINVAL);
            }
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let flowinfo = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
            let addr = Ipv6Address::new(bytes[8..24].try_into().unwrap());
            let scope_id = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
            Ok(KernelSockAddr::V6(SockAddrIn6 {
                family,
                port,
                flowinfo,
                addr,
                scope_id,
            }))
        }
        0 => Ok(KernelSockAddr::Unspec),
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

/// Derive the cwd-absolute bind/connect key for an `AF_UNIX` pathname socket.
/// Used by `unlink(2)` (fs_mut) to match the bound key for relative pathname
/// sockets. Restored alongside the net subsystem re-home.
pub(crate) fn unix_pathname_key(
    ctx: &SyscallCtx<'_>,
    raw_path: &[u8],
) -> Result<UnixSocketPath, Errno> {
    match raw_path.first() {
        Some(&0) | Some(&b'/') | None => UnixSocketPath::new(raw_path),
        Some(_) => {
            if let Some(cwd) = ctx.process.cwd() {
                if let Some(mut abs) = tx_subsystems::vfs::render_dentry_path(&cwd) {
                    if abs.last() != Some(&b'/') {
                        abs.push(b'/');
                    }
                    abs.extend_from_slice(raw_path);
                    if let Ok(path) = UnixSocketPath::new(&abs) {
                        return Ok(path);
                    }
                }
            }
            UnixSocketPath::new(raw_path)
        }
    }
}

pub(super) fn read_sockaddr_un_path<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len: u64,
) -> Result<UnixSocketPath, Errno> {
    if sockaddr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if sockaddr_len < SOCKADDR_UN_MIN_BYTES {
        return Err(Errno::EINVAL);
    }

    let copy_len = core::cmp::min(sockaddr_len, SOCKADDR_UN_MAX_BYTES) as usize;
    let mut bytes = [0u8; SOCKADDR_UN_MAX_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes[..copy_len], sockaddr_ptr)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    if family != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }

    let raw_path = &bytes[SOCKADDR_UN_MIN_BYTES as usize..copy_len];
    if raw_path.is_empty() || raw_path.len() > SOCKADDR_UN_PATH_BYTES {
        return Err(Errno::EINVAL);
    }
    if raw_path[0] == 0 {
        return UnixSocketPath::new(raw_path);
    }

    let path_len = raw_path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(raw_path.len());
    if path_len == 0 {
        return Err(Errno::EINVAL);
    }
    UnixSocketPath::new(&raw_path[..path_len])
}

pub(super) fn unix_pathname_bind_precheck<'a>(
    ctx: &SyscallCtx<'a>,
    path: &[u8],
) -> Result<(), Errno> {
    let Some(cwd) = ctx.process.cwd() else {
        return Ok(());
    };
    let cred = ctx.walker_cred();
    let guard = tx_substrate::epoch::guard();
    match step_walk(cwd, path, &cred, &guard) {
        StepOutcome::Done(_) => Err(Errno::EADDRINUSE),
        StepOutcome::Err(Errno::ENOENT) => Ok(()),
        StepOutcome::Err(errno) => Err(errno),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => Err(Errno::EIO),
    }
}

pub(super) fn read_sockaddr_nl<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len: u64,
) -> Result<(), Errno> {
    if sockaddr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if sockaddr_len < SOCKADDR_NL_BYTES as u64 {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_NL_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, sockaddr_ptr)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    if family != AF_NETLINK {
        return Err(Errno::EAFNOSUPPORT);
    }
    Ok(())
}

pub(super) fn read_sockaddr_ll<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len: u64,
) -> Result<SockAddrLl, Errno> {
    if sockaddr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    if sockaddr_len < SOCKADDR_LL_BYTES as u64 {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_LL_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, sockaddr_ptr)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    if family != AF_PACKET {
        return Err(Errno::EAFNOSUPPORT);
    }
    let protocol = u16::from_be_bytes([bytes[2], bytes[3]]);
    let ifindex = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let hatype = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    let pkttype = bytes[10];
    let halen = bytes[11];
    let mut addr = [0u8; 8];
    addr.copy_from_slice(&bytes[12..20]);
    Ok(SockAddrLl::with_link_layer_addr(
        protocol, ifindex, hatype, pkttype, addr, halen,
    ))
}

pub(super) fn read_msghdr<'a>(ctx: &SyscallCtx<'a>, msghdr_ptr: u64) -> Result<UserMsghdr, Errno> {
    if msghdr_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let mut bytes = [0u8; MSGHDR_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, msghdr_ptr)?;
    Ok(UserMsghdr {
        name: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
        namelen: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        iov: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        iovlen: u64::from_le_bytes(bytes[24..32].try_into().unwrap()),
        control: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
        controllen: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
    })
}

pub(super) fn validate_sendmsg_control<'a>(
    ctx: &SyscallCtx<'a>,
    header: UserMsghdr,
) -> Result<(), Errno> {
    if header.controllen == 0 {
        return Ok(());
    }
    if header.control == 0 {
        return Err(Errno::EFAULT);
    }
    let controllen = usize::try_from(header.controllen).map_err(|_| Errno::EINVAL)?;
    let control_end = header
        .control
        .checked_add(header.controllen)
        .ok_or(Errno::EFAULT)?;
    if control_end > tx_subsystems::vm::FULL_USER_V1_TOP as u64 {
        return Err(Errno::EFAULT);
    }
    validate_user_range(ctx, header.control, controllen, UserAccessKind::Read)?;
    if header.controllen < CMSGHDR_BYTES {
        return Err(Errno::EINVAL);
    }

    let cmsg_len: u64 = bootstrap_read_user(&ctx.aspace, header.control)?;
    let cmsg_level: i32 = bootstrap_read_user(&ctx.aspace, header.control + 8)?;
    let cmsg_type: i32 = bootstrap_read_user(&ctx.aspace, header.control + 12)?;
    if cmsg_len < CMSGHDR_BYTES || cmsg_len > header.controllen {
        return Err(Errno::EINVAL);
    }
    if cmsg_level == SOL_SOCKET && cmsg_type == SCM_RIGHTS {
        let fd_bytes = cmsg_len - CMSGHDR_BYTES;
        if fd_bytes < core::mem::size_of::<i32>() as u64 {
            return Err(Errno::EINVAL);
        }
        let fd: i32 = bootstrap_read_user(&ctx.aspace, header.control + CMSGHDR_BYTES)?;
        if fd < 0 || resolve_fd(&ctx.process, fd as u32).is_none() {
            return Err(Errno::EBADF);
        }
    }
    Ok(())
}

pub(super) fn mmsghdr_slot_ptr(msgvec: u64, index: u64) -> Result<u64, Errno> {
    if msgvec == 0 {
        return Err(Errno::EFAULT);
    }
    let offset = index.checked_mul(MMSGHDR_BYTES).ok_or(Errno::EINVAL)?;
    msgvec.checked_add(offset).ok_or(Errno::EINVAL)
}

pub(super) fn validate_mmsghdr_slot<'a>(
    ctx: &SyscallCtx<'a>,
    mmsghdr_ptr: u64,
) -> Result<(), Errno> {
    validate_user_range(
        ctx,
        mmsghdr_ptr,
        MMSGHDR_BYTES as usize,
        UserAccessKind::Read,
    )?;
    validate_user_range(
        ctx,
        mmsghdr_ptr,
        MMSGHDR_BYTES as usize,
        UserAccessKind::Write,
    )
}

pub(super) fn validate_user_range<'a>(
    ctx: &SyscallCtx<'a>,
    uaddr: u64,
    len: usize,
    access: UserAccessKind,
) -> Result<(), Errno> {
    #[cfg(not(target_os = "none"))]
    {
        let _ = (ctx, access);
        let limit = if cfg!(any(test, feature = "test-support")) {
            0x1000
        } else {
            tx_subsystems::vm::FULL_USER_V1_TOP as u64
        };
        let end = uaddr.checked_add(len as u64).ok_or(Errno::EFAULT)?;
        if uaddr < limit || end < limit {
            return Err(Errno::EFAULT);
        }
        Ok(())
    }

    #[cfg(target_os = "none")]
    {
        let range = covering_user_range(uaddr, len).ok_or(Errno::EFAULT)?;
        match ctx.aspace.reserve_user_range_for_access(range, access) {
            StepOutcome::Done(()) | StepOutcome::Continue { .. } => Ok(()),
            StepOutcome::Err(e) => Err(Errno::from(e)),
            StepOutcome::Yield { .. } => Err(Errno::EIO),
        }
    }
}

pub(super) fn write_mmsghdr_len<'a>(
    ctx: &SyscallCtx<'a>,
    mmsghdr_ptr: u64,
    msg_len: u32,
) -> Result<(), Errno> {
    bootstrap_write_user(&ctx.aspace, mmsghdr_ptr + MMSGHDR_LEN_OFFSET, msg_len)
}

pub(super) fn read_recvmmsg_timeout<'a>(
    ctx: &SyscallCtx<'a>,
    timeout_ptr: u64,
) -> Result<Option<u64>, Errno> {
    if timeout_ptr == 0 {
        return Ok(None);
    }

    let mut bytes = [0u8; 16];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, timeout_ptr)?;
    let tv_sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let tv_nsec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if tv_sec < 0 || !(0..1_000_000_000).contains(&tv_nsec) {
        return Err(Errno::EINVAL);
    }

    Ok(Some(
        (tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(tv_nsec as u64),
    ))
}

pub(super) fn read_iovecs<'a>(
    ctx: &SyscallCtx<'a>,
    iov_ptr: u64,
    iovlen: u64,
) -> Result<alloc::vec::Vec<UserIovec>, Errno> {
    if iovlen > MAX_MSG_IOV {
        return Err(Errno::EMSGSIZE);
    }
    if iovlen == 0 {
        return Ok(alloc::vec::Vec::new());
    }
    if iov_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let mut out = alloc::vec::Vec::with_capacity(iovlen as usize);
    for i in 0..iovlen {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut bytes = [0u8; IOVEC_BYTES as usize];
        bootstrap_copy_from_user(&ctx.aspace, &mut bytes, ent_ptr)?;
        out.push(UserIovec {
            base: u64::from_le_bytes(bytes[0..8].try_into().unwrap()),
            len: u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize,
        });
    }
    Ok(out)
}

pub(super) fn iov_total_len(iovecs: &[UserIovec]) -> Result<usize, i32> {
    iov_total_len_with_limit(iovecs, TTY_WRITE_MAX_INLINE)
}

pub(super) fn iov_total_len_with_limit(iovecs: &[UserIovec], limit: usize) -> Result<usize, i32> {
    let mut total = 0usize;
    for iov in iovecs {
        total = total.checked_add(iov.len).ok_or(EINVAL_VALUE)?;
        if total > limit {
            return Err(E2BIG_VALUE);
        }
    }
    Ok(total)
}

pub(super) fn scatter_to_iovecs<'a>(
    ctx: &SyscallCtx<'a>,
    iovecs: &[UserIovec],
    mut bytes: &[u8],
) -> Result<(), Errno> {
    for iov in iovecs {
        if bytes.is_empty() {
            break;
        }
        if iov.len == 0 {
            continue;
        }
        let n = core::cmp::min(iov.len, bytes.len());
        bootstrap_copy_to_user(&ctx.aspace, iov.base, &bytes[..n])?;
        bytes = &bytes[n..];
    }
    Ok(())
}

pub(super) fn write_msghdr_namelen<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    namelen: u32,
) -> Result<(), Errno> {
    bootstrap_write_user(&ctx.aspace, msghdr_ptr + MSGHDR_NAMELEN_OFFSET, namelen)
}

pub(super) fn write_msghdr_controllen<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    controllen: u64,
) -> Result<(), Errno> {
    bootstrap_write_user(
        &ctx.aspace,
        msghdr_ptr + MSGHDR_CONTROLLEN_OFFSET,
        controllen,
    )
}

pub(super) fn write_msghdr_flags<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    msg_flags: u32,
) -> Result<(), Errno> {
    bootstrap_write_user(&ctx.aspace, msghdr_ptr + MSGHDR_FLAGS_OFFSET, msg_flags)
}

pub(super) fn write_sockaddr_into_msghdr<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    header: UserMsghdr,
    endpoint: IpEndpoint,
) -> Result<(), Errno> {
    if header.name == 0 {
        return Ok(());
    }
    match endpoint.family {
        AddressFamily::Inet6 => {
            write_msghdr_namelen(ctx, msghdr_ptr, SOCKADDR_IN6_BYTES)?;
            if header.namelen < SOCKADDR_IN6_BYTES {
                return Err(Errno::EINVAL);
            }
            let bytes = sockaddr_in6_bytes(endpoint);
            bootstrap_copy_to_user(&ctx.aspace, header.name, &bytes)
        }
        _ => {
            write_msghdr_namelen(ctx, msghdr_ptr, SOCKADDR_IN_BYTES)?;
            if header.namelen < SOCKADDR_IN_BYTES {
                return Err(Errno::EINVAL);
            }
            let bytes = sockaddr_in_bytes(endpoint);
            bootstrap_copy_to_user(&ctx.aspace, header.name, &bytes)
        }
    }
}

pub(super) fn write_sockaddr_nl_into_msghdr<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    header: UserMsghdr,
) -> Result<(), Errno> {
    if header.name == 0 {
        return Ok(());
    }
    write_msghdr_namelen(ctx, msghdr_ptr, SOCKADDR_NL_BYTES)?;
    if header.namelen < SOCKADDR_NL_BYTES {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_NL_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_NETLINK.to_le_bytes());
    bootstrap_copy_to_user(&ctx.aspace, header.name, &bytes)
}

pub(super) fn write_sockaddr_ll_into_msghdr<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    header: UserMsghdr,
    sockaddr: SockAddrLl,
) -> Result<(), Errno> {
    if header.name == 0 {
        return Ok(());
    }
    write_msghdr_namelen(ctx, msghdr_ptr, SOCKADDR_LL_BYTES)?;
    if header.namelen < SOCKADDR_LL_BYTES {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_LL_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_PACKET.to_le_bytes());
    bytes[2..4].copy_from_slice(&sockaddr.protocol.to_be_bytes());
    bytes[4..8].copy_from_slice(&sockaddr.ifindex.to_le_bytes());
    bytes[8..10].copy_from_slice(&sockaddr.hatype.to_le_bytes());
    bytes[10] = sockaddr.pkttype;
    bytes[11] = sockaddr.halen;
    bytes[12..20].copy_from_slice(&sockaddr.addr);
    bootstrap_copy_to_user(&ctx.aspace, header.name, &bytes)
}

pub(super) fn write_sockaddr_un_into_msghdr<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    header: UserMsghdr,
    path: Option<UnixSocketPath>,
) -> Result<(), Errno> {
    if header.name == 0 {
        return Ok(());
    }
    let sockaddr_len = sockaddr_un_len(path);
    write_msghdr_namelen(ctx, msghdr_ptr, sockaddr_len as u32)?;
    if header.namelen < sockaddr_len as u32 {
        return Err(Errno::EINVAL);
    }
    let bytes = sockaddr_un_bytes(path);
    bootstrap_copy_to_user(&ctx.aspace, header.name, &bytes[..sockaddr_len])
}

pub(super) fn write_sockaddr_nl<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len_ptr: u64,
) -> Result<(), Errno> {
    if sockaddr_ptr == 0 && sockaddr_len_ptr == 0 {
        return Ok(());
    }
    if sockaddr_ptr == 0 || sockaddr_len_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let len: u32 = bootstrap_read_user(&ctx.aspace, sockaddr_len_ptr)?;
    bootstrap_write_user(&ctx.aspace, sockaddr_len_ptr, SOCKADDR_NL_BYTES)?;
    if invalid_socklen(len) {
        return Err(Errno::EINVAL);
    }
    if len < SOCKADDR_NL_BYTES {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_NL_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_NETLINK.to_le_bytes());
    bootstrap_copy_to_user(&ctx.aspace, sockaddr_ptr, &bytes)
}

pub(super) fn write_sockaddr_ll<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len_ptr: u64,
    sockaddr: SockAddrLl,
) -> Result<(), Errno> {
    if sockaddr_ptr == 0 && sockaddr_len_ptr == 0 {
        return Ok(());
    }
    if sockaddr_ptr == 0 || sockaddr_len_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let len: u32 = bootstrap_read_user(&ctx.aspace, sockaddr_len_ptr)?;
    bootstrap_write_user(&ctx.aspace, sockaddr_len_ptr, SOCKADDR_LL_BYTES)?;
    if invalid_socklen(len) {
        return Err(Errno::EINVAL);
    }
    if len < SOCKADDR_LL_BYTES {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_LL_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_PACKET.to_le_bytes());
    bytes[2..4].copy_from_slice(&sockaddr.protocol.to_be_bytes());
    bytes[4..8].copy_from_slice(&sockaddr.ifindex.to_le_bytes());
    bytes[8..10].copy_from_slice(&sockaddr.hatype.to_le_bytes());
    bytes[10] = sockaddr.pkttype;
    bytes[11] = sockaddr.halen;
    bytes[12..20].copy_from_slice(&sockaddr.addr);
    bootstrap_copy_to_user(&ctx.aspace, sockaddr_ptr, &bytes)
}

pub(super) fn write_sockaddr_un<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len_ptr: u64,
    path: Option<UnixSocketPath>,
) -> Result<(), Errno> {
    if sockaddr_ptr == 0 && sockaddr_len_ptr == 0 {
        return Ok(());
    }
    if sockaddr_ptr == 0 || sockaddr_len_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let sockaddr_len = sockaddr_un_len(path);
    let len: u32 = bootstrap_read_user(&ctx.aspace, sockaddr_len_ptr)?;
    bootstrap_write_user(&ctx.aspace, sockaddr_len_ptr, sockaddr_len as u32)?;
    if invalid_socklen(len) {
        return Err(Errno::EINVAL);
    }
    if len < sockaddr_len as u32 {
        return Err(Errno::EINVAL);
    }

    let bytes = sockaddr_un_bytes(path);
    bootstrap_copy_to_user(&ctx.aspace, sockaddr_ptr, &bytes[..sockaddr_len])
}

pub(super) fn sockaddr_un_len(path: Option<UnixSocketPath>) -> usize {
    SOCKADDR_UN_MIN_BYTES as usize
        + path.map_or(0, |path| {
            if path.is_abstract() {
                path.len()
            } else {
                path.len().saturating_add(1)
            }
        })
}

pub(super) fn sockaddr_un_bytes(
    path: Option<UnixSocketPath>,
) -> [u8; SOCKADDR_UN_MAX_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_UN_MAX_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_UNIX.to_le_bytes());
    if let Some(path) = path {
        let start = SOCKADDR_UN_MIN_BYTES as usize;
        let end = start + path.len();
        bytes[start..end].copy_from_slice(path.as_bytes());
    }
    bytes
}

pub(super) fn write_sockaddr_endpoint<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_ptr: u64,
    sockaddr_len_ptr: u64,
    endpoint: IpEndpoint,
) -> Result<(), Errno> {
    if sockaddr_ptr == 0 && sockaddr_len_ptr == 0 {
        return Ok(());
    }
    if sockaddr_ptr == 0 || sockaddr_len_ptr == 0 {
        return Err(Errno::EFAULT);
    }

    let out_len = if endpoint.family == AddressFamily::Inet6 {
        SOCKADDR_IN6_BYTES
    } else {
        SOCKADDR_IN_BYTES
    };
    let len: u32 = bootstrap_read_user(&ctx.aspace, sockaddr_len_ptr)?;
    bootstrap_write_user(&ctx.aspace, sockaddr_len_ptr, out_len)?;
    if invalid_socklen(len) {
        return Err(Errno::EINVAL);
    }

    if endpoint.family == AddressFamily::Inet6 {
        let bytes = sockaddr_in6_bytes(endpoint);
        let copy_len = core::cmp::min(len, out_len) as usize;
        bootstrap_copy_to_user(&ctx.aspace, sockaddr_ptr, &bytes[..copy_len])
    } else {
        let bytes = sockaddr_in_bytes(endpoint);
        let copy_len = core::cmp::min(len, out_len) as usize;
        bootstrap_copy_to_user(&ctx.aspace, sockaddr_ptr, &bytes[..copy_len])
    }
}

fn sockaddr_in_bytes(endpoint: IpEndpoint) -> [u8; SOCKADDR_IN_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_IN_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    bytes[4..8].copy_from_slice(&endpoint.addr.octets());
    bytes
}

fn sockaddr_in6_bytes(endpoint: IpEndpoint) -> [u8; SOCKADDR_IN6_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_IN6_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_INET6.to_le_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    bytes[8..24].copy_from_slice(&endpoint.addr6.octets());
    bytes
}

pub(super) fn socket_local_endpoint(socket: &Cap<SocketIdentity>) -> Result<IpEndpoint, Errno> {
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Bound { local })
        | SocketProtocol::Tcp(TcpState::Listening { local, .. })
        | SocketProtocol::Tcp(TcpState::Connecting { local, .. })
        | SocketProtocol::Tcp(TcpState::Connected { local, .. })
        | SocketProtocol::Sctp(TcpState::Bound { local })
        | SocketProtocol::Sctp(TcpState::Listening { local, .. })
        | SocketProtocol::Sctp(TcpState::Connecting { local, .. })
        | SocketProtocol::Sctp(TcpState::Connected { local, .. })
        | SocketProtocol::Rds(tx_subsystems::net::RdsState::Bound { local })
        | SocketProtocol::Udp(UdpInner::Bound { local })
        | SocketProtocol::Udp(UdpInner::Connected { local, .. }) => Ok(local),
        SocketProtocol::RawIcmp(state) if payload.family() == AddressFamily::Inet6 => Ok(
            IpEndpoint::new_v6(state.bound_local6.unwrap_or(Ipv6Address::UNSPECIFIED), 0),
        ),
        SocketProtocol::RawIcmp(state) => Ok(IpEndpoint::new(
            state.bound_local.unwrap_or(Ipv4Address::UNSPECIFIED),
            0,
        )),
        SocketProtocol::Tcp(TcpState::Init)
        | SocketProtocol::Sctp(TcpState::Init)
        | SocketProtocol::Udp(UdpInner::Unbound)
        | SocketProtocol::Rds(tx_subsystems::net::RdsState::Unbound) => {
            Ok(IpEndpoint::unspecified_for_family(payload.family(), 0))
        }
        SocketProtocol::UnixDatagram(_)
        | SocketProtocol::UnixStream(_)
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => Ok(IpEndpoint::unspecified_for_family(payload.family(), 0)),
        SocketProtocol::Tcp(TcpState::Closed)
        | SocketProtocol::Sctp(TcpState::Closed)
        | SocketProtocol::Udp(UdpInner::Closed)
        | SocketProtocol::Rds(tx_subsystems::net::RdsState::Closed) => Err(Errno::ENOTCONN),
    }
}

pub(super) fn socket_unix_local_path(
    socket: &Cap<SocketIdentity>,
) -> Result<Option<UnixSocketPath>, Errno> {
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    match payload.protocol_snapshot() {
        SocketProtocol::UnixDatagram(UnixDatagramState::Bound { local }) => Ok(Some(local)),
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { local, .. }) => Ok(local),
        SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { .. }) => Ok(None),
        SocketProtocol::UnixDatagram(UnixDatagramState::Unbound) => Ok(None),
        SocketProtocol::UnixStream(UnixStreamState::Bound { local })
        | SocketProtocol::UnixStream(UnixStreamState::Listening { local, .. }) => Ok(Some(local)),
        SocketProtocol::UnixStream(UnixStreamState::Connected { local, .. }) => Ok(local),
        SocketProtocol::UnixStream(UnixStreamState::Init) => Ok(None),
        _ => Err(Errno::ENOTSOCK),
    }
}

pub(super) fn socket_peer_endpoint(socket: &Cap<SocketIdentity>) -> Result<IpEndpoint, Errno> {
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connecting { remote, .. })
        | SocketProtocol::Tcp(TcpState::Connected { remote, .. })
        | SocketProtocol::Sctp(TcpState::Connecting { remote, .. })
        | SocketProtocol::Sctp(TcpState::Connected { remote, .. })
        | SocketProtocol::Udp(UdpInner::Connected { remote, .. }) => Ok(remote),
        _ => Err(Errno::ENOTCONN),
    }
}

pub(super) fn socket_unix_peer_path(
    socket: &Cap<SocketIdentity>,
) -> Result<Option<UnixSocketPath>, Errno> {
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    match payload.protocol_snapshot() {
        SocketProtocol::UnixDatagram(UnixDatagramState::Connected { peer, .. }) => Ok(Some(peer)),
        SocketProtocol::UnixDatagram(UnixDatagramState::ConnectedPair { .. }) => Ok(None),
        SocketProtocol::UnixStream(UnixStreamState::Connected { .. }) => Ok(None),
        SocketProtocol::UnixDatagram(_)
        | SocketProtocol::UnixStream(UnixStreamState::Init | UnixStreamState::Bound { .. })
        | SocketProtocol::UnixStream(UnixStreamState::Listening { .. }) => Err(Errno::ENOTCONN),
        _ => Err(Errno::ENOTSOCK),
    }
}

pub(super) fn tcp_sendto_ignores_destination(socket: &Cap<SocketIdentity>) -> bool {
    if !matches!(socket.kind, SocketKind::Tcp | SocketKind::Sctp) {
        return false;
    }
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connected { .. })
                | SocketProtocol::Sctp(TcpState::Connected { .. })
        )
    })
}

pub(super) fn socket_is_tcp_connecting(socket: &Cap<SocketIdentity>) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connecting { .. })
        )
    })
}

/// True when a recv with no buffered data on this SCTP socket must report
/// ENOTCONN instead of blocking: the association is not established (never
/// connected, or locally shut down via SHUT_WR). Mirrors the step_recv check;
/// needed because recvfrom()/recv() block in a poll-wait loop before reaching
/// the recv step.
pub(super) fn sctp_recv_disconnected(socket: &Cap<SocketIdentity>) -> bool {
    let Some(payload) = socket.acquire_operational() else {
        return true;
    };
    let seqpacket = payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket);
    match payload.protocol_snapshot() {
        SocketProtocol::Sctp(TcpState::Connected { .. }) => payload.shutdown_wr(),
        // 1-to-many: listening or associated can still receive (block); neither
        // listening nor associated reports ENOTCONN.
        SocketProtocol::Sctp(TcpState::Listening { .. }) if seqpacket => false,
        SocketProtocol::Sctp(_) if seqpacket => payload.sctp_assoc_count() == 0,
        SocketProtocol::Sctp(_) => true,
        _ => false,
    }
}

pub(super) fn validate_recvfrom_addrlen<'a>(
    ctx: &SyscallCtx<'a>,
    sockaddr_len_ptr: u64,
) -> Result<(), Errno> {
    if sockaddr_len_ptr == 0 {
        return Ok(());
    }
    let len: u32 = bootstrap_read_user(&ctx.aspace, sockaddr_len_ptr)?;
    if invalid_socklen(len) {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

pub(super) fn read_sockopt_bool<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<bool, Errno> {
    Ok(read_sockopt_i32(ctx, optval, optlen)? != 0)
}

pub(super) fn read_sockopt_positive_usize<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<usize, Errno> {
    let value = read_sockopt_i32(ctx, optval, optlen)?;
    if value <= 0 {
        return Err(Errno::EINVAL);
    }
    Ok(value as usize)
}

pub(super) fn read_sockopt_i32<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<i32, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < core::mem::size_of::<i32>() as u32 {
        return Err(Errno::EINVAL);
    }
    bootstrap_read_user(&ctx.aspace, optval)
}

/// Read an `int`-or-`unsigned char` socket option value. `IP_MULTICAST_TTL` and
/// `IP_MULTICAST_LOOP` accept either a 4-byte `int` or a 1-byte `char`; Linux
/// reads a `char` when `optlen < sizeof(int)`.
pub(super) fn read_sockopt_byte_or_i32<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<i32, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen >= core::mem::size_of::<i32>() as u32 {
        bootstrap_read_user(&ctx.aspace, optval)
    } else if optlen >= 1 {
        let byte: u8 = bootstrap_read_user(&ctx.aspace, optval)?;
        Ok(byte as i32)
    } else {
        Err(Errno::EINVAL)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PacketRxRingReq {
    pub block_size: u32,
    pub block_nr: u32,
    pub frame_size: u32,
    pub frame_nr: u32,
    pub sizeof_priv: u32,
}

pub(super) fn read_packet_rx_ring_req<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<PacketRxRingReq, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < 16 {
        return Err(Errno::EINVAL);
    }

    let copy_len = core::cmp::min(optlen as usize, 28);
    let mut bytes = [0u8; 28];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes[..copy_len], optval)?;
    Ok(PacketRxRingReq {
        block_size: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        block_nr: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        frame_size: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        frame_nr: u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        sizeof_priv: if optlen >= 24 {
            u32::from_le_bytes(bytes[20..24].try_into().unwrap())
        } else {
            0
        },
    })
}

pub(super) fn validate_packet_rx_ring_req(req: PacketRxRingReq) -> Result<(), Errno> {
    if req.block_nr == 0 && req.frame_nr == 0 {
        return Ok(());
    }
    if req.block_size == 0 || req.block_nr == 0 || req.frame_size == 0 || req.frame_nr == 0 {
        return Err(Errno::EINVAL);
    }
    if req.frame_size > req.block_size || req.sizeof_priv >= req.block_size {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

pub(super) fn read_sockopt_ipv4_mcast_group_req<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<Ipv4MulticastGroup, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < GROUP_REQ_BYTES {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0u8; GROUP_REQ_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, optval)?;
    let interface = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let group = &bytes[GROUP_REQ_GROUP_OFFSET..];
    let family = u16::from_le_bytes(group[0..2].try_into().unwrap());
    if family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    let group = Ipv4Address::new([group[4], group[5], group[6], group[7]]);
    if !group.is_multicast() {
        return Err(Errno::EINVAL);
    }
    Ok(Ipv4MulticastGroup::new(interface, group))
}

/// `struct ip_mreq { struct in_addr imr_multiaddr; struct in_addr
/// imr_interface; }` for the classic `IP_ADD_MEMBERSHIP` /
/// `IP_DROP_MEMBERSHIP`. The interface here is an IPv4 address (0 =
/// any), not an ifindex; we key membership by group only, so just
/// validate the group is multicast.
pub(super) fn read_sockopt_ipv4_ip_mreq<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<Ipv4MulticastGroup, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    // Linux accepts ip_mreq (8) or ip_mreqn (12); only the first 8 bytes
    // (multiaddr + interface addr) matter for membership keying.
    if optlen < 8 {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0u8; 8];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, optval)?;
    let group = Ipv4Address::new([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if !group.is_multicast() {
        return Err(Errno::EINVAL);
    }
    Ok(Ipv4MulticastGroup::new(0, group))
}

pub(super) fn read_sockopt_linger<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<LingerOption, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < 8 {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0u8; 8];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, optval)?;
    let enabled = i32::from_le_bytes(bytes[0..4].try_into().unwrap()) != 0;
    let timeout = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if timeout < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(LingerOption {
        enabled,
        timeout_secs: timeout as u32,
    })
}

pub(super) fn read_sockopt_timeval<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<Option<core::time::Duration>, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < 16 {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0u8; 16];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, optval)?;
    let sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let usec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if sec < 0 || !(0..1_000_000).contains(&usec) {
        return Err(Errno::EINVAL);
    }
    if sec == 0 && usec == 0 {
        return Ok(None);
    }
    Ok(Some(core::time::Duration::new(
        sec as u64,
        (usec as u32) * 1_000,
    )))
}

pub(super) fn write_sockopt_i32<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
    value: i32,
) -> Result<(), Errno> {
    if optval == 0 || optlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let optlen: u32 = bootstrap_read_user(&ctx.aspace, optlen_ptr)?;
    bootstrap_write_user(&ctx.aspace, optlen_ptr, core::mem::size_of::<i32>() as u32)?;
    if invalid_socklen(optlen) {
        return Err(Errno::EINVAL);
    }
    if optlen < core::mem::size_of::<i32>() as u32 {
        return Err(Errno::EINVAL);
    }
    bootstrap_write_user(&ctx.aspace, optval, value)
}

pub(super) fn write_sockopt_linger<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
    value: LingerOption,
) -> Result<(), Errno> {
    if optval == 0 || optlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let optlen: u32 = bootstrap_read_user(&ctx.aspace, optlen_ptr)?;
    bootstrap_write_user(&ctx.aspace, optlen_ptr, 8u32)?;
    if invalid_socklen(optlen) {
        return Err(Errno::EINVAL);
    }
    if optlen < 8 {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0u8; 8];
    bytes[0..4].copy_from_slice(&(value.enabled as i32).to_le_bytes());
    bytes[4..8].copy_from_slice(&(value.timeout_secs as i32).to_le_bytes());
    bootstrap_copy_to_user(&ctx.aspace, optval, &bytes)
}

pub(super) fn write_sockopt_timeval<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
    value: Option<core::time::Duration>,
) -> Result<(), Errno> {
    if optval == 0 || optlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let optlen: u32 = bootstrap_read_user(&ctx.aspace, optlen_ptr)?;
    bootstrap_write_user(&ctx.aspace, optlen_ptr, 16u32)?;
    if invalid_socklen(optlen) {
        return Err(Errno::EINVAL);
    }
    if optlen < 16 {
        return Err(Errno::EINVAL);
    }
    let (sec, usec) = match value {
        Some(duration) => (duration.as_secs() as i64, (duration.subsec_micros()) as i64),
        None => (0, 0),
    };
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&sec.to_le_bytes());
    bytes[8..16].copy_from_slice(&usec.to_le_bytes());
    bootstrap_copy_to_user(&ctx.aspace, optval, &bytes)
}

pub(super) fn write_sockopt_bytes<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
    value: &[u8],
) -> Result<(), Errno> {
    if optval == 0 || optlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let optlen: u32 = bootstrap_read_user(&ctx.aspace, optlen_ptr)?;
    if invalid_socklen(optlen) {
        return Err(Errno::EINVAL);
    }
    let bytes = core::cmp::min(optlen as usize, value.len());
    bootstrap_write_user(&ctx.aspace, optlen_ptr, bytes as u32)?;
    for (offset, byte) in value[..bytes].iter().copied().enumerate() {
        bootstrap_write_user(&ctx.aspace, optval + offset as u64, byte)?;
    }
    Ok(())
}

pub(super) fn validate_getsockopt_i32_args<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
) -> Result<(), Errno> {
    if optval == 0 || optlen_ptr == 0 {
        return Err(Errno::EFAULT);
    }
    let optlen: u32 = bootstrap_read_user(&ctx.aspace, optlen_ptr)?;
    if invalid_socklen(optlen) {
        return Err(Errno::EINVAL);
    }
    if optlen < core::mem::size_of::<i32>() as u32 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

pub(super) fn invalid_socklen(len: u32) -> bool {
    len > i32::MAX as u32
}

pub(super) fn socket_type_i32(socket: &Cap<SocketIdentity>) -> i32 {
    if let Some(payload) = socket.acquire_operational() {
        return match payload.with_options(|options| options.socket.sock_type) {
            SocketType::Stream => 1,
            SocketType::Dgram => 2,
            SocketType::Raw => 3,
            SocketType::SeqPacket => 5,
        };
    }

    match socket.kind {
        SocketKind::UnixStream => 1,
        SocketKind::UnixDatagram => 2,
        SocketKind::Tcp => 1,
        SocketKind::Sctp => 1,
        SocketKind::Udp => 2,
        SocketKind::RdsSeqPacket => 5,
        SocketKind::RawIcmp => 3,
        SocketKind::NetlinkRoute
        | SocketKind::NetlinkXfrm
        | SocketKind::NetlinkNetfilter
        | SocketKind::Packet => 3,
    }
}

pub(super) fn step_unit_result(outcome: StepOutcome<(), NoProgress>) -> SyscallResult {
    match outcome {
        StepOutcome::Done(()) | StepOutcome::Continue { .. } => SyscallResult::Return(0),
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        StepOutcome::Yield { .. } => SyscallResult::Error(EIO_VALUE),
    }
}

pub(super) fn wait_on_yield_shape(shape: YieldShape) -> Option<wait_source::RegisteredWaitFuture> {
    match shape {
        YieldShape::OnWaitSource { source, interests }
        | YieldShape::OnEdge { source, interests } => {
            wait_source::wait_on_registered_source_id(source.raw(), interests.raw())
        }
        YieldShape::OnAgent { .. } | YieldShape::OnTimer { .. } => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SocketWaitWake {
    SocketReady,
    ItimerExpired,
}

pub(super) async fn wait_on_socket_or_itimer<P>(
    mut socket_future: wait_source::RegisteredWaitFuture,
    ctx: &SyscallCtx<'_>,
) -> SocketWaitWake
where
    TimekeeperClock<P>: ClockRead,
{
    let pid = ctx.process.pid.0;
    // Linux never parks a task in a slow syscall while a signal that would be
    // delivered is already pending — the syscall aborts with EINTR and the AST
    // checkpoint delivers on return. Without this, a one-shot ITIMER_REAL
    // consumed at a syscall boundary leaves busybox ping's blocking recvfrom
    // with no armed deadline AND a pending SIGALRM: it parked until unrelated
    // traffic woke the socket (observed as a 361s stall in if-updown).
    if tx_subsystems::signal::pending_signal_interrupts_wait(&ctx.thread, &ctx.process) {
        return SocketWaitWake::ItimerExpired;
    }
    if super::time::consume_itimer_real_delivered_interrupt(pid) {
        return SocketWaitWake::ItimerExpired;
    }
    let Some(deadline_ns) = super::time::itimer_real_deadline_ns(pid) else {
        let _ = socket_future.await;
        return SocketWaitWake::SocketReady;
    };
    if timekeeper_clock::<P>().monotonic_now_ns() >= deadline_ns {
        super::time::fire_itimer_real_with_post::<P>(ctx);
        return SocketWaitWake::ItimerExpired;
    }
    let Some(mut timer_future) = super::deadline_timer(ctx, deadline_ns) else {
        let _ = socket_future.await;
        return SocketWaitWake::SocketReady;
    };

    let wake = core::future::poll_fn(|cx| {
        if core::future::Future::poll(core::pin::Pin::new(&mut socket_future), cx).is_ready() {
            return core::task::Poll::Ready(SocketWaitWake::SocketReady);
        }
        if core::future::Future::poll(core::pin::Pin::new(&mut timer_future), cx).is_ready() {
            return core::task::Poll::Ready(SocketWaitWake::ItimerExpired);
        }
        core::task::Poll::Pending
    })
    .await;
    if wake == SocketWaitWake::ItimerExpired {
        super::time::fire_itimer_real_with_post::<P>(ctx);
    }
    wake
}

pub(super) fn recv_special_flags_errno(flags: SendRecvFlags) -> Option<i32> {
    if flags.contains(SendRecvFlags::MSG_OOB) {
        Some(EINVAL_VALUE)
    } else if flags.contains(SendRecvFlags::MSG_ERRQUEUE) {
        Some(EAGAIN_VALUE)
    } else {
        None
    }
}

pub(super) fn maybe_raise_sigpipe<'a>(ctx: &SyscallCtx<'a>, errno: Errno, flags: SendRecvFlags) {
    if errno == Errno::EPIPE && !flags.contains(SendRecvFlags::MSG_NOSIGNAL) {
        let _ = tx_subsystems::signal::step_kill_process_with_post(
            &ctx.process,
            Signum::SIGPIPE,
            None,
            |mailbox, event| ctx.post_mailbox_event(mailbox, event),
        );
    }
}
