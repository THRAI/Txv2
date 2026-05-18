//! Socket syscall shims for the N39 fdtable integration slice.
//!
//! The syscall layer owns Linux ABI decoding and fdtable installation.
//! Socket state transitions stay in `tx_subsystems::net::execution`
//! steps so the identity/payload split and wait-carrier discipline stay
//! in the network subsystem.

use super::*;

use tx_substrate::step::{NoProgress, StepOutcome, YieldShape};
use tx_subsystems::net::{
    socket_open_file_from_identity, step_accept, step_bind, step_connect, step_listen,
    step_poll_ready, step_poll_wait_token, step_recv_kernel_bytes, step_send_to_kernel_bytes,
    step_shutdown, step_socket_close, step_socket_open_file, step_tcp_loopback_transfer,
    IpEndpoint, Ipv4Address, KernelSockAddr, LingerOption, PollMask, SendRecvFlags, SockAddrIn,
    SockShutdownCmd, SocketHandleFlags, SocketIdentity, SocketKind, SocketProtocol, TcpState,
    UdpInner,
};
use tx_subsystems::signal::step_kill_process;
use tx_subsystems::wait_source;

const SOCKADDR_IN_BYTES: u32 = 16;
const ACCEPT4_KNOWN_FLAGS: u32 = O_CLOEXEC | O_NONBLOCK;
const EPHEMERAL_PORT_START: u16 = 49_152;
const EPHEMERAL_PORT_END: u16 = 49_216;
const IOVEC_BYTES: u64 = 16;
const MSGHDR_BYTES: u64 = 56;
const MSGHDR_NAMELEN_OFFSET: u64 = 8;
const MSGHDR_CONTROLLEN_OFFSET: u64 = 40;
const MSGHDR_FLAGS_OFFSET: u64 = 48;
const MAX_MSG_IOV: u64 = 1024;

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

    let outcome = {
        let guard = tx_substrate::epoch::guard();
        step_socket_open_file(domain, type_, protocol, &guard)
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
                if let Err(errno) =
                    write_sockaddr_endpoint(ctx, addr_ptr, addrlen_ptr, accepted.peer)
                {
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
    let remote = match read_sockaddr_in(ctx, args[1], args[2]) {
        Ok(remote) => remote,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if let Err(errno) = maybe_autobind_tcp_client(&socket, remote) {
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
                if nonblocking {
                    let errno = if was_connecting {
                        Errno::EALREADY
                    } else {
                        Errno::EINPROGRESS
                    };
                    return SyscallResult::Error(errno_to_i32(errno));
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

fn maybe_autobind_tcp_client(
    socket: &Cap<SocketIdentity>,
    remote: KernelSockAddr,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::Tcp {
        return Ok(());
    }

    let remote_endpoint = remote.as_ip_endpoint();
    let local_addr = if remote_endpoint.addr == Ipv4Address::LOOPBACK {
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

pub(super) fn sys_getsockname<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
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

    let dst = if args[4] != 0 {
        match read_sockaddr_in(ctx, args[4], args[5]) {
            Ok(addr) => Some(addr.as_ip_endpoint()),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        None
    };

    let mut bytes = alloc::vec![0; len];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, args[1]) {
        return SyscallResult::Error(errno_to_i32(errno));
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
                    drive_tcp_loopback_after_sendto(&socket, sent);
                    tx_reactor::yield_now().await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Continue { progress } => {
                let sent = progress.bytes();
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    drive_tcp_loopback_after_sendto(&socket, sent);
                    tx_reactor::yield_now().await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Yield { progress, shape } => {
                let sent = progress.bytes();
                total += sent;
                if sent >= remaining.len() {
                    drive_tcp_loopback_after_sendto(&socket, sent);
                    tx_reactor::yield_now().await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
                if total > 0 {
                    drive_tcp_loopback_after_sendto(&socket, total);
                    tx_reactor::yield_now().await;
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
                    drive_tcp_loopback_after_sendto(&socket, total);
                    tx_reactor::yield_now().await;
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
    if len == 0 {
        return SyscallResult::Return(0);
    }

    let mut staging = alloc::vec![0; len.min(TTY_WRITE_MAX_INLINE)];
    loop {
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
    let _control_ignored = header.control != 0 && header.controllen != 0;
    let mut flags = match SendRecvFlags::validate(args[2] as i32) {
        Ok(flags) => flags,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if file.flags().nonblocking {
        flags |= SendRecvFlags::MSG_DONTWAIT;
    }

    let dst = if header.name != 0 {
        match read_sockaddr_in(ctx, header.name, header.namelen as u64) {
            Ok(addr) => Some(addr.as_ip_endpoint()),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        None
    };

    let iovecs = match read_iovecs(ctx, header.iov, header.iovlen) {
        Ok(iovecs) => iovecs,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let total_len = match iov_total_len(&iovecs) {
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
            step_send_to_kernel_bytes(&socket, dst, remaining, flags, &guard)
        };
        match outcome {
            StepOutcome::Done(sent) => {
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Continue { progress } => {
                let sent = progress.bytes();
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Yield { progress, shape } => {
                let sent = progress.bytes();
                total += sent;
                if sent >= remaining.len() {
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
                if total > 0 {
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
    let total_len = match iov_total_len(&iovecs) {
        Ok(total_len) => total_len,
        Err(errno_value) => return SyscallResult::Error(errno_value),
    };
    if let Err(errno) = write_msghdr_flags(ctx, args[1], 0) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    if let Err(errno) = write_msghdr_controllen(ctx, args[1], 0) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    if total_len == 0 {
        return SyscallResult::Return(0);
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
        (IPPROTO_IP, IP_RECVERR) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.ip.recv_err = on);
            Ok(())
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
            let _ = match read_sockopt_positive_usize(ctx, optval, optlen) {
                Ok(size) => size,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            Ok(())
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
        (IPPROTO_TCP, TCP_MAXSEG) => write_sockopt_i32(ctx, optval, optlen_ptr, 1460),
        (IPPROTO_TCP, TCP_INFO) => write_sockopt_bytes(ctx, optval, optlen_ptr, &[0u8; 104]),
        (IPPROTO_TCP, TCP_CONGESTION) => write_sockopt_bytes(ctx, optval, optlen_ptr, b"reno\0"),
        _ => Err(Errno::ENOPROTOOPT),
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

pub(super) fn maybe_close_socket_file(file: &Cap<OpenFile>) {
    if file.retain_count() > 2 {
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
    match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Socket { identity },
        } => Ok(identity.clone()),
        _ => Err(Errno::ENOTSOCK),
    }
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

fn read_iovecs<'a>(
    ctx: &SyscallCtx<'a>,
    iov_ptr: u64,
    iovlen: u64,
) -> Result<alloc::vec::Vec<UserIovec>, Errno> {
    if iovlen > MAX_MSG_IOV {
        return Err(Errno::EINVAL);
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
    let mut total = 0usize;
    for iov in iovecs {
        total = total.checked_add(iov.len).ok_or(EINVAL_VALUE)?;
        if total > TTY_WRITE_MAX_INLINE {
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

fn socket_is_tcp_connecting(socket: &Cap<SocketIdentity>) -> bool {
    socket.acquire_operational().is_some_and(|payload| {
        matches!(
            payload.protocol_snapshot(),
            SocketProtocol::Tcp(TcpState::Connecting { .. })
        )
    })
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
    let bytes = core::cmp::min(optlen as usize, value.len());
    bootstrap_write_user(&ctx.aspace, optlen_ptr, bytes as u32)?;
    bootstrap_copy_to_user(&ctx.aspace, optval, &value[..bytes])
}

fn socket_type_i32(socket: &Cap<SocketIdentity>) -> i32 {
    match socket.kind {
        SocketKind::Tcp => 1,
        SocketKind::Udp => 2,
        SocketKind::RawIcmp => 3,
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

fn maybe_raise_sigpipe<'a>(ctx: &SyscallCtx<'a>, errno: Errno, flags: SendRecvFlags) {
    if errno == Errno::EPIPE && !flags.contains(SendRecvFlags::MSG_NOSIGNAL) {
        let _ = step_kill_process(&ctx.process, Signum::SIGPIPE, None);
    }
}
