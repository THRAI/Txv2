//! Socket syscall shims for the N39 fdtable integration slice.
//!
//! The syscall layer owns Linux ABI decoding and fdtable installation.
//! Socket state transitions stay in `tx_subsystems::net::execution`
//! steps so the identity/payload split and wait-carrier discipline stay
//! in the network subsystem.

use super::*;

use tx_substrate::step::{NoProgress, StepOutcome, YieldShape};
use tx_subsystems::net::protocol::loopback_iface;
use tx_subsystems::net::{
    net_namespace_payload_from_file, netlink_netfilter_recv, netlink_netfilter_send,
    netlink_route_recv, netlink_route_send_with_netns_resolvers, require_net_raw,
    socket_open_file_from_identity, step_accept, step_bind, step_connect, step_listen,
    step_poll_ready, step_poll_wait_token, step_process_loopback_udp, step_recv_kernel_bytes,
    step_send_to_kernel_bytes, step_send_to_unix_path_kernel_bytes,
    step_send_udp_loopback_kernel_bytes, step_shutdown, step_socket_close,
    step_socket_open_file_in_namespace, step_tcp_loopback_handshake, step_tcp_loopback_transfer,
    AddressFamily, ConnectionKey, IpEndpoint, Ipv4Address, Ipv4MulticastGroup, KernelSockAddr,
    LingerOption, PollMask, SendRecvFlags, SockAddrIn, SockAddrLl, SockShutdownCmd,
    SocketHandleFlags, SocketIdentity, SocketKind, SocketProtocol, SocketType, TcpState, UdpInner,
    UnixDatagramState, UnixSocketPath, UnixStreamState, ValidSocketType, VIRTIO_NET_DEFAULT_MTU,
};
use tx_subsystems::signal::step_kill_process;
use tx_subsystems::vfs::structure::OpenFileBacking;
use tx_subsystems::vm::UserAccessKind;
use tx_subsystems::wait_source;

const SOCKADDR_IN_BYTES: u32 = 16;
const SOCKADDR_UN_MIN_BYTES: u64 = 2;
const SOCKADDR_UN_MAX_BYTES: u64 = 110;
const SOCKADDR_UN_PATH_BYTES: usize = 108;
const SOCKADDR_NL_BYTES: u32 = 12;
const SOCKADDR_LL_BYTES: u32 = 20;
const ACCEPT4_KNOWN_FLAGS: u32 = O_CLOEXEC | O_NONBLOCK;
const EPHEMERAL_PORT_START: u16 = 49_152;
const EPHEMERAL_PORT_END: u16 = 49_216;
const IOVEC_BYTES: u64 = 16;
const MSGHDR_BYTES: u64 = 56;
const MSGHDR_NAMELEN_OFFSET: u64 = 8;
const MSGHDR_CONTROLLEN_OFFSET: u64 = 40;
const MSGHDR_FLAGS_OFFSET: u64 = 48;
const MMSGHDR_BYTES: u64 = 64;
const MMSGHDR_LEN_OFFSET: u64 = MSGHDR_BYTES;
const CMSGHDR_BYTES: u64 = 16;
const SCM_RIGHTS: i32 = 1;
const MAX_MSG_IOV: u64 = 1024;
const SOCKET_MSG_MAX_BYTES: usize = 1024 * 1024;
const NETLINK_RECVMSG_MAX: usize = 1024 * 1024;
const IPT_GETINFO_BYTES: usize = 84;
const IPT_GET_ENTRIES_EMPTY_BYTES: usize = 36;
const GROUP_REQ_BYTES: u32 = 136;
const GROUP_REQ_GROUP_OFFSET: usize = 8;
const IPV4_TCP_HEADER_BYTES: u16 = 40;

#[derive(Clone, Copy)]
struct UserIovec {
    base: u64,
    len: usize,
}

#[derive(Clone, Copy)]
struct UserMsghdr {
    name: u64,
    namelen: u32,
    iov: u64,
    iovlen: u64,
    control: u64,
    controllen: u64,
}

pub(super) fn sys_socket<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let domain = args[0] as i32;
    let type_ = args[1] as i32;
    let protocol = args[2] as i32;
    let valid = match ValidSocketType::validate(domain, type_, protocol) {
        Ok(valid) => valid,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let kind = match SocketKind::from_valid_socket_type(valid) {
        Ok(kind) => kind,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if socket_requires_net_raw(kind, valid) {
        if let Err(errno) = require_net_raw(ctx.cred()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    let outcome = {
        let guard = tx_substrate::epoch::guard();
        let Some(net_namespace) = ctx.process.net_namespace() else {
            return SyscallResult::Error(ESRCH_VALUE);
        };
        step_socket_open_file_in_namespace(domain, type_, protocol, net_namespace, &guard)
    };

    let opened = match outcome {
        StepOutcome::Done(opened) => opened,
        StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
            return SyscallResult::Error(EIO_VALUE);
        }
    };

    let fd = ctx.process.allocate_fd();
    let _ = ctx.process.set_fd(fd, Some(opened.file));
    ctx.process.set_fd_cloexec(fd, opened.cloexec);
    SyscallResult::Return(fd as i64)
}

fn socket_requires_net_raw(kind: SocketKind, valid: ValidSocketType) -> bool {
    kind == SocketKind::Packet
        || (kind == SocketKind::RawIcmp
            && matches!(
                (valid.domain, valid.sock_type),
                (AddressFamily::Inet, SocketType::Raw)
            ))
}

pub(super) fn sys_socketpair<'a>(args: [u64; 6], _ctx: &SyscallCtx<'a>) -> SyscallResult {
    let domain = args[0] as i32;
    if domain != AF_INET as i32 {
        return SyscallResult::Error(errno_to_i32(Errno::EAFNOSUPPORT));
    }
    SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP))
}

pub(super) fn sys_bind<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    ) {
        return match read_sockaddr_nl(ctx, args[1], args[2]) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }
    if socket.kind == SocketKind::Packet {
        let sockaddr = match read_sockaddr_ll(ctx, args[1], args[2]) {
            Ok(sockaddr) => sockaddr,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let Some(payload) = socket.acquire_operational() else {
            return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN));
        };
        return match payload.bind_packet_socket(sockaddr) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }
    if socket.kind == SocketKind::UnixDatagram || socket.kind == SocketKind::UnixStream {
        let path = match read_sockaddr_un_path(ctx, args[1], args[2]) {
            Ok(path) => path,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        if let Err(errno) = unix_pathname_bind_precheck(ctx, path.as_bytes()) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_bind(&socket, KernelSockAddr::Unix(path), &guard)
        };
        return step_unit_result(outcome);
    }
    let addr = match read_sockaddr_in(ctx, args[1], args[2]) {
        Ok(addr) => addr,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    bind_with_ephemeral_port(&socket, addr)
}

pub(super) fn sys_listen<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let backlog = if (args[1] as i64) < 0 {
        0
    } else {
        args[1] as usize
    };

    let outcome = {
        let guard = tx_substrate::epoch::guard();
        step_listen(&socket, backlog, &guard)
    };
    step_unit_result(outcome)
}

pub(super) async fn sys_accept<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    sys_accept_impl::<P>(args[0] as i32, args[1], args[2], 0, ctx).await
}

pub(super) async fn sys_accept4<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let flags = args[3] as u32;
    if flags & !ACCEPT4_KNOWN_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    sys_accept_impl::<P>(args[0] as i32, args[1], args[2], flags, ctx).await
}

