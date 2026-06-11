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
    step_unix_socketpair_connect, AddressFamily, ConnectionKey, IpEndpoint, Ipv4Address,
    Ipv4MulticastGroup, Ipv6Address, KernelSockAddr, LingerOption, PollMask, SendRecvFlags,
    SockAddrIn, SockAddrIn6, SockAddrLl, SockShutdownCmd, SocketHandleFlags, SocketIdentity,
    SocketKind, SocketProtocol, SocketType, TcpState, TcpTlsUlpState, UdpInner, UnixDatagramState,
    UnixPeerCred, UnixSocketPath, UnixStreamState, ValidSocketType, UDP_IPV4_MAX_PAYLOAD_BYTES,
    VIRTIO_NET_DEFAULT_MTU,
};
use tx_subsystems::signal::step_kill_process;
use tx_subsystems::vfs::structure::OpenFileBacking;
use tx_subsystems::vm::UserAccessKind;
use tx_subsystems::wait_source;

const SOCKADDR_IN_BYTES: u32 = 16;
const SOCKADDR_IN6_BYTES: u32 = 28;
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
const IPT_REPLACE_HEADER_BYTES: usize = 96;
const IPT_REPLACE_SIZE_OFFSET: usize = 40;
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

pub(super) fn sys_socketpair<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let domain = args[0] as i32;
    let type_ = args[1] as i32;
    let protocol = args[2] as i32;
    let sv = args[3];
    let valid = match ValidSocketType::validate(domain, type_, protocol) {
        Ok(valid) => valid,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let kind = match SocketKind::from_valid_socket_type(valid) {
        Ok(kind) => kind,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if valid.domain != AddressFamily::Unix {
        return SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP));
    }
    if !matches!(kind, SocketKind::UnixDatagram | SocketKind::UnixStream) {
        return SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP));
    }
    if let Err(errno) = validate_user_range(ctx, sv, 8, UserAccessKind::Write) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    let Some(net_namespace) = ctx.process.net_namespace() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let first = {
        let guard = tx_substrate::epoch::guard();
        match step_socket_open_file_in_namespace(
            domain,
            type_,
            protocol,
            net_namespace.clone(),
            &guard,
        ) {
            StepOutcome::Done(opened) => opened,
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
        }
    };
    let second = {
        let guard = tx_substrate::epoch::guard();
        match step_socket_open_file_in_namespace(domain, type_, protocol, net_namespace, &guard) {
            StepOutcome::Done(opened) => opened,
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
        }
    };
    {
        let guard = tx_substrate::epoch::guard();
        match step_unix_socketpair_connect(&first.identity, &second.identity, &guard) {
            StepOutcome::Done(()) => {}
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            StepOutcome::Continue { .. } | StepOutcome::Yield { .. } => {
                return SyscallResult::Error(EIO_VALUE);
            }
        }
    }

    let first_fd = ctx.process.allocate_fd();
    let _ = ctx.process.set_fd(first_fd, Some(first.file));
    ctx.process.set_fd_cloexec(first_fd, first.cloexec);
    let second_fd = ctx.process.allocate_fd();
    let _ = ctx.process.set_fd(second_fd, Some(second.file));
    ctx.process.set_fd_cloexec(second_fd, second.cloexec);
    match bootstrap_write_user::<[i32; 2]>(&ctx.aspace, sv, [first_fd as i32, second_fd as i32]) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
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
        if !path.is_abstract() {
            if let Err(errno) = unix_pathname_bind_precheck(ctx, path.as_bytes()) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
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
    let requested = addr.as_ip_endpoint();
    if requested.port != 0 && requested.port < 1024 && ctx.cred().euid.raw() != 0 {
        return SyscallResult::Error(EACCES_VALUE);
    }

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

pub(super) fn sys_accept<'a, P: TimeIf + 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    accept_entry::<P>(args, ctx)
}

async fn accept_entry<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    accept_impl::<P>(args[0] as i32, args[1], args[2], 0, ctx).await
}

pub(super) fn sys_accept4<'a, P: TimeIf + 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    accept4_entry::<P>(args, ctx)
}

async fn accept4_entry<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let flags = args[3] as u32;
    if flags & !ACCEPT4_KNOWN_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    accept_impl::<P>(args[0] as i32, args[1], args[2], flags, ctx).await
}