async fn sys_accept_impl<'a, P: TimeIf>(
    fd: i32,
    addr_ptr: u64,
    addrlen_ptr: u64,
    flags: u32,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let (file, listener) = match resolve_socket_fd(ctx, fd) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let nonblocking = file.flags().nonblocking;

    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_accept(&listener, &guard)
        };
        match outcome {
            StepOutcome::Done(accepted) => {
                let handle_flags = SocketHandleFlags {
                    nonblock: flags & O_NONBLOCK != 0,
                    cloexec: flags & O_CLOEXEC != 0,
                };
                let opened = match socket_open_file_from_identity(accepted.child, handle_flags) {
                    Ok(opened) => opened,
                    Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                };
                let write_addr = if listener.kind == SocketKind::UnixStream {
                    write_sockaddr_un(ctx, addr_ptr, addrlen_ptr, accepted.unix_peer)
                } else {
                    write_sockaddr_endpoint(ctx, addr_ptr, addrlen_ptr, accepted.peer)
                };
                if let Err(errno) = write_addr {
                    return SyscallResult::Error(errno_to_i32(errno));
                }

                let new_fd = ctx.process.allocate_fd();
                let _ = ctx.process.set_fd(new_fd, Some(opened.file));
                ctx.process.set_fd_cloexec(new_fd, opened.cloexec);
                return SyscallResult::Return(new_fd as i64);
            }
            StepOutcome::Yield { shape, .. } => {
                if nonblocking {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                if let Some(future) = wait_on_yield_shape(shape) {
                    if matches!(
                        wait_on_socket_or_itimer::<P>(future, ctx.process.pid.0).await,
                        SocketWaitWake::ItimerExpired
                    ) {
                        return SyscallResult::Error(EINTR_VALUE);
                    }
                } else {
                    return SyscallResult::Error(EIO_VALUE);
                }
            }
            StepOutcome::Continue { .. } => {}
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    }
}

pub(super) async fn sys_connect<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let (file, socket) = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let remote = if matches!(
        socket.kind,
        SocketKind::UnixDatagram | SocketKind::UnixStream
    ) {
        match read_sockaddr_un_path(ctx, args[1], args[2]) {
            Ok(path) => KernelSockAddr::Unix(path),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        match read_sockaddr_in(ctx, args[1], args[2]) {
            Ok(remote) => remote,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };
    let remote = connect_sockaddr_for_local_stack(socket.kind, remote);
    if let Err(errno) = maybe_autobind_connect_client(&socket, remote) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let nonblocking = file.flags().nonblocking;
    let was_connecting = socket_is_tcp_connecting(&socket);
    let mut waited_for_connect = was_connecting;

    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_connect(&socket, remote, &guard)
        };
        match outcome {
            StepOutcome::Done(()) => return SyscallResult::Return(0),
            StepOutcome::Yield { shape, .. } => {
                let connected = match drive_tcp_loopback_after_connect(&socket) {
                    Ok(connected) => connected || !socket_is_tcp_connecting(&socket),
                    Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                };
                if nonblocking {
                    let errno = if was_connecting {
                        Errno::EALREADY
                    } else {
                        Errno::EINPROGRESS
                    };
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                if connected {
                    return SyscallResult::Return(0);
                }
                if let Some(future) = wait_on_yield_shape(shape) {
                    waited_for_connect = true;
                    let _ = future.await;
                } else {
                    return SyscallResult::Error(EIO_VALUE);
                }
            }
            StepOutcome::Continue { .. } => {}
            StepOutcome::Err(Errno::EISCONN) if waited_for_connect => {
                return SyscallResult::Return(0);
            }
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    }
}

fn connect_sockaddr_for_local_stack(kind: SocketKind, remote: KernelSockAddr) -> KernelSockAddr {
    if kind != SocketKind::Tcp && kind != SocketKind::Udp {
        return remote;
    }
    match remote {
        KernelSockAddr::V4(sockaddr) if sockaddr.addr == Ipv4Address::UNSPECIFIED => {
            KernelSockAddr::V4(SockAddrIn::new(sockaddr.port, Ipv4Address::LOOPBACK))
        }
        _ => remote,
    }
}

fn maybe_autobind_connect_client(
    socket: &Cap<SocketIdentity>,
    remote: KernelSockAddr,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::Tcp && socket.kind != SocketKind::Udp {
        return Ok(());
    }

    let remote_endpoint = remote.as_ip_endpoint();
    let local_addr = if remote_endpoint.addr == Ipv4Address::LOOPBACK
        || remote_endpoint.addr == Ipv4Address::UNSPECIFIED
    {
        Ipv4Address::LOOPBACK
    } else {
        Ipv4Address::UNSPECIFIED
    };

    for port in EPHEMERAL_PORT_START..EPHEMERAL_PORT_END {
        let local_endpoint = IpEndpoint::new(local_addr, port);
        if socket.kind == SocketKind::Tcp
            && tcp_connect_tuple_in_use(socket, local_endpoint, remote_endpoint)
        {
            continue;
        }
        let local = KernelSockAddr::V4(SockAddrIn::new(port, local_addr));
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

fn tcp_connect_tuple_in_use(
    socket: &Cap<SocketIdentity>,
    local: IpEndpoint,
    remote: IpEndpoint,
) -> bool {
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    let table = payload.socket_table();
    let guard = tx_substrate::epoch::guard();
    table
        .lookup_tcp_connection(ConnectionKey::new(local, remote), &guard)
        .is_some()
        || table
            .lookup_tcp_connection(ConnectionKey::new(remote, local), &guard)
            .is_some()
}

fn maybe_autobind_udp_sendto(
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

    let local_addr = if dst.is_some_and(|endpoint| endpoint.addr == Ipv4Address::LOOPBACK) {
        Ipv4Address::LOOPBACK
    } else {
        Ipv4Address::UNSPECIFIED
    };

    for port in EPHEMERAL_PORT_START..EPHEMERAL_PORT_END {
        let local = KernelSockAddr::V4(SockAddrIn::new(port, local_addr));
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

fn drive_tcp_loopback_after_connect(socket: &Cap<SocketIdentity>) -> Result<bool, Errno> {
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

fn bind_with_ephemeral_port(socket: &Cap<SocketIdentity>, addr: KernelSockAddr) -> SyscallResult {
    let requested = addr.as_ip_endpoint();
    if requested.port != 0 {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_bind(socket, addr, &guard)
        };
        return step_unit_result(outcome);
    }

    for port in EPHEMERAL_PORT_START..EPHEMERAL_PORT_END {
        if ephemeral_port_in_use(socket, port) {
            continue;
        }
        let local = KernelSockAddr::V4(SockAddrIn::new(port, requested.addr));
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

fn ephemeral_port_in_use(socket: &Cap<SocketIdentity>, port: u16) -> bool {
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    let table = payload.socket_table();
    let guard = tx_substrate::epoch::guard();

    match socket.kind {
        SocketKind::Tcp => table
            .snapshot_tcp_bound(&guard)
            .into_iter()
            .chain(table.snapshot_tcp_listeners(&guard))
            .any(|existing| {
                existing.raw() != socket.raw()
                    && socket_local_endpoint(&existing).is_ok_and(|local| local.port == port)
            }),
        SocketKind::Udp => table
            .snapshot_udp_bound(&guard)
            .into_iter()
            .any(|existing| {
                existing.raw() != socket.raw()
                    && socket_local_endpoint(&existing).is_ok_and(|local| local.port == port)
            }),
        _ => false,
    }
}

fn drive_tcp_loopback_after_sendto(socket: &Cap<SocketIdentity>, written: usize) {
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

fn drive_udp_loopback_after_sendto(socket: &Cap<SocketIdentity>, written: usize) -> bool {
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

fn drive_loopback_after_sendto(socket: &Cap<SocketIdentity>, written: usize) -> bool {
    drive_tcp_loopback_after_sendto(socket, written);
    drive_udp_loopback_after_sendto(socket, written)
}

async fn yield_after_sendto_if_needed(socket: &Cap<SocketIdentity>) {
    if socket.kind != SocketKind::Udp {
        tx_reactor::yield_now().await;
    }
}

async fn finish_sendto_progress(
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

fn drive_loopback_pending() {
    let guard = tx_substrate::epoch::guard();
    let _ = tx_subsystems::net::execution::step_process_loopback_pending_zero(
        tx_subsystems::net::protocol::loopback_iface(),
        tx_subsystems::net::execution::LOOPBACK_POLL_BUDGET_DEFAULT,
        &guard,
    );
}

fn recv_ready_mask(mask: PollMask) -> bool {
    mask.intersects(PollMask::IN | PollMask::ERR | PollMask::HUP | PollMask::RDHUP)
}

fn recv_staging_len(socket: &Cap<SocketIdentity>, requested: usize) -> usize {
    let capped = requested.min(TTY_WRITE_MAX_INLINE);
    let ready = recv_queued_len(socket);
    if ready == 0 {
        capped
    } else {
        capped.min(ready)
    }
}

fn recv_queued_len(socket: &Cap<SocketIdentity>) -> usize {
    let Some(payload) = socket.acquire_operational() else {
        return 0;
    };
    payload.io_snapshot().recv_len
}

fn socket_recv_should_yield_after_success(socket: &Cap<SocketIdentity>, bytes: usize) -> bool {
    bytes > 0 && socket.kind == SocketKind::Tcp
}

fn sendto_can_drive_loopback_inline(socket: &Cap<SocketIdentity>, dst: Option<IpEndpoint>) -> bool {
    if socket.kind != SocketKind::Udp {
        return false;
    }
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    match payload.protocol_snapshot() {
        SocketProtocol::Udp(UdpInner::Bound { local }) => dst.is_some_and(|dst| {
            local_allows_loopback_inline(local) && dst.addr == Ipv4Address::LOOPBACK
        }),
        SocketProtocol::Udp(UdpInner::Connected { local, remote }) => {
            local_allows_loopback_inline(local) && remote.addr == Ipv4Address::LOOPBACK
        }
        _ => false,
    }
}

fn local_allows_loopback_inline(local: IpEndpoint) -> bool {
    local.addr == Ipv4Address::UNSPECIFIED || local.addr == Ipv4Address::LOOPBACK
}

fn tcp_effective_maxseg(socket: &Cap<SocketIdentity>) -> i32 {
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

fn tcp_route_maxseg(socket: &Cap<SocketIdentity>) -> u16 {
    let mtu = match socket_peer_endpoint(socket) {
        Ok(peer) if peer.addr == Ipv4Address::LOOPBACK => loopback_iface().mtu(),
        _ => match socket_local_endpoint(socket) {
            Ok(local) if local.addr == Ipv4Address::LOOPBACK => loopback_iface().mtu(),
            _ => VIRTIO_NET_DEFAULT_MTU,
        },
    };
    mtu.saturating_sub(IPV4_TCP_HEADER_BYTES).max(1)
}

pub(super) fn sys_getsockname<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    ) {
        return match write_sockaddr_nl(ctx, args[1], args[2]) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }
    if socket.kind == SocketKind::Packet {
        let Some(payload) = socket.acquire_operational() else {
            return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN));
        };
        let Some(sockaddr) = payload.packet_sockaddr() else {
            return SyscallResult::Error(EINVAL_VALUE);
        };
        return match write_sockaddr_ll(ctx, args[1], args[2], sockaddr) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }
    let endpoint = match socket_local_endpoint(&socket) {
        Ok(endpoint) => endpoint,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    match write_sockaddr_endpoint(ctx, args[1], args[2], endpoint) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) fn sys_getpeername<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let endpoint = match socket_peer_endpoint(&socket) {
        Ok(endpoint) => endpoint,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    match write_sockaddr_endpoint(ctx, args[1], args[2], endpoint) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) async fn sys_sendto<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let (file, socket) = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let len = args[2] as usize;

    let mut flags = match SendRecvFlags::validate(args[3] as i32) {
        Ok(flags) => flags,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if file.flags().nonblocking {
        flags |= SendRecvFlags::MSG_DONTWAIT;
    }

    if matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    ) {
        if args[4] != 0 {
            if let Err(errno) = read_sockaddr_nl(ctx, args[4], args[5]) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        let mut bytes = alloc::vec![0; len];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, args[1]) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let mut resolve_netns_fd = |fd: i32| {
            if fd < 0 {
                return None;
            }
            let file = resolve_fd(&ctx.process, fd as u32)?;
            net_namespace_payload_from_file(&file)
        };
        let mut resolve_netns_pid = |pid: u32| {
            let process = process_by_pid(Pid(pid))?;
            process.net_namespace()
        };
        let result = match socket.kind {
            SocketKind::NetlinkRoute => netlink_route_send_with_netns_resolvers(
                &socket,
                &bytes,
                ctx.cred(),
                &mut resolve_netns_fd,
                &mut resolve_netns_pid,
            ),
            SocketKind::NetlinkNetfilter => netlink_netfilter_send(&socket, &bytes, ctx.cred()),
            _ => unreachable!(),
        };
        return match result {
            Ok(sent) => SyscallResult::Return(sent as i64),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }

    let ignore_dst = tcp_sendto_ignores_destination(&socket);
    let dst = if args[4] != 0 && !ignore_dst {
        match read_sockaddr_in(ctx, args[4], args[5]) {
            Ok(addr) => Some(addr.as_ip_endpoint()),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        None
    };
    if let Err(errno) = maybe_autobind_udp_sendto(&socket, dst) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    let mut bytes = alloc::vec![0; len];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, args[1]) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    let inline_loopback = sendto_can_drive_loopback_inline(&socket, dst);
    if inline_loopback {
        loop {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                step_send_udp_loopback_kernel_bytes(&socket, dst, &bytes, flags, &guard)
            };
            match outcome {
                StepOutcome::Done(sent) => {
                    if sent > 0 && !flags.contains(SendRecvFlags::MSG_MORE) {
                        tx_reactor::yield_now().await;
                    }
                    return SyscallResult::Return(sent as i64);
                }
                StepOutcome::Continue { progress } => {
                    if progress.bytes() > 0 && !flags.contains(SendRecvFlags::MSG_MORE) {
                        tx_reactor::yield_now().await;
                    }
                    return SyscallResult::Return(progress.bytes() as i64);
                }
                StepOutcome::Yield { progress, shape } => {
                    if progress.bytes() > 0 {
                        if flags.contains(SendRecvFlags::MSG_MORE) {
                            return SyscallResult::Return(progress.bytes() as i64);
                        }
                        tx_reactor::yield_now().await;
                        return SyscallResult::Return(progress.bytes() as i64);
                    }
                    if flags.is_nonblocking() {
                        return SyscallResult::Error(EAGAIN_VALUE);
                    }
                    if let Some(future) = wait_on_yield_shape(shape) {
                        let _ = future.await;
                    } else {
                        return SyscallResult::Error(EIO_VALUE);
                    }
                }
                StepOutcome::Err(errno) => {
                    maybe_raise_sigpipe(ctx, errno, flags);
                    return SyscallResult::Error(errno_to_i32(errno));
                }
            }
        }
    }
    let mut total = 0usize;
    let mut remaining = bytes.as_slice();
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_send_to_kernel_bytes(&socket, dst, remaining, flags, &guard)
        };
        match outcome {
            StepOutcome::Done(sent) => {
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    finish_sendto_progress(&socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Continue { progress } => {
                let sent = progress.bytes();
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    finish_sendto_progress(&socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Yield { progress, shape } => {
                let sent = progress.bytes();
                total += sent;
                if sent >= remaining.len() {
                    finish_sendto_progress(&socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
                if total > 0 {
                    finish_sendto_progress(&socket, total, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                if flags.is_nonblocking() {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                if let Some(future) = wait_on_yield_shape(shape) {
                    let _ = future.await;
                } else {
                    return SyscallResult::Error(EIO_VALUE);
                }
            }
            StepOutcome::Err(errno) => {
                if total > 0 {
                    finish_sendto_progress(&socket, total, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                maybe_raise_sigpipe(ctx, errno, flags);
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

pub(super) async fn sys_recvfrom<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let (file, socket) = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let len = args[2] as usize;

    let mut flags = match SendRecvFlags::validate(args[3] as i32) {
        Ok(flags) => flags,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if file.flags().nonblocking {
        flags |= SendRecvFlags::MSG_DONTWAIT;
    }
    let is_netlink_socket = matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    );
    if !is_netlink_socket {
        if let Some(errno) = recv_special_flags_errno(flags) {
            return SyscallResult::Error(errno);
        }
        if let Err(errno) = validate_recvfrom_addrlen(ctx, args[5]) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if len == 0 && !(is_netlink_socket && flags.contains(SendRecvFlags::MSG_TRUNC)) {
        return SyscallResult::Return(0);
    }

    if is_netlink_socket {
        let mut staging = alloc::vec![0; len.min(NETLINK_RECVMSG_MAX)];
        let result = match socket.kind {
            SocketKind::NetlinkRoute => netlink_route_recv(&socket, &mut staging, flags),
            SocketKind::NetlinkNetfilter => netlink_netfilter_recv(&socket, &mut staging, flags),
            _ => unreachable!(),
        };
        let recv = match result {
            Ok(recv) => recv,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let copied = core::cmp::min(recv, staging.len());
        if copied > 0 {
            if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, args[1], &staging[..copied]) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        if let Err(errno) = write_sockaddr_nl(ctx, args[4], args[5]) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        return SyscallResult::Return(recv as i64);
    }

    let mut yielded_before_wait = false;
    loop {
        drive_loopback_pending();
        if recv_queued_len(&socket) == 0 {
            let ready = {
                let guard = tx_substrate::epoch::guard();
                match step_poll_ready(&socket, &guard) {
                    StepOutcome::Done(mask) => mask,
                    StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                    StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => PollMask::empty(),
                }
            };
            if !recv_ready_mask(ready) {
                if flags.is_nonblocking() {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                if !yielded_before_wait {
                    yielded_before_wait = true;
                    tx_reactor::yield_now().await;
                    continue;
                }
                let wait_token = {
                    let guard = tx_substrate::epoch::guard();
                    match step_poll_wait_token(&socket, PollMask::IN, &guard) {
                        StepOutcome::Done(token) => token,
                        StepOutcome::Err(errno) => {
                            return SyscallResult::Error(errno_to_i32(errno));
                        }
                        StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => None,
                    }
                };
                let Some(wait_token) = wait_token else {
                    return SyscallResult::Error(EIO_VALUE);
                };
                let Some(future) = wait_source::wait_on_token(wait_token) else {
                    return SyscallResult::Error(EIO_VALUE);
                };
                if matches!(
                    wait_on_socket_or_itimer::<P>(future, ctx.process.pid.0).await,
                    SocketWaitWake::ItimerExpired
                ) {
                    if recv_queued_len(&socket) > 0 {
                        yielded_before_wait = false;
                        continue;
                    }
                    return SyscallResult::Error(EINTR_VALUE);
                }
                yielded_before_wait = false;
                continue;
            }
        }
        yielded_before_wait = false;

        let mut staging = alloc::vec![0; recv_staging_len(&socket, len)];
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_recv_kernel_bytes(&socket, &mut staging, flags, &guard)
        };
        match outcome {
            StepOutcome::Done(recv) => {
                if recv.bytes > 0 {
                    if let Err(errno) =
                        bootstrap_copy_to_user(&ctx.aspace, args[1], &staging[..recv.bytes])
                    {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                if let Some(source) = recv.source {
                    if let Err(errno) = write_sockaddr_endpoint(ctx, args[4], args[5], source) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                if socket_recv_should_yield_after_success(&socket, recv.bytes) {
                    tx_reactor::yield_now().await;
                }
                return SyscallResult::Return(recv.bytes as i64);
            }
            StepOutcome::Yield { shape, .. } => {
                if flags.is_nonblocking() {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                if let Some(future) = wait_on_yield_shape(shape) {
                    if matches!(
                        wait_on_socket_or_itimer::<P>(future, ctx.process.pid.0).await,
                        SocketWaitWake::ItimerExpired
                    ) {
                        if recv_queued_len(&socket) > 0 {
                            continue;
                        }
                        return SyscallResult::Error(EINTR_VALUE);
                    }
                } else {
                    return SyscallResult::Error(EIO_VALUE);
                }
            }
            StepOutcome::Continue { .. } => {}
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    }
}

pub(super) async fn sys_sendmsg<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let (file, socket) = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let header = match read_msghdr(ctx, args[1]) {
        Ok(header) => header,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let mut flags = match SendRecvFlags::validate(args[2] as i32) {
        Ok(flags) => flags,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if file.flags().nonblocking {
        flags |= SendRecvFlags::MSG_DONTWAIT;
    }
    if let Err(errno) = validate_sendmsg_control(ctx, header) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    if matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    ) {
        if header.name != 0 {
            if let Err(errno) = read_sockaddr_nl(ctx, header.name, header.namelen as u64) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        let iovecs = match read_iovecs(ctx, header.iov, header.iovlen) {
            Ok(iovecs) => iovecs,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let total_len = match iov_total_len_with_limit(&iovecs, NETLINK_RECVMSG_MAX) {
            Ok(total_len) => total_len,
            Err(errno_value) => return SyscallResult::Error(errno_value),
        };
        if total_len == 0 {
            return SyscallResult::Return(0);
        }
        let mut bytes = alloc::vec::Vec::with_capacity(total_len);
        for iov in &iovecs {
            if iov.len == 0 {
                continue;
            }
            let start = bytes.len();
            bytes.resize(start + iov.len, 0);
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes[start..], iov.base)
            {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        let mut resolve_netns_fd = |fd: i32| {
            if fd < 0 {
                return None;
            }
            let file = resolve_fd(&ctx.process, fd as u32)?;
            net_namespace_payload_from_file(&file)
        };
        let mut resolve_netns_pid = |pid: u32| {
            let process = process_by_pid(Pid(pid))?;
            process.net_namespace()
        };
        let result = match socket.kind {
            SocketKind::NetlinkRoute => netlink_route_send_with_netns_resolvers(
                &socket,
                &bytes,
                ctx.cred(),
                &mut resolve_netns_fd,
                &mut resolve_netns_pid,
            ),
            SocketKind::NetlinkNetfilter => netlink_netfilter_send(&socket, &bytes, ctx.cred()),
            _ => unreachable!(),
        };
        return match result {
            Ok(sent) => SyscallResult::Return(sent as i64),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }

    let unix_dst = if matches!(
        socket.kind,
        SocketKind::UnixDatagram | SocketKind::UnixStream
    ) && header.name != 0
    {
        match read_sockaddr_un_path(ctx, header.name, header.namelen as u64) {
            Ok(path) => Some(path),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        None
    };
    let dst = if header.name != 0 && unix_dst.is_none() {
        match read_sockaddr_in(ctx, header.name, header.namelen as u64) {
            Ok(addr) => Some(addr.as_ip_endpoint()),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        None
    };
    if let Err(errno) = maybe_autobind_udp_sendto(&socket, dst) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    let iovecs = match read_iovecs(ctx, header.iov, header.iovlen) {
        Ok(iovecs) => iovecs,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let total_len = match if matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    ) {
        iov_total_len_with_limit(&iovecs, NETLINK_RECVMSG_MAX)
    } else {
        iov_total_len_with_limit(&iovecs, SOCKET_MSG_MAX_BYTES)
    } {
        Ok(total_len) => total_len,
        Err(errno_value) => return SyscallResult::Error(errno_value),
    };
    if total_len == 0 {
        return SyscallResult::Return(0);
    }

    let mut bytes = alloc::vec::Vec::with_capacity(total_len);
    for iov in &iovecs {
        if iov.len == 0 {
            continue;
        }
        let start = bytes.len();
        bytes.resize(start + iov.len, 0);
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes[start..], iov.base) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    let mut total = 0usize;
    let mut remaining = bytes.as_slice();
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            if let Some(unix_dst) = unix_dst {
                step_send_to_unix_path_kernel_bytes(&socket, unix_dst, remaining, flags, &guard)
            } else {
                step_send_to_kernel_bytes(&socket, dst, remaining, flags, &guard)
            }
        };
        match outcome {
            StepOutcome::Done(sent) => {
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    finish_sendto_progress(&socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Continue { progress } => {
                let sent = progress.bytes();
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    finish_sendto_progress(&socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Yield { progress, shape } => {
                let sent = progress.bytes();
                total += sent;
                if sent >= remaining.len() {
                    finish_sendto_progress(&socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
                if total > 0 {
                    finish_sendto_progress(&socket, total, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                if flags.is_nonblocking() {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                if let Some(future) = wait_on_yield_shape(shape) {
                    let _ = future.await;
                } else {
                    return SyscallResult::Error(EIO_VALUE);
                }
            }
            StepOutcome::Err(errno) => {
                if total > 0 {
                    finish_sendto_progress(&socket, total, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                maybe_raise_sigpipe(ctx, errno, flags);
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

pub(super) async fn sys_recvmsg<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let (file, socket) = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let header = match read_msghdr(ctx, args[1]) {
        Ok(header) => header,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let _control_ignored = header.control != 0 && header.controllen != 0;
    let mut flags = match SendRecvFlags::validate(args[2] as i32) {
        Ok(flags) => flags,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if file.flags().nonblocking {
        flags |= SendRecvFlags::MSG_DONTWAIT;
    }

    let iovecs = match read_iovecs(ctx, header.iov, header.iovlen) {
        Ok(iovecs) => iovecs,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let total_len = match if matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    ) {
        iov_total_len_with_limit(&iovecs, NETLINK_RECVMSG_MAX)
    } else {
        iov_total_len_with_limit(&iovecs, SOCKET_MSG_MAX_BYTES)
    } {
        Ok(total_len) => total_len,
        Err(errno_value) => return SyscallResult::Error(errno_value),
    };
    if let Err(errno) = write_msghdr_flags(ctx, args[1], 0) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    if let Err(errno) = write_msghdr_controllen(ctx, args[1], 0) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let is_netlink_socket = matches!(
        socket.kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
    );
    if !is_netlink_socket {
        if let Some(errno) = recv_special_flags_errno(flags) {
            return SyscallResult::Error(errno);
        }
    }
    if total_len == 0 && !(is_netlink_socket && flags.contains(SendRecvFlags::MSG_TRUNC)) {
        return SyscallResult::Return(0);
    }

    if is_netlink_socket {
        let mut staging = alloc::vec![0; total_len];
        let result = match socket.kind {
            SocketKind::NetlinkRoute => netlink_route_recv(&socket, &mut staging, flags),
            SocketKind::NetlinkNetfilter => netlink_netfilter_recv(&socket, &mut staging, flags),
            _ => unreachable!(),
        };
        let recv = match result {
            Ok(recv) => recv,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let copied = core::cmp::min(recv, staging.len());
        if copied > 0 {
            if let Err(errno) = scatter_to_iovecs(ctx, &iovecs, &staging[..copied]) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        if recv > copied {
            if let Err(errno) =
                write_msghdr_flags(ctx, args[1], SendRecvFlags::MSG_TRUNC.bits() as u32)
            {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        if let Err(errno) = write_sockaddr_nl_into_msghdr(ctx, args[1], header) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        return SyscallResult::Return(recv as i64);
    }

    let mut staging = alloc::vec![0; total_len];
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_recv_kernel_bytes(&socket, &mut staging, flags, &guard)
        };
        match outcome {
            StepOutcome::Done(recv) => {
                if recv.bytes > 0 {
                    if let Err(errno) = scatter_to_iovecs(ctx, &iovecs, &staging[..recv.bytes]) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                if let Some(source) = recv.source {
                    if let Err(errno) = write_sockaddr_into_msghdr(ctx, args[1], header, source) {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                if let Some(source) = recv.unix_source {
                    if let Err(errno) =
                        write_sockaddr_un_into_msghdr(ctx, args[1], header, Some(source))
                    {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                return SyscallResult::Return(recv.bytes as i64);
            }
            StepOutcome::Yield { shape, .. } => {
                if flags.is_nonblocking() {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                if let Some(future) = wait_on_yield_shape(shape) {
                    let _ = future.await;
                } else {
                    return SyscallResult::Error(EIO_VALUE);
                }
            }
            StepOutcome::Continue { .. } => {}
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    }
}

pub(super) async fn sys_sendmmsg<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if let Err(errno) = resolve_socket_fd(ctx, args[0] as i32) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let msgvec = args[1];
    let vlen = (args[2] as u64).min(MAX_MSG_IOV);
    let flags = args[3];
    if vlen == 0 {
        return SyscallResult::Return(0);
    }

    let mut sent_messages = 0u64;
    for index in 0..vlen {
        let header_ptr = match mmsghdr_slot_ptr(msgvec, index) {
            Ok(ptr) => ptr,
            Err(errno) => {
                if sent_messages == 0 {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                return SyscallResult::Return(sent_messages as i64);
            }
        };
        if let Err(errno) = validate_mmsghdr_slot(ctx, header_ptr) {
            if sent_messages == 0 {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            return SyscallResult::Return(sent_messages as i64);
        }
        let result = sys_sendmsg([args[0], header_ptr, flags, 0, 0, 0], ctx).await;
        match result {
            SyscallResult::Return(sent) if sent >= 0 => {
                if let Err(errno) = write_mmsghdr_len(ctx, header_ptr, sent as u32) {
                    if sent_messages == 0 {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    return SyscallResult::Return(sent_messages as i64);
                }
                sent_messages += 1;
            }
            SyscallResult::Error(_) if sent_messages > 0 => {
                return SyscallResult::Return(sent_messages as i64);
            }
            other => return other,
        }
    }

    SyscallResult::Return(sent_messages as i64)
}

pub(super) async fn sys_recvmmsg<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if let Err(errno) = resolve_socket_fd(ctx, args[0] as i32) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let msgvec = args[1];
    let vlen = (args[2] as u64).min(MAX_MSG_IOV);
    let flags = args[3];
    let timeout_ptr = args[4];
    let _timeout_ns = match read_recvmmsg_timeout(ctx, timeout_ptr) {
        Ok(timeout_ns) => timeout_ns,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if vlen == 0 {
        return SyscallResult::Return(0);
    }

    let mut received_messages = 0u64;
    for index in 0..vlen {
        let header_ptr = match mmsghdr_slot_ptr(msgvec, index) {
            Ok(ptr) => ptr,
            Err(errno) => {
                if received_messages == 0 {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                return SyscallResult::Return(received_messages as i64);
            }
        };
        if let Err(errno) = validate_mmsghdr_slot(ctx, header_ptr) {
            if received_messages == 0 {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            return SyscallResult::Return(received_messages as i64);
        }
        let result = sys_recvmsg([args[0], header_ptr, flags, 0, 0, 0], ctx).await;
        match result {
            SyscallResult::Return(recv) if recv >= 0 => {
                if let Err(errno) = write_mmsghdr_len(ctx, header_ptr, recv as u32) {
                    if received_messages == 0 {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                    return SyscallResult::Return(received_messages as i64);
                }
                received_messages += 1;
            }
            SyscallResult::Error(_) if received_messages > 0 => {
                return SyscallResult::Return(received_messages as i64);
            }
            other => return other,
        }
    }

    SyscallResult::Return(received_messages as i64)
}

pub(super) fn sys_setsockopt<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let level = args[1] as i32;
    let optname = args[2] as i32;
    let optval = args[3];
    let optlen = args[4] as u32;

    let payload = match socket.acquire_operational() {
        Some(payload) => payload,
        None => return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN)),
    };
    let result = match (level, optname) {
        (SOL_SOCKET, SO_REUSEADDR) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.reuse_addr = on);
            Ok(())
        }
        (SOL_SOCKET, SO_REUSEPORT) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.reuse_port = on);
            Ok(())
        }
        (SOL_SOCKET, SO_DONTROUTE) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.dont_route = on);
            Ok(())
        }
        (SOL_SOCKET, SO_KEEPALIVE) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.keep_alive = on);
            Ok(())
        }
        (SOL_SOCKET, SO_BROADCAST) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.broadcast = on);
            Ok(())
        }
        (SOL_SOCKET, SO_SNDBUF) => {
            let size = match read_sockopt_positive_usize(ctx, optval, optlen) {
                Ok(size) => size,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.send_buf_size = size);
            Ok(())
        }
        (SOL_SOCKET, SO_RCVBUF) => {
            let size = match read_sockopt_positive_usize(ctx, optval, optlen) {
                Ok(size) => size,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.recv_buf_size = size);
            Ok(())
        }
        (SOL_SOCKET, SO_LINGER) => {
            let linger = match read_sockopt_linger(ctx, optval, optlen) {
                Ok(linger) => linger,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.linger = linger);
            Ok(())
        }
        (SOL_SOCKET, SO_RCVTIMEO) => {
            let timeout = match read_sockopt_timeval(ctx, optval, optlen) {
                Ok(timeout) => timeout,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.recv_timeout = timeout);
            Ok(())
        }
        (SOL_SOCKET, SO_SNDTIMEO) => {
            let timeout = match read_sockopt_timeval(ctx, optval, optlen) {
                Ok(timeout) => timeout,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.socket.send_timeout = timeout);
            Ok(())
        }
        (SOL_SOCKET, SO_OOBINLINE) => {
            let _ = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(value) => value,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            Err(Errno::ENOPROTOOPT)
        }
        (IPPROTO_IP, IP_RECVERR) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.ip.recv_err = on);
            Ok(())
        }
        (IPPROTO_IP, MCAST_JOIN_GROUP | MCAST_LEAVE_GROUP) => {
            if !matches!(
                socket.kind,
                SocketKind::Tcp | SocketKind::Udp | SocketKind::RawIcmp
            ) {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let group = match read_sockopt_ipv4_mcast_group_req(ctx, optval, optlen) {
                Ok(group) => group,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            match optname {
                MCAST_JOIN_GROUP => payload.join_ipv4_multicast_group(group),
                MCAST_LEAVE_GROUP => payload.leave_ipv4_multicast_group(group),
                _ => unreachable!(),
            }
        }
        (IPPROTO_TCP, TCP_NODELAY) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.tcp.nodelay = on);
            Ok(())
        }
        (IPPROTO_TCP, TCP_MAXSEG) => {
            let size = match read_sockopt_positive_usize(ctx, optval, optlen) {
                Ok(size) => size,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            if size > u16::MAX as usize {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            payload.with_options_mut(|opts| opts.tcp.maxseg = size as u16);
            Ok(())
        }
        (SOL_NETLINK, NETLINK_EXT_ACK)
            if matches!(
                socket.kind,
                SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
            ) =>
        {
            let _ = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            Ok(())
        }
        (IPPROTO_IP, IPT_SO_SET_REPLACE) | (IPPROTO_IP, IPT_SO_SET_ADD_COUNTERS) => {
            Err(Errno::EOPNOTSUPP)
        }
        _ => Err(Errno::ENOPROTOOPT),
    };

    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) fn sys_getsockopt<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let level = args[1] as i32;
    let optname = args[2] as i32;
    let optval = args[3];
    let optlen_ptr = args[4];

    let payload = match socket.acquire_operational() {
        Some(payload) => payload,
        None => return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN)),
    };

    let result = match (level, optname) {
        (SOL_SOCKET, SO_REUSEADDR) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.reuse_addr as i32),
        ),
        (SOL_SOCKET, SO_REUSEPORT) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.reuse_port as i32),
        ),
        (SOL_SOCKET, SO_DONTROUTE) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.dont_route as i32),
        ),
        (SOL_SOCKET, SO_KEEPALIVE) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.keep_alive as i32),
        ),
        (SOL_SOCKET, SO_BROADCAST) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.broadcast as i32),
        ),
        (SOL_SOCKET, SO_SNDBUF) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.send_buf_size as i32),
        ),
        (SOL_SOCKET, SO_RCVBUF) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.recv_buf_size as i32),
        ),
        (SOL_SOCKET, SO_LINGER) => write_sockopt_linger(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.linger),
        ),
        (SOL_SOCKET, SO_TYPE) => {
            write_sockopt_i32(ctx, optval, optlen_ptr, socket_type_i32(&socket))
        }
        (SOL_SOCKET, SO_ERROR) => write_sockopt_i32(ctx, optval, optlen_ptr, 0),
        (SOL_SOCKET, SO_RCVTIMEO) => write_sockopt_timeval(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.recv_timeout),
        ),
        (SOL_SOCKET, SO_SNDTIMEO) => write_sockopt_timeval(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.socket.send_timeout),
        ),
        (SOL_SOCKET, SO_OOBINLINE) => {
            validate_getsockopt_i32_args(ctx, optval, optlen_ptr).and(Err(Errno::ENOPROTOOPT))
        }
        (IPPROTO_IP, IP_RECVERR) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.ip.recv_err as i32),
        ),
        (IPPROTO_TCP, TCP_NODELAY) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.tcp.nodelay as i32),
        ),
        (IPPROTO_TCP, TCP_MAXSEG) => {
            write_sockopt_i32(ctx, optval, optlen_ptr, tcp_effective_maxseg(&socket))
        }
        (IPPROTO_TCP, TCP_INFO) => write_sockopt_bytes(ctx, optval, optlen_ptr, &[0u8; 104]),
        (IPPROTO_TCP, TCP_CONGESTION) => write_sockopt_bytes(ctx, optval, optlen_ptr, b"reno\0"),
        (SOL_NETLINK, NETLINK_EXT_ACK)
            if matches!(
                socket.kind,
                SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter
            ) =>
        {
            write_sockopt_i32(ctx, optval, optlen_ptr, 1)
        }
        (IPPROTO_IP, IPT_SO_GET_INFO) => {
            write_sockopt_bytes(ctx, optval, optlen_ptr, &[0u8; IPT_GETINFO_BYTES])
        }
        (IPPROTO_IP, IPT_SO_GET_ENTRIES) => {
            write_sockopt_bytes(ctx, optval, optlen_ptr, &[0u8; IPT_GET_ENTRIES_EMPTY_BYTES])
        }
        (IPPROTO_UDP, _) => Err(Errno::EOPNOTSUPP),
        (SOL_SOCKET | IPPROTO_IP | IPPROTO_TCP | SOL_NETLINK, _) => Err(Errno::ENOPROTOOPT),
        _ => Err(Errno::EOPNOTSUPP),
    };

    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) fn sys_shutdown<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let how = match SockShutdownCmd::validate(args[1] as i32) {
        Ok(how) => how,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    let outcome = {
        let guard = tx_substrate::epoch::guard();
        step_shutdown(&socket, how, &guard)
    };
    match outcome {
        StepOutcome::Done(_) | StepOutcome::Continue { .. } => SyscallResult::Return(0),
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        StepOutcome::Yield { .. } => SyscallResult::Error(EIO_VALUE),
    }
}

pub(super) fn maybe_close_socket_file_after_fd_remove(file: &Cap<OpenFile>) {
    // `CloseOp` has already removed the fd-table entry. The `Cap` passed here
    // is the syscall's temporary reference; if anything else still retains the
    // same open-file description (dup, fork, or another in-kernel owner), the
    // underlying socket must stay alive.
    if file.retain_count() > 1 {
        return;
    }
    let socket = match socket_identity_from_file(file) {
        Ok(socket) => socket,
        Err(_) => return,
    };
    let guard = tx_substrate::epoch::guard();
    let _ = step_socket_close(&socket, &guard);
}

fn resolve_socket_fd<'a>(
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

fn socket_identity_from_file(file: &Cap<OpenFile>) -> Result<Cap<SocketIdentity>, Errno> {
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Socket { identity },
            } => Ok(identity.clone()),
            _ if open_file_is_path_only(file) => Err(Errno::EBADF),
            _ => Err(Errno::ENOTSOCK),
        },
        _ => Err(Errno::ENOTSOCK),
    }
}

fn open_file_is_path_only(file: &OpenFile) -> bool {
    let flags = file.flags();
    !flags.read && !flags.write
}

pub(super) fn socket_poll_mask_from_file(
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

pub(super) fn socket_poll_wait_token_from_file(
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

fn read_sockaddr_in<'a>(
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
    if sockaddr_len < SOCKADDR_IN_BYTES as u64 {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_IN_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, sockaddr_ptr)?;
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    if family != AF_INET {
        return Err(Errno::EAFNOSUPPORT);
    }
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    let addr = Ipv4Address::new([bytes[4], bytes[5], bytes[6], bytes[7]]);
    Ok(KernelSockAddr::V4(SockAddrIn::new(port, addr)))
}

fn read_sockaddr_un_path<'a>(
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
    let path_len = raw_path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(raw_path.len());
    if path_len == 0 || path_len > SOCKADDR_UN_PATH_BYTES {
        return Err(Errno::EINVAL);
    }
    UnixSocketPath::new(&raw_path[..path_len])
}

fn unix_pathname_bind_precheck<'a>(ctx: &SyscallCtx<'a>, path: &[u8]) -> Result<(), Errno> {
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

fn read_sockaddr_nl<'a>(
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

fn read_sockaddr_ll<'a>(
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
    Ok(SockAddrLl::new(protocol, ifindex))
}

fn read_msghdr<'a>(ctx: &SyscallCtx<'a>, msghdr_ptr: u64) -> Result<UserMsghdr, Errno> {
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

fn validate_sendmsg_control<'a>(ctx: &SyscallCtx<'a>, header: UserMsghdr) -> Result<(), Errno> {
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

fn mmsghdr_slot_ptr(msgvec: u64, index: u64) -> Result<u64, Errno> {
    if msgvec == 0 {
        return Err(Errno::EFAULT);
    }
    let offset = index.checked_mul(MMSGHDR_BYTES).ok_or(Errno::EINVAL)?;
    msgvec.checked_add(offset).ok_or(Errno::EINVAL)
}

fn validate_mmsghdr_slot<'a>(ctx: &SyscallCtx<'a>, mmsghdr_ptr: u64) -> Result<(), Errno> {
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

fn validate_user_range<'a>(
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

fn write_mmsghdr_len<'a>(
    ctx: &SyscallCtx<'a>,
    mmsghdr_ptr: u64,
    msg_len: u32,
) -> Result<(), Errno> {
    bootstrap_write_user(&ctx.aspace, mmsghdr_ptr + MMSGHDR_LEN_OFFSET, msg_len)
}

fn read_recvmmsg_timeout<'a>(ctx: &SyscallCtx<'a>, timeout_ptr: u64) -> Result<Option<u64>, Errno> {
    if timeout_ptr == 0 {
        return Ok(None);
    }

    let mut bytes = [0u8; 16];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, timeout_ptr)?;
    let tv_sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let tv_nsec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if tv_sec < 0 || tv_nsec < 0 || tv_nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }

    Ok(Some(
        (tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(tv_nsec as u64),
    ))
}

fn read_iovecs<'a>(
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

fn iov_total_len(iovecs: &[UserIovec]) -> Result<usize, i32> {
    iov_total_len_with_limit(iovecs, TTY_WRITE_MAX_INLINE)
}

fn iov_total_len_with_limit(iovecs: &[UserIovec], limit: usize) -> Result<usize, i32> {
    let mut total = 0usize;
    for iov in iovecs {
        total = total.checked_add(iov.len).ok_or(EINVAL_VALUE)?;
        if total > limit {
            return Err(E2BIG_VALUE);
        }
    }
    Ok(total)
}

fn scatter_to_iovecs<'a>(
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

fn write_msghdr_namelen<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    namelen: u32,
) -> Result<(), Errno> {
    bootstrap_write_user(&ctx.aspace, msghdr_ptr + MSGHDR_NAMELEN_OFFSET, namelen)
}

fn write_msghdr_controllen<'a>(
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

fn write_msghdr_flags<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    msg_flags: u32,
) -> Result<(), Errno> {
    bootstrap_write_user(&ctx.aspace, msghdr_ptr + MSGHDR_FLAGS_OFFSET, msg_flags)
}

fn write_sockaddr_into_msghdr<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    header: UserMsghdr,
    endpoint: IpEndpoint,
) -> Result<(), Errno> {
    if header.name == 0 {
        return Ok(());
    }
    write_msghdr_namelen(ctx, msghdr_ptr, SOCKADDR_IN_BYTES)?;
    if header.namelen < SOCKADDR_IN_BYTES {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_IN_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    bytes[4..8].copy_from_slice(&endpoint.addr.octets());
    bootstrap_copy_to_user(&ctx.aspace, header.name, &bytes)
}

fn write_sockaddr_nl_into_msghdr<'a>(
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

fn write_sockaddr_un_into_msghdr<'a>(
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

fn write_sockaddr_nl<'a>(
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

fn write_sockaddr_ll<'a>(
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
    bootstrap_copy_to_user(&ctx.aspace, sockaddr_ptr, &bytes)
}

fn write_sockaddr_un<'a>(
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

fn sockaddr_un_len(path: Option<UnixSocketPath>) -> usize {
    SOCKADDR_UN_MIN_BYTES as usize + path.map_or(0, |path| path.len().saturating_add(1))
}

fn sockaddr_un_bytes(path: Option<UnixSocketPath>) -> [u8; SOCKADDR_UN_MAX_BYTES as usize] {
    let mut bytes = [0u8; SOCKADDR_UN_MAX_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_UNIX.to_le_bytes());
    if let Some(path) = path {
        let start = SOCKADDR_UN_MIN_BYTES as usize;
        let end = start + path.len();
        bytes[start..end].copy_from_slice(path.as_bytes());
    }
    bytes
}

fn write_sockaddr_endpoint<'a>(
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

    let len: u32 = bootstrap_read_user(&ctx.aspace, sockaddr_len_ptr)?;
    bootstrap_write_user(&ctx.aspace, sockaddr_len_ptr, SOCKADDR_IN_BYTES)?;
    if invalid_socklen(len) {
        return Err(Errno::EINVAL);
    }
    if len < SOCKADDR_IN_BYTES {
        return Err(Errno::EINVAL);
    }

    let mut bytes = [0u8; SOCKADDR_IN_BYTES as usize];
    bytes[0..2].copy_from_slice(&AF_INET.to_le_bytes());
    bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    bytes[4..8].copy_from_slice(&endpoint.addr.octets());
    bootstrap_copy_to_user(&ctx.aspace, sockaddr_ptr, &bytes)
}

fn socket_local_endpoint(socket: &Cap<SocketIdentity>) -> Result<IpEndpoint, Errno> {
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Bound { local })
        | SocketProtocol::Tcp(TcpState::Listening { local, .. })
        | SocketProtocol::Tcp(TcpState::Connecting { local, .. })
        | SocketProtocol::Tcp(TcpState::Connected { local, .. })
        | SocketProtocol::Udp(UdpInner::Bound { local })
        | SocketProtocol::Udp(UdpInner::Connected { local, .. }) => Ok(local),
        SocketProtocol::RawIcmp(state) => Ok(IpEndpoint::new(
            state.bound_local.unwrap_or(Ipv4Address::UNSPECIFIED),
            0,
        )),
        SocketProtocol::Tcp(TcpState::Init) | SocketProtocol::Udp(UdpInner::Unbound) => {
            Ok(IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0))
        }
        SocketProtocol::UnixDatagram(_)
        | SocketProtocol::UnixStream(_)
        | SocketProtocol::NetlinkRoute(_)
        | SocketProtocol::NetlinkNetfilter(_)
        | SocketProtocol::Packet(_) => Ok(IpEndpoint::new(Ipv4Address::UNSPECIFIED, 0)),
        SocketProtocol::Tcp(TcpState::Closed) | SocketProtocol::Udp(UdpInner::Closed) => {
            Err(Errno::ENOTCONN)
        }
    }
}

fn socket_peer_endpoint(socket: &Cap<SocketIdentity>) -> Result<IpEndpoint, Errno> {
    let payload = socket.acquire_operational().ok_or(Errno::ENOTCONN)?;
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connecting { remote, .. })
        | SocketProtocol::Tcp(TcpState::Connected { remote, .. })
        | SocketProtocol::Udp(UdpInner::Connected { remote, .. }) => Ok(remote),
        _ => Err(Errno::ENOTCONN),
    }
}

fn tcp_sendto_ignores_destination(socket: &Cap<SocketIdentity>) -> bool {
    if socket.kind != SocketKind::Tcp {
        return false;
    }
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connected { .. })
        )
    })
}

fn socket_is_tcp_connecting(socket: &Cap<SocketIdentity>) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connecting { .. })
        )
    })
}