async fn accept_impl<'a, P: TimeIf>(
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
                    let wake = wait_on_socket_or_itimer::<P>(
                        future,
                        ctx.process.pid.0,
                        ctx.mailbox.as_deref(),
                    )
                    .await;
                    if matches!(wake, SocketWaitWake::ItimerExpired)
                        || (matches!(wake, SocketWaitWake::SignalInterrupted)
                            && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread))
                    {
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

pub(super) fn sys_connect<'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    connect_impl(args, ctx)
}

async fn connect_impl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
            StepOutcome::Done(()) => {
                record_unix_stream_peer_cred(&socket, ctx);
                return SyscallResult::Return(0);
            }
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

fn record_unix_stream_peer_cred(socket: &Cap<SocketIdentity>, ctx: &SyscallCtx<'_>) {
    if socket.kind != SocketKind::UnixStream {
        return;
    }
    let Some(payload) = socket.acquire_operational() else {
        return;
    };
    let guard = tx_substrate::epoch::guard();
    let Some(peer) = payload
        .socket_table()
        .lookup_unix_stream_peer(socket.raw(), &guard)
    else {
        return;
    };
    let Some(peer_payload) = peer.acquire_operational() else {
        return;
    };
    let cred = ctx.cred();
    peer_payload.set_unix_peer_cred(UnixPeerCred {
        pid: ctx.process.pid.0,
        uid: cred.uid.raw(),
        gid: cred.gid.raw(),
    });
}

mod helpers;
use helpers::*;
pub(super) use helpers::{
    drive_loopback_pending, socket_identity_from_file, socket_poll_mask_from_file,
    socket_poll_wait_token_from_file, unix_pathname_key,
};

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
    if matches!(
        socket.kind,
        SocketKind::UnixDatagram | SocketKind::UnixStream
    ) {
        let path = match socket_unix_local_path(&socket) {
            Ok(path) => path,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        return match write_sockaddr_un(ctx, args[1], args[2], path) {
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
    if matches!(
        socket.kind,
        SocketKind::UnixDatagram | SocketKind::UnixStream
    ) {
        let path = match socket_unix_peer_path(&socket) {
            Ok(path) => path,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        return match write_sockaddr_un(ctx, args[1], args[2], path) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }
    let endpoint = match socket_peer_endpoint(&socket) {
        Ok(endpoint) => endpoint,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    match write_sockaddr_endpoint(ctx, args[1], args[2], endpoint) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

pub(super) fn sys_sendto<'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    sendto_impl(args, ctx)
}

async fn sendto_impl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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

    if socket.kind == SocketKind::Packet {
        let Some(payload) = socket.acquire_operational() else {
            return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN));
        };
        let sockaddr = if args[4] != 0 {
            match read_sockaddr_ll(ctx, args[4], args[5]) {
                Ok(sockaddr) => sockaddr,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            }
        } else {
            match payload.packet_sockaddr() {
                Some(sockaddr) if sockaddr.ifindex != 0 => sockaddr,
                _ => return SyscallResult::Error(errno_to_i32(Errno::EDESTADDRREQ)),
            }
        };
        let Some(netns) = ctx.process.net_namespace() else {
            return SyscallResult::Error(ESRCH_VALUE);
        };
        if sockaddr.ifindex <= 0
            || !netns
                .link_snapshot()
                .into_iter()
                .any(|link| link.ifindex == sockaddr.ifindex as u32)
        {
            return SyscallResult::Error(ENODEV_VALUE);
        }
        if let Err(errno) = validate_user_range(ctx, args[1], len, UserAccessKind::Read) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        return SyscallResult::Return(len as i64);
    }

    let ignore_dst = tcp_sendto_ignores_destination(&socket);
    let unix_dst = if matches!(
        socket.kind,
        SocketKind::UnixDatagram | SocketKind::UnixStream
    ) && args[4] != 0
        && !ignore_dst
    {
        match read_sockaddr_un_path(ctx, args[4], args[5]) {
            Ok(path) => Some(path),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        None
    };
    let dst = if args[4] != 0 && unix_dst.is_none() && !ignore_dst {
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

    if raw_icmp_hdrincl_enabled(&socket) {
        if let Err(errno) = validate_user_range(ctx, args[1], len, UserAccessKind::Read) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        return SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP));
    }
    if let Err(errno) = validate_udp_send_payload_len(&socket, len) {
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
                        if wait_on_socket_or_signal(future, ctx.mailbox.as_deref()).await
                            && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread)
                        {
                            return SyscallResult::Error(EINTR_VALUE);
                        }
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
                    if wait_on_socket_or_signal(future, ctx.mailbox.as_deref()).await
                        && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread)
                    {
                        return SyscallResult::Error(EINTR_VALUE);
                    }
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

pub(super) fn sys_recvfrom<'a, P: TimeIf + 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    recvfrom_impl::<P>(args, ctx)
}

async fn recvfrom_impl<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
                let wake =
                    wait_on_socket_or_itimer::<P>(future, ctx.process.pid.0, ctx.mailbox.as_deref())
                        .await;
                if matches!(wake, SocketWaitWake::ItimerExpired)
                    || (matches!(wake, SocketWaitWake::SignalInterrupted)
                        && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread))
                {
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
                } else if let Some(source) = recv.unix_source {
                    if let Err(errno) = write_sockaddr_un(ctx, args[4], args[5], Some(source)) {
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
                    let wake = wait_on_socket_or_itimer::<P>(
                        future,
                        ctx.process.pid.0,
                        ctx.mailbox.as_deref(),
                    )
                    .await;
                    if matches!(wake, SocketWaitWake::ItimerExpired)
                        || (matches!(wake, SocketWaitWake::SignalInterrupted)
                            && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread))
                    {
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

pub(super) fn sys_sendmsg<'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    sendmsg_impl(args, ctx)
}

async fn sendmsg_impl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    if let Err(errno) = validate_udp_send_payload_len(&socket, total_len) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    if raw_icmp_hdrincl_enabled(&socket) {
        if let Err(errno) = validate_iovec_read_ranges(ctx, &iovecs) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        return SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP));
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
                    if wait_on_socket_or_signal(future, ctx.mailbox.as_deref()).await
                        && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread)
                    {
                        return SyscallResult::Error(EINTR_VALUE);
                    }
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

fn raw_icmp_hdrincl_enabled(socket: &Cap<SocketIdentity>) -> bool {
    if socket.kind != SocketKind::RawIcmp {
        return false;
    }
    socket
        .acquire_operational()
        .is_some_and(|payload| payload.with_options(|options| options.ip.hdr_incl))
}

fn validate_udp_send_payload_len(socket: &Cap<SocketIdentity>, len: usize) -> Result<(), Errno> {
    if socket.kind != SocketKind::Udp {
        return Ok(());
    }
    let Some(payload) = socket.acquire_operational() else {
        return Err(Errno::ENOTCONN);
    };
    if payload.udp_corked_send_len().saturating_add(len) > UDP_IPV4_MAX_PAYLOAD_BYTES {
        return Err(Errno::EMSGSIZE);
    }
    Ok(())
}

fn validate_iovec_read_ranges<'a>(ctx: &SyscallCtx<'a>, iovecs: &[UserIovec]) -> Result<(), Errno> {
    for iov in iovecs {
        if iov.len == 0 {
            continue;
        }
        validate_user_range(ctx, iov.base, iov.len, UserAccessKind::Read)?;
    }
    Ok(())
}

pub(super) fn sys_recvmsg<'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    recvmsg_impl(args, ctx)
}

async fn recvmsg_impl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
                    if wait_on_socket_or_signal(future, ctx.mailbox.as_deref()).await
                        && tx_subsystems::signal::thread_pending_signal_interrupts(&ctx.thread)
                    {
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

pub(super) fn sys_sendmmsg<'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    sendmmsg_impl(args, ctx)
}

async fn sendmmsg_impl<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
        let result = sendmsg_impl([args[0], header_ptr, flags, 0, 0, 0], ctx).await;
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

pub(super) fn sys_recvmmsg<'a, P: TimeIf + 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a {
    recvmmsg_impl::<P>(args, ctx)
}