fn validate_recvfrom_addrlen<'a>(ctx: &SyscallCtx<'a>, sockaddr_len_ptr: u64) -> Result<(), Errno> {
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

fn read_sockopt_bool<'a>(ctx: &SyscallCtx<'a>, optval: u64, optlen: u32) -> Result<bool, Errno> {
    Ok(read_sockopt_i32(ctx, optval, optlen)? != 0)
}

fn read_sockopt_positive_usize<'a>(
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

fn read_sockopt_i32<'a>(ctx: &SyscallCtx<'a>, optval: u64, optlen: u32) -> Result<i32, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < core::mem::size_of::<i32>() as u32 {
        return Err(Errno::EINVAL);
    }
    bootstrap_read_user(&ctx.aspace, optval)
}

fn read_sockopt_ipv4_mcast_group_req<'a>(
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

fn read_sockopt_linger<'a>(
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

fn read_sockopt_timeval<'a>(
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

fn write_sockopt_i32<'a>(
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

fn write_sockopt_linger<'a>(
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

fn write_sockopt_timeval<'a>(
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

fn write_sockopt_bytes<'a>(
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

fn validate_getsockopt_i32_args<'a>(
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

fn invalid_socklen(len: u32) -> bool {
    len > i32::MAX as u32
}

fn socket_type_i32(socket: &Cap<SocketIdentity>) -> i32 {
    match socket.kind {
        SocketKind::UnixStream => 1,
        SocketKind::UnixDatagram => 2,
        SocketKind::Tcp => 1,
        SocketKind::Udp => 2,
        SocketKind::RawIcmp => 3,
        SocketKind::NetlinkRoute | SocketKind::NetlinkNetfilter | SocketKind::Packet => 3,
    }
}