async fn recvmmsg_impl<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
        let result = recvmsg_impl([args[0], header_ptr, flags, 0, 0, 0], ctx).await;
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
        (SOL_SOCKET, SO_SNDBUFFORCE) => {
            let raw_size = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(size) => size as u32,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let size = core::cmp::min(raw_size as usize, i32::MAX as usize);
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
        (SOL_SOCKET, SO_NO_CHECK) => {
            let _ = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
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
        (IPPROTO_IP, IP_HDRINCL) => {
            if socket.kind != SocketKind::RawIcmp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.ip.hdr_incl = on);
            Ok(())
        }
        (SOL_IPV6, IPV6_V6ONLY) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            if payload.family() != AddressFamily::Inet6 {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            payload.with_options_mut(|opts| opts.ip.ipv6_v6only = on);
            Ok(())
        }
        (SOL_IPV6, IPV6_ADDRFORM) => set_ipv6_addrform(&socket, &payload, ctx, optval, optlen),
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
        (IPPROTO_TCP, TCP_ULP) => set_tcp_ulp(&socket, &payload, ctx, optval, optlen),
        (SOL_TLS, TLS_TX) => set_tls_tx(&socket, &payload, ctx, optval, optlen),
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
        (IPPROTO_IP, IPT_SO_SET_REPLACE) => {
            validate_ipt_replace_request(ctx, optval, optlen.into())
        }
        (IPPROTO_IP, IPT_SO_SET_ADD_COUNTERS) => Err(Errno::EOPNOTSUPP),
        (SOL_PACKET, PACKET_VERSION) => {
            if socket.kind != SocketKind::Packet {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let version = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(version) => version,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            if !(TPACKET_V1..=TPACKET_V3).contains(&version) {
                Err(Errno::EINVAL)
            } else {
                payload.set_packet_version(version)
            }
        }
        (SOL_PACKET, PACKET_RESERVE) => {
            if socket.kind != SocketKind::Packet {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let reserve = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(reserve) if reserve >= 0 => reserve as u32,
                Ok(_) => return SyscallResult::Error(errno_to_i32(Errno::EINVAL)),
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.set_packet_reserve(reserve)
        }
        (SOL_PACKET, PACKET_VNET_HDR) => {
            if socket.kind != SocketKind::Packet {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let enabled = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(enabled) => enabled != 0,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.set_packet_vnet_hdr(enabled)
        }
        (SOL_PACKET, PACKET_RX_RING) => {
            if socket.kind != SocketKind::Packet {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let req = match read_packet_rx_ring_req(ctx, optval, optlen) {
                Ok(req) => req,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            if req.block_nr != 0
                && req.frame_nr != 0
                && matches!(payload.packet_reserve(), Ok(reserve) if reserve > req.block_size)
            {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            match validate_packet_rx_ring_req(req) {
                Ok(()) => {
                    let block_size = if req.block_nr == 0 && req.frame_nr == 0 {
                        None
                    } else {
                        Some(req.block_size)
                    };
                    payload.set_packet_rx_ring_block_size(block_size)
                }
                Err(errno) => Err(errno),
            }
        }
        _ => Err(Errno::ENOPROTOOPT),
    };

    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

fn set_tcp_ulp<'a>(
    socket: &Cap<SocketIdentity>,
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::Tcp {
        return Err(Errno::ENOPROTOOPT);
    }
    if !matches!(
        payload.protocol_snapshot(),
        SocketProtocol::Tcp(TcpState::Connected { .. })
    ) {
        return Err(Errno::ENOTCONN);
    }
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if !(3..=16).contains(&optlen) {
        return Err(Errno::EINVAL);
    }

    let len = optlen as usize;
    let mut name = [0u8; 16];
    bootstrap_copy_from_user(&ctx.aspace, &mut name[..len], optval)?;
    let end = name[..len]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(len);
    if &name[..end] != b"tls" {
        return Err(Errno::ENOENT);
    }

    payload.with_options_mut(|opts| opts.tcp.tls_ulp = Some(TcpTlsUlpState::attached()));
    Ok(())
}

fn set_tls_tx<'a>(
    socket: &Cap<SocketIdentity>,
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::Tcp {
        return Err(Errno::ENOPROTOOPT);
    }
    if !payload.with_options(|opts| opts.tcp.tls_ulp.is_some()) {
        return Err(Errno::ENOPROTOOPT);
    }
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < 4 {
        return Err(Errno::EINVAL);
    }

    let mut info = [0u8; 4];
    bootstrap_copy_from_user(&ctx.aspace, &mut info, optval)?;
    payload.with_options_mut(|opts| opts.tcp.tls_ulp = Some(TcpTlsUlpState::with_tx_config()));
    Ok(())
}

fn set_ipv6_addrform<'a>(
    socket: &Cap<SocketIdentity>,
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<(), Errno> {
    let requested_family = read_sockopt_i32(ctx, optval, optlen)?;
    if requested_family != AF_INET as i32 {
        return Err(Errno::EINVAL);
    }
    if socket.kind != SocketKind::Tcp || payload.family() != AddressFamily::Inet6 {
        return Err(Errno::EINVAL);
    }
    match payload.protocol_snapshot() {
        SocketProtocol::Tcp(TcpState::Connected { local, remote })
            if local.family == AddressFamily::Inet && remote.family == AddressFamily::Inet =>
        {
            payload.set_family(AddressFamily::Inet);
            Ok(())
        }
        _ => Err(Errno::EINVAL),
    }
}

fn validate_ipt_replace_request<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u64,
) -> Result<(), Errno> {
    let optlen = usize::try_from(optlen).map_err(|_| Errno::EINVAL)?;
    if optlen < IPT_REPLACE_HEADER_BYTES {
        if optval == 0 {
            return Err(Errno::EFAULT);
        }
        validate_user_range(ctx, optval, optlen, UserAccessKind::Read)?;
        return Err(Errno::EINVAL);
    }

    let mut header = [0u8; IPT_REPLACE_HEADER_BYTES];
    bootstrap_copy_from_user(&ctx.aspace, &mut header, optval)?;
    let size = u32::from_le_bytes(
        header[IPT_REPLACE_SIZE_OFFSET..IPT_REPLACE_SIZE_OFFSET + 4]
            .try_into()
            .map_err(|_| Errno::EINVAL)?,
    ) as usize;
    let total = IPT_REPLACE_HEADER_BYTES
        .checked_add(size)
        .ok_or(Errno::EINVAL)?;
    if total > optlen {
        return Err(Errno::EINVAL);
    }
    validate_user_range(ctx, optval, total, UserAccessKind::Read)?;
    Err(Errno::EOPNOTSUPP)
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
        (SOL_SOCKET, SO_SNDBUFFORCE) => write_sockopt_i32(
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
        (SOL_SOCKET, SO_PEERCRED) if socket.kind == SocketKind::UnixStream => {
            match payload.unix_peer_cred() {
                Some(cred) => write_sockopt_unix_peer_cred(ctx, optval, optlen_ptr, cred),
                None => Err(Errno::ENOTCONN),
            }
        }
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
        (IPPROTO_IP, IP_HDRINCL) if socket.kind == SocketKind::RawIcmp => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.ip.hdr_incl as i32),
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
        (SOL_PACKET, PACKET_RESERVE) if socket.kind == SocketKind::Packet => {
            match payload.packet_reserve() {
                Ok(reserve) => write_sockopt_i32(ctx, optval, optlen_ptr, reserve as i32),
                Err(errno) => Err(errno),
            }
        }
        (SOL_PACKET, PACKET_VNET_HDR) if socket.kind == SocketKind::Packet => {
            match payload.packet_vnet_hdr() {
                Ok(enabled) => {
                    write_sockopt_i32(ctx, optval, optlen_ptr, if enabled { 1 } else { 0 })
                }
                Err(errno) => Err(errno),
            }
        }
        (IPPROTO_UDP, _) => Err(Errno::EOPNOTSUPP),
        (SOL_SOCKET | IPPROTO_IP | IPPROTO_TCP | SOL_NETLINK | SOL_PACKET, _) => {
            Err(Errno::ENOPROTOOPT)
        }
        _ => Err(Errno::EOPNOTSUPP),
    };

    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

fn write_sockopt_unix_peer_cred<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
    cred: UnixPeerCred,
) -> Result<(), Errno> {
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&(cred.pid as i32).to_le_bytes());
    bytes[4..8].copy_from_slice(&cred.uid.to_le_bytes());
    bytes[8..12].copy_from_slice(&cred.gid.to_le_bytes());
    write_sockopt_bytes(ctx, optval, optlen_ptr, &bytes)
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