fn step_unit_result(outcome: StepOutcome<(), NoProgress>) -> SyscallResult {
    match outcome {
        StepOutcome::Done(()) | StepOutcome::Continue { .. } => SyscallResult::Return(0),
        StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        StepOutcome::Yield { .. } => SyscallResult::Error(EIO_VALUE),
    }
}

fn wait_on_yield_shape(shape: YieldShape) -> Option<wait_source::RegisteredWaitFuture> {
    match shape {
        YieldShape::OnWaitSource { source, interests }
        | YieldShape::OnEdge { source, interests } => {
            let token = tx_subsystems::execution::WaitToken::new(source.raw(), interests.raw());
            wait_source::wait_on_token(token)
        }
        YieldShape::OnAgent { .. } | YieldShape::OnTimer { .. } => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketWaitWake {
    SocketReady,
    ItimerExpired,
}

async fn wait_on_socket_or_itimer<P: TimeIf>(
    mut socket_future: wait_source::RegisteredWaitFuture,
    pid: u32,
) -> SocketWaitWake {
    if super::time::consume_itimer_real_delivered_interrupt(pid) {
        return SocketWaitWake::ItimerExpired;
    }
    let Some(deadline_ns) = super::time::itimer_real_deadline_ns(pid) else {
        let _ = socket_future.await;
        return SocketWaitWake::SocketReady;
    };
    if <P as TimeIf>::read_ns() >= deadline_ns {
        return SocketWaitWake::ItimerExpired;
    }
    let Some(mut timer_future) = tx_subsystems::timer_sleep::sleep_until_ns(deadline_ns) else {
        let _ = socket_future.await;
        return SocketWaitWake::SocketReady;
    };

    core::future::poll_fn(|cx| {
        if core::future::Future::poll(core::pin::Pin::new(&mut socket_future), cx).is_ready() {
            return core::task::Poll::Ready(SocketWaitWake::SocketReady);
        }
        if core::future::Future::poll(core::pin::Pin::new(&mut timer_future), cx).is_ready() {
            return core::task::Poll::Ready(SocketWaitWake::ItimerExpired);
        }
        core::task::Poll::Pending
    })
    .await
}

fn recv_special_flags_errno(flags: SendRecvFlags) -> Option<i32> {
    if flags.contains(SendRecvFlags::MSG_OOB) {
        Some(EINVAL_VALUE)
    } else if flags.contains(SendRecvFlags::MSG_ERRQUEUE) {
        Some(EAGAIN_VALUE)
    } else {
        None
    }
}

fn maybe_raise_sigpipe<'a>(ctx: &SyscallCtx<'a>, errno: Errno, flags: SendRecvFlags) {
    if errno == Errno::EPIPE && !flags.contains(SendRecvFlags::MSG_NOSIGNAL) {
        let _ = step_kill_process(&ctx.process, Signum::SIGPIPE, None);
    }
}
