//! Socket syscall shims for the N39 fdtable integration slice.
//!
//! The syscall layer owns Linux ABI decoding and fdtable installation.
//! Socket state transitions stay in `tx_subsystems::net::execution`
//! steps so the identity/payload split and wait-carrier discipline stay
//! in the network subsystem.

use super::*;

use tx_services::time::{ClockRead, TimekeeperClock};
use tx_substrate::step::{NoProgress, StepOutcome, YieldShape};
use tx_subsystems::net::protocol::loopback_iface;
use tx_subsystems::net::{
    net_namespace_payload_from_file, net_namespace_payloads_snapshot, netlink_netfilter_recv,
    netlink_netfilter_send, netlink_route_recv, netlink_route_recv_packet,
    netlink_route_send_with_netns_resolvers, netlink_xfrm_recv, netlink_xfrm_send, require_net_raw,
    socket_open_file_from_identity, step_accept, step_bind, step_connect, step_listen,
    step_poll_ready, step_poll_wait_token, step_process_loopback_udp, step_recv_kernel_bytes,
    step_sctp_peeloff, step_sctp_shutdown_assoc, step_send_sctp_message, step_send_sctp_seqpacket,
    step_send_to_kernel_bytes, step_send_to_unix_path_kernel_bytes,
    step_send_udp_loopback_kernel_bytes, step_shutdown, step_socket_open_file_in_namespace,
    step_tcp_loopback_handshake, step_tcp_loopback_transfer, step_unix_socketpair_connect,
    AddressFamily, ConnectionKey, IpEndpoint, Ipv4Address, Ipv4MulticastGroup, Ipv6Address,
    KernelSockAddr, LingerOption, NetNamespacePayload, PollMask, RecvWireSet, SendRecvFlags,
    SockAddrIn, SockAddrIn6, SockAddrLl, SockShutdownCmd, SocketHandleFlags, SocketIdentity,
    SocketKind, SocketOperationalEvidence, SocketProtocol, SocketType, TcpState, TcpTlsUlpState,
    UdpInner, UnixDatagramState, UnixPeerCred, UnixSocketPath, UnixStreamState, ValidSocketType,
    VIRTIO_NET_DEFAULT_MTU,
};
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

/// Shared rotation offset for ephemeral-port allocation (P2-S5). Every
/// scan (connect autobind and bind(port=0) alike) starts one slot past
/// the previous scan's start, so back-to-back connects do not re-pick
/// the port a just-closed connection used — the peer (e.g. QEMU slirp)
/// may still hold that tuple in TIME_WAIT-ish state.
static NEXT_EPHEMERAL_PORT_OFFSET: core::sync::atomic::AtomicU16 =
    core::sync::atomic::AtomicU16::new(0);

pub(super) fn ephemeral_port_candidates() -> impl Iterator<Item = u16> {
    const LEN: u16 = EPHEMERAL_PORT_END - EPHEMERAL_PORT_START;
    let start =
        NEXT_EPHEMERAL_PORT_OFFSET.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % LEN;
    (0..LEN).map(move |i| EPHEMERAL_PORT_START + (start + i) % LEN)
}
const IOVEC_BYTES: u64 = 16;
const MSGHDR_BYTES: u64 = 56;
const MSGHDR_NAMELEN_OFFSET: u64 = 8;
const MSGHDR_CONTROLLEN_OFFSET: u64 = 40;
const MSGHDR_FLAGS_OFFSET: u64 = 48;
const MMSGHDR_BYTES: u64 = 64;
const MMSGHDR_LEN_OFFSET: u64 = MSGHDR_BYTES;
const CMSGHDR_BYTES: u64 = 16;
const MSG_CTRUNC_BITS: u32 = 0x08;
/// `MSG_EOR` — recvmsg delivered a complete record (SCTP message boundary).
const MSG_EOR_BITS: u32 = 0x80;
/// `MSG_NOTIFICATION` — recvmsg delivered an SCTP control notification.
const MSG_NOTIFICATION_BITS: u32 = 0x8000;
/// `SCTP_SNDRCV` cmsg type (ancillary sctp_sndrcvinfo at IPPROTO_SCTP level).
const SCTP_SNDRCV_CMSG: i32 = 1;
/// `struct sctp_sndrcvinfo` size: stream/ssn/flags (+pad) + ppid/context/ttl/
/// tsn/cumtsn/assoc_id = 32 bytes.
const SCTP_SNDRCVINFO_BYTES: usize = 32;
/// sinfo_flags bits that request association teardown — invalid on a 1-to-1
/// (TCP-style) socket: SCTP_ABORT (0x4) and SCTP_EOF (MSG_FIN, 0x200).
const SCTP_SINFO_TEARDOWN_FLAGS: u16 = 0x0004 | 0x0200;

/// Default association fragmentation point when SCTP_MAXSEG is unset: the loopback
/// MTU (65536) minus SCTP common + DATA chunk overhead. With SCTP_DISABLE_FRAGMENTS
/// a single message larger than this is rejected with EMSGSIZE instead of split.
const SCTP_DEFAULT_FRAG_POINT: usize = 65515;
const SCM_RIGHTS: i32 = 1;
const MAX_MSG_IOV: u64 = 1024;
const SOCKET_MSG_MAX_BYTES: usize = 1024 * 1024;
/// Linux floor for SO_SNDBUF/SO_RCVBUF after the 2x bookkeeping multiplier
/// (`SOCK_MIN_SNDBUF`/`SOCK_MIN_RCVBUF` ≈ `2048 + sizeof(struct sk_buff)`).
const SOCK_MIN_BUF: usize = 2304;
const NETLINK_RECVMSG_MAX: usize = 1024 * 1024;
const NETLINK_INLINE_SEND_MAX: usize = 256;
const IPT_GETINFO_BYTES: usize = 84;
const IPT_GET_ENTRIES_EMPTY_BYTES: usize = 36;
const IPT_REPLACE_HEADER_BYTES: usize = 96;
const IPT_REPLACE_SIZE_OFFSET: usize = 40;
const IN_ADDR_BYTES: u32 = 4;
const IP_MREQ_BYTES: u32 = 8;
const IP_MREQN_BYTES: u32 = 12;
const GROUP_REQ_BYTES: u32 = 136;
const GROUP_REQ_GROUP_OFFSET: usize = 8;
const IPV4_TCP_HEADER_BYTES: u16 = 40;
const IFNAMSIZ: usize = 16;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const PACKET_HOST: u8 = 0;
const ETH_P_IP: u16 = 0x0800;
const ETH_P_ARP: u16 = 0x0806;
const ARPOP_REQUEST: u16 = 1;
const ARPOP_REPLY: u16 = 2;
const ARP_ETH_IPV4_PACKET_BYTES: usize = 28;

pub(super) fn is_netlink_socket_kind(kind: SocketKind) -> bool {
    matches!(
        kind,
        SocketKind::NetlinkRoute | SocketKind::NetlinkXfrm | SocketKind::NetlinkNetfilter
    )
}

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
                (AddressFamily::Inet | AddressFamily::Inet6, SocketType::Raw)
            ))
}

pub(super) async fn sys_socketpair<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
    if let Err(errno) = validate_user_range_wait(ctx, sv, 8, UserAccessKind::Write).await {
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
    let mut fd_bytes = [0u8; 8];
    fd_bytes[..4].copy_from_slice(&(first_fd as i32).to_le_bytes());
    fd_bytes[4..].copy_from_slice(&(second_fd as i32).to_le_bytes());
    match bootstrap_copy_to_user_wait(&ctx.aspace, sv, &fd_bytes).await {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => {
            let _ = ctx.process.set_fd(first_fd, None);
            let _ = ctx.process.set_fd(second_fd, None);
            SyscallResult::Error(errno_to_i32(errno))
        }
    }
}

pub(super) fn sys_bind<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if is_netlink_socket_kind(socket.kind) {
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

pub(super) fn sys_accept<'a, P: 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a
where
    TimekeeperClock<P>: ClockRead,
{
    accept_entry::<P>(args, ctx)
}

async fn accept_entry<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    accept_impl::<P>(args[0] as i32, args[1], args[2], 0, ctx).await
}

pub(super) fn sys_accept4<'a, P: 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a
where
    TimekeeperClock<P>: ClockRead,
{
    accept4_entry::<P>(args, ctx)
}

async fn accept4_entry<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let flags = args[3] as u32;
    if flags & !ACCEPT4_KNOWN_FLAGS != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    accept_impl::<P>(args[0] as i32, args[1], args[2], flags, ctx).await
}

async fn accept_impl<'a, P>(
    fd: i32,
    addr_ptr: u64,
    addrlen_ptr: u64,
    flags: u32,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
                let write_addr = if addr_ptr == 0 {
                    // accept(fd, NULL, addrlen): Linux returns no peer address and
                    // leaves addrlen untouched (it is not faulted even if non-NULL).
                    Ok(())
                } else if listener.kind == SocketKind::UnixStream {
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
                        wait_on_socket_or_itimer::<P>(future, ctx).await,
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
            // Linux SCTP (sctp_verify_addr) rejects an unsupported address
            // family on connect() with EINVAL, unlike TCP's EAFNOSUPPORT.
            Err(Errno::EAFNOSUPPORT) if socket.kind == SocketKind::Sctp => {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
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
                // Linux reports a fresh non-blocking SCTP connect() as
                // EINPROGRESS even though our loopback association is set up
                // synchronously; the socket is in fact already connected, so
                // subsequent accept/recv on the peer proceed normally.
                if nonblocking && socket.kind == SocketKind::Sctp && !was_connecting {
                    return SyscallResult::Error(errno_to_i32(Errno::EINPROGRESS));
                }
                return SyscallResult::Return(0);
            }
            StepOutcome::Yield { shape, .. } => {
                let connected = match drive_tcp_loopback_after_connect(&socket) {
                    Ok(connected) => connected || socket_is_tcp_connected(&socket),
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

/// `sctp_connectx()` (default symver `sctp_connectx3`) is implemented in lksctp as
/// `getsockopt(SCTP_SOCKOPT_CONNECTX3)` carrying
/// `struct sctp_getaddrs_old { sctp_assoc_t assoc_id; int addr_num; struct sockaddr
/// *addrs; }` (16 bytes on 64-bit): `addr_num` is the byte length of the packed
/// address array pointed at by `addrs`. We connect to the first address (every
/// candidate is loopback) and return the new association id in `assoc_id`. An empty
/// address list (`addr_num == 0`) is EINVAL.
fn sctp_connectx3(
    ctx: &SyscallCtx<'_>,
    socket: &Cap<SocketIdentity>,
    optval: u64,
    optlen_ptr: u64,
    nonblocking: bool,
) -> Result<(), Errno> {
    let mut param = [0u8; 16];
    bootstrap_copy_from_user(&ctx.aspace, &mut param, optval).map_err(|_| Errno::EFAULT)?;
    let addr_num = i32::from_le_bytes([param[4], param[5], param[6], param[7]]);
    let addrs_ptr = u64::from_le_bytes([
        param[8], param[9], param[10], param[11], param[12], param[13], param[14], param[15],
    ]);
    if addr_num <= 0 {
        return Err(Errno::EINVAL);
    }
    // Parse the first sockaddr from the packed list (its family sizes it).
    let remote = match read_sockaddr_in(ctx, addrs_ptr, addr_num as u64) {
        Ok(remote) => remote,
        // SCTP maps an unsupported family to EINVAL (sctp_verify_addr).
        Err(Errno::EAFNOSUPPORT) => return Err(Errno::EINVAL),
        Err(errno) => return Err(errno),
    };
    let remote = connect_sockaddr_for_local_stack(socket.kind, remote);
    maybe_autobind_connect_client(socket, remote)?;
    // SCTP loopback associations are established synchronously by step_connect.
    let outcome = {
        let guard = tx_substrate::epoch::guard();
        step_connect(socket, remote, &guard)
    };
    match outcome {
        StepOutcome::Done(()) => {}
        StepOutcome::Err(errno) => return Err(errno),
        _ => return Err(Errno::EINPROGRESS),
    }
    // Report the association id so it matches the COMM_UP notification's
    // sac_assoc_id (1-to-many). 1-to-1 sockets keep no peer list → 0.
    let assoc_id = socket
        .acquire_operational()
        .and_then(|p| p.sctp_assoc_id_for_peer(remote.as_ip_endpoint()))
        .unwrap_or(0);
    param[0..4].copy_from_slice(&assoc_id.to_le_bytes());
    write_sockopt_bytes(ctx, optval, optlen_ptr, &param)?;
    // A non-blocking connectx reports EINPROGRESS even though our loopback
    // association is already established; the assoc id is still written back so
    // sctp_connectx3() returns it to the caller.
    if nonblocking {
        Err(Errno::EINPROGRESS)
    } else {
        Ok(())
    }
}

mod helpers;
pub(super) use helpers::drive_loopback_pending;
use helpers::*;
pub(crate) use helpers::{
    socket_identity_from_file, socket_poll_mask_from_file, socket_poll_wait_token_from_file,
    unix_pathname_key,
};

pub(super) fn sys_getsockname<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let socket = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok((_, socket)) => socket,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    if is_netlink_socket_kind(socket.kind) {
        return match write_sockaddr_nl(ctx, args[1], args[2]) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }
    if socket.kind == SocketKind::Packet {
        let Some(payload) = socket.acquire_operational() else {
            return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN));
        };
        let Some(mut sockaddr) = payload.packet_sockaddr() else {
            return SyscallResult::Error(EINVAL_VALUE);
        };
        if sockaddr.ifindex > 0 {
            let ifindex = sockaddr.ifindex as u32;
            if let Some(link) = payload
                .net_namespace()
                .link_snapshot()
                .into_iter()
                .find(|link| link.ifindex == ifindex)
            {
                sockaddr.hatype = if link.is_loopback {
                    ARPHRD_LOOPBACK
                } else {
                    ARPHRD_ETHER
                };
                sockaddr.pkttype = PACKET_HOST;
                if let Some(mac) = link.mac {
                    let octets = mac.octets();
                    sockaddr.halen = octets.len() as u8;
                    sockaddr.addr[..octets.len()].copy_from_slice(&octets);
                }
            }
        }
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

pub(super) fn dispatch_netlink_send(
    ctx: &SyscallCtx<'_>,
    socket: &Cap<SocketIdentity>,
    bytes: &[u8],
) -> Result<usize, Errno> {
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
    match socket.kind {
        SocketKind::NetlinkRoute => netlink_route_send_with_netns_resolvers(
            socket,
            bytes,
            ctx.cred(),
            &mut resolve_netns_fd,
            &mut resolve_netns_pid,
        ),
        SocketKind::NetlinkXfrm => netlink_xfrm_send(socket, bytes, ctx.cred()),
        SocketKind::NetlinkNetfilter => netlink_netfilter_send(socket, bytes, ctx.cred()),
        _ => unreachable!(),
    }
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

    if is_netlink_socket_kind(socket.kind) {
        if args[4] != 0 {
            if let Err(errno) = read_sockaddr_nl(ctx, args[4], args[5]) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        if len <= NETLINK_INLINE_SEND_MAX {
            let mut inline = [0u8; NETLINK_INLINE_SEND_MAX];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut inline[..len], args[1]) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let result = dispatch_netlink_send(ctx, &socket, &inline[..len]);
            return match result {
                Ok(sent) => SyscallResult::Return(sent as i64),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            };
        }

        let mut bytes = alloc::vec![0; len];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, args[1]) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let result = dispatch_netlink_send(ctx, &socket, &bytes);
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
        let mut bytes = alloc::vec![0; len];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, args[1]) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        maybe_queue_packet_arp_reply(ctx, &socket, &payload, &netns, sockaddr, &bytes);
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
    // SCTP 1-to-1: sendto with a destination on a socket that has no association
    // yet implicitly establishes one (Linux SCTP implicit association), then
    // sends on it. A socket that is already connected ignores the destination.
    if socket.kind == SocketKind::Sctp && args[4] != 0 && socket_peer_endpoint(&socket).is_err() {
        // Validate the user send buffer before the implicit association: Linux
        // faults on the buffer (EFAULT) before reporting protocol-state errors,
        // so sendto(fd, NULL, ...) must return EFAULT, not ECONNREFUSED from the
        // implicit connect (sendto02).
        if len > 0 {
            if let Err(errno) = validate_user_range(ctx, args[1], len, UserAccessKind::Read) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        let remote = match read_sockaddr_in(ctx, args[4], args[5]) {
            Ok(addr) => connect_sockaddr_for_local_stack(socket.kind, addr),
            Err(Errno::EAFNOSUPPORT) => return SyscallResult::Error(errno_to_i32(Errno::EINVAL)),
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        if let Err(errno) = maybe_autobind_connect_client(&socket, remote) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_connect(&socket, remote, &guard)
        };
        match outcome {
            StepOutcome::Done(()) => {}
            StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            _ => return SyscallResult::Error(EIO_VALUE),
        }
    }
    if let Err(errno) = maybe_autobind_udp_sendto(&socket, dst) {
        return SyscallResult::Error(errno_to_i32(errno));
    }

    if raw_icmp_hdrincl_enabled(&socket) {
        if let Err(errno) = validate_user_range(ctx, args[1], len, UserAccessKind::Read) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        return SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP));
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
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Continue { progress } => {
                let sent = progress.bytes();
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Yield { progress, shape } => {
                let sent = progress.bytes();
                total += sent;
                if sent >= remaining.len() {
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
                if total > 0 {
                    finish_sendto_progress(ctx, &socket, total, flags).await;
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
                    finish_sendto_progress(ctx, &socket, total, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                maybe_raise_sigpipe(ctx, errno, flags);
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

pub(super) fn sys_recvfrom<'a, P: 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a
where
    TimekeeperClock<P>: ClockRead,
{
    recvfrom_impl::<P>(args, ctx)
}

async fn recvfrom_impl<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
    let is_netlink_socket = is_netlink_socket_kind(socket.kind);
    if !is_netlink_socket {
        if let Some(errno) = recv_special_flags_errno(flags) {
            return SyscallResult::Error(errno);
        }
        if let Err(errno) = validate_recvfrom_addrlen(ctx, args[5]) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
    if len == 0
        && socket.kind != SocketKind::Udp
        && !(is_netlink_socket && flags.contains(SendRecvFlags::MSG_TRUNC))
    {
        return SyscallResult::Return(0);
    }

    if is_netlink_socket {
        if socket.kind == SocketKind::NetlinkRoute {
            let response = match netlink_route_recv_packet(&socket, flags) {
                Ok(response) => response,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let copied = core::cmp::min(len.min(NETLINK_RECVMSG_MAX), response.len());
            if copied > 0 {
                if let Err(errno) = bootstrap_copy_to_user_wait(
                    &ctx.aspace,
                    args[1],
                    &response.as_slice()[..copied],
                )
                .await
                {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
            }
            if let Err(errno) = write_sockaddr_nl(ctx, args[4], args[5]) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let reported = if flags.contains(SendRecvFlags::MSG_TRUNC) {
                response.len()
            } else {
                copied
            };
            return SyscallResult::Return(reported as i64);
        }

        let mut staging = alloc::vec![0; len.min(NETLINK_RECVMSG_MAX)];
        let recv_result = match socket.kind {
            SocketKind::NetlinkXfrm => netlink_xfrm_recv(&socket, &mut staging, flags),
            SocketKind::NetlinkNetfilter => netlink_netfilter_recv(&socket, &mut staging, flags),
            _ => unreachable!(),
        };
        let recv = match recv_result {
            Ok(recv) => recv,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let copied = core::cmp::min(recv, staging.len());
        if copied > 0 {
            if let Err(errno) =
                bootstrap_copy_to_user_wait(&ctx.aspace, args[1], &staging[..copied]).await
            {
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
                // SCTP: recv on a socket with no data and no established
                // association (never connected, or locally shut down) reports
                // ENOTCONN rather than blocking/EAGAIN.
                if socket.kind == SocketKind::Sctp && sctp_recv_disconnected(&socket) {
                    return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN));
                }
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
                let Some(future) = wait_source::wait_on_registered_source_id(
                    wait_token.source_id(),
                    wait_token.interest(),
                ) else {
                    return SyscallResult::Error(EIO_VALUE);
                };
                if matches!(
                    wait_on_socket_or_itimer::<P>(future, ctx).await,
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

        let staging_len = recv_staging_len(&socket, len);
        // Validate the destination buffer is writable BEFORE consuming the
        // message, so a bad buffer (e.g. -1) returns EFAULT without dropping the
        // queued message (a later recv must still see it).
        if staging_len > 0 && !flags.contains(SendRecvFlags::MSG_PEEK) {
            if let Err(errno) =
                validate_user_range_wait(ctx, args[1], staging_len, UserAccessKind::Write).await
            {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
        let mut staging = alloc::vec![0; staging_len];
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            step_recv_kernel_bytes(&socket, &mut staging, flags, &guard)
        };
        match outcome {
            StepOutcome::Done(recv) => {
                if recv.bytes > 0 {
                    if let Err(errno) =
                        bootstrap_copy_to_user_wait(&ctx.aspace, args[1], &staging[..recv.bytes])
                            .await
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
                } else if let Some(source) = recv.packet_source {
                    if let Err(errno) = write_sockaddr_ll(ctx, args[4], args[5], source) {
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
                        wait_on_socket_or_itimer::<P>(future, ctx).await,
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

fn maybe_queue_packet_arp_reply(
    ctx: &SyscallCtx<'_>,
    socket: &Cap<SocketIdentity>,
    payload: &SocketOperationalEvidence,
    netns: &NetNamespacePayload,
    sockaddr: SockAddrLl,
    bytes: &[u8],
) {
    if sockaddr.protocol != ETH_P_ARP || bytes.len() < ARP_ETH_IPV4_PACKET_BYTES {
        return;
    }
    if u16::from_be_bytes([bytes[0], bytes[1]]) != ARPHRD_ETHER
        || u16::from_be_bytes([bytes[2], bytes[3]]) != ETH_P_IP
        || bytes[4] != 6
        || bytes[5] != 4
        || u16::from_be_bytes([bytes[6], bytes[7]]) != ARPOP_REQUEST
    {
        return;
    }

    let sender_mac = match <[u8; 6]>::try_from(&bytes[8..14]) {
        Ok(mac) => mac,
        Err(_) => return,
    };
    let sender_ip = match <[u8; 4]>::try_from(&bytes[14..18]) {
        Ok(ip) => ip,
        Err(_) => return,
    };
    let target_ip = match <[u8; 4]>::try_from(&bytes[24..28]) {
        Ok(ip) => Ipv4Address::new(ip),
        Err(_) => return,
    };

    let Some(target_mac) = packet_arp_target_mac(netns, target_ip) else {
        return;
    };
    let target_ip = target_ip.octets();

    let mut reply = alloc::vec![0u8; ARP_ETH_IPV4_PACKET_BYTES];
    reply[0..2].copy_from_slice(&ARPHRD_ETHER.to_be_bytes());
    reply[2..4].copy_from_slice(&ETH_P_IP.to_be_bytes());
    reply[4] = 6;
    reply[5] = 4;
    reply[6..8].copy_from_slice(&ARPOP_REPLY.to_be_bytes());
    reply[8..14].copy_from_slice(&target_mac);
    reply[14..18].copy_from_slice(&target_ip);
    reply[18..24].copy_from_slice(&sender_mac);
    reply[24..28].copy_from_slice(&sender_ip);

    let mut source_addr = [0u8; 8];
    source_addr[..target_mac.len()].copy_from_slice(&target_mac);
    let source = SockAddrLl::with_link_layer_addr(
        ETH_P_ARP,
        sockaddr.ifindex,
        ARPHRD_ETHER,
        PACKET_HOST,
        source_addr,
        target_mac.len() as u8,
    );
    if payload.record_packet_frame(source, reply).unwrap_or(false) {
        socket.readiness.fire_recv(RecvWireSet::HAS_DATA);
    }
}

fn packet_arp_target_mac(netns: &NetNamespacePayload, target_ip: Ipv4Address) -> Option<[u8; 6]> {
    find_packet_arp_target_mac_in_netns(netns, target_ip).or_else(|| {
        net_namespace_payloads_snapshot()
            .into_iter()
            .find_map(|candidate| find_packet_arp_target_mac_in_netns(&candidate, target_ip))
    })
}

fn find_packet_arp_target_mac_in_netns(
    netns: &NetNamespacePayload,
    target_ip: Ipv4Address,
) -> Option<[u8; 6]> {
    netns.link_snapshot().into_iter().find_map(|link| {
        if link.ipv4_addr == Some(target_ip) && !link.is_loopback {
            link.mac.map(|mac| mac.octets())
        } else {
            None
        }
    })
}

/// Parsed sctp_sndrcvinfo ancillary data from a sendmsg control message.
#[derive(Clone, Copy, Default)]
struct SctpSndInfo {
    stream: u16,
    flags: u16,
    ppid: u32,
    /// sinfo_timetolive (ms): a non-zero value marks a partially-reliable
    /// (PR-SCTP timed) message that is abandoned if it cannot be delivered in time.
    ttl: u32,
    assoc_id: u32,
}

/// Parse an SCTP_SNDRCV control message (sctp_sndrcvinfo) from a sendmsg msghdr.
/// Only the first cmsg is inspected, which is what the SCTP API tests build.
fn parse_sctp_sndrcvinfo(ctx: &SyscallCtx<'_>, header: &UserMsghdr) -> Option<SctpSndInfo> {
    if header.control == 0 {
        return None;
    }
    const CMSG_HDR: usize = 16; // size_t cmsg_len + int level + int type
    let total = header.controllen as usize;
    if total < CMSG_HDR + SCTP_SNDRCVINFO_BYTES {
        return None;
    }
    let mut buf = [0u8; CMSG_HDR + SCTP_SNDRCVINFO_BYTES];
    bootstrap_copy_from_user(&ctx.aspace, &mut buf, header.control).ok()?;
    let level = i32::from_le_bytes(buf[8..12].try_into().ok()?);
    let ctype = i32::from_le_bytes(buf[12..16].try_into().ok()?);
    if level != SOL_SCTP || ctype != SCTP_SNDRCV_CMSG {
        return None;
    }
    // sctp_sndrcvinfo: sinfo_stream @0, sinfo_flags @4, sinfo_ppid @8,
    // sinfo_timetolive @16, sinfo_assoc_id @28.
    Some(SctpSndInfo {
        stream: u16::from_le_bytes(buf[CMSG_HDR..CMSG_HDR + 2].try_into().ok()?),
        flags: u16::from_le_bytes(buf[CMSG_HDR + 4..CMSG_HDR + 6].try_into().ok()?),
        ppid: u32::from_le_bytes(buf[CMSG_HDR + 8..CMSG_HDR + 12].try_into().ok()?),
        ttl: u32::from_le_bytes(buf[CMSG_HDR + 16..CMSG_HDR + 20].try_into().ok()?),
        assoc_id: u32::from_le_bytes(buf[CMSG_HDR + 28..CMSG_HDR + 32].try_into().ok()?),
    })
}

/// Write a `struct sctp_getaddrs { assoc_id; addr_num; addrs[] }` reply for
/// SCTP_GET_LOCAL_ADDRS / SCTP_GET_PEER_ADDRS with a single address. addr_num is
/// at offset 4, the sockaddr starts at offset 8 (where libc memmoves it from).
fn write_sctp_getaddrs(
    ctx: &SyscallCtx<'_>,
    optval: u64,
    optlen_ptr: u64,
    endpoint: IpEndpoint,
) -> Result<(), Errno> {
    let mut buf = [0u8; 8 + SOCKADDR_IN6_BYTES as usize];
    buf[4..8].copy_from_slice(&1u32.to_le_bytes()); // addr_num = 1
    let total = if endpoint.family == AddressFamily::Inet6 {
        // sockaddr_in6 @8: family/port/flowinfo/addr@16/scope = 28 bytes.
        buf[8..10].copy_from_slice(&AF_INET6.to_le_bytes());
        buf[10..12].copy_from_slice(&endpoint.port.to_be_bytes());
        buf[16..32].copy_from_slice(&endpoint.addr6.octets());
        8 + SOCKADDR_IN6_BYTES as usize
    } else {
        // sockaddr_in @8: family/port/addr = 16 bytes.
        buf[8..10].copy_from_slice(&AF_INET.to_le_bytes());
        buf[10..12].copy_from_slice(&endpoint.port.to_be_bytes());
        buf[12..16].copy_from_slice(&endpoint.addr.octets());
        8 + SOCKADDR_IN_BYTES as usize
    };
    write_sockopt_bytes(ctx, optval, optlen_ptr, &buf[..total])
}

/// Write a `struct sctp_getaddrs { assoc_id; addr_num; addrs[] }` reply with N
/// addresses (multi-homing): addr_num @4 = N, the packed sockaddrs follow @8.
/// `sctp_getpaddrs()` returns addr_num, so every peer address must be present.
fn write_sctp_getaddrs_multi(
    ctx: &SyscallCtx<'_>,
    optval: u64,
    optlen_ptr: u64,
    endpoints: &[IpEndpoint],
) -> Result<(), Errno> {
    let mut buf = alloc::vec![0u8; 8];
    buf[4..8].copy_from_slice(&(endpoints.len() as u32).to_le_bytes()); // addr_num
    for ep in endpoints {
        if ep.family == AddressFamily::Inet6 {
            let mut sa = [0u8; SOCKADDR_IN6_BYTES as usize];
            sa[0..2].copy_from_slice(&AF_INET6.to_le_bytes());
            sa[2..4].copy_from_slice(&ep.port.to_be_bytes());
            sa[8..24].copy_from_slice(&ep.addr6.octets());
            buf.extend_from_slice(&sa);
        } else {
            let mut sa = [0u8; SOCKADDR_IN_BYTES as usize];
            sa[0..2].copy_from_slice(&AF_INET.to_le_bytes());
            sa[2..4].copy_from_slice(&ep.port.to_be_bytes());
            sa[4..8].copy_from_slice(&ep.addr.octets());
            buf.extend_from_slice(&sa);
        }
    }
    write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
}

/// Write an SCTP_SNDRCV control message (sctp_sndrcvinfo with `stream`/`ppid`)
/// into a recvmsg msghdr's control buffer and set msg_controllen. If there is no
/// room, the control data is omitted (controllen left at 0).
fn write_sctp_sndrcv_cmsg(
    ctx: &SyscallCtx<'_>,
    msghdr_ptr: u64,
    header: UserMsghdr,
    stream: u16,
    ppid: u32,
) -> Result<bool, Errno> {
    const CMSG_HDR: usize = 16;
    const TOTAL: usize = CMSG_HDR + SCTP_SNDRCVINFO_BYTES; // CMSG_LEN(sizeof sndrcvinfo) = 48
    if header.control == 0 || (header.controllen as usize) < TOTAL {
        // The sndrcvinfo cmsg does not fit: the data is still delivered but the
        // control message is truncated (the caller sets MSG_CTRUNC).
        return Ok(true);
    }
    let mut buf = [0u8; TOTAL];
    buf[0..8].copy_from_slice(&(TOTAL as u64).to_le_bytes()); // cmsg_len
    buf[8..12].copy_from_slice(&SOL_SCTP.to_le_bytes()); // cmsg_level = IPPROTO_SCTP
    buf[12..16].copy_from_slice(&SCTP_SNDRCV_CMSG.to_le_bytes()); // cmsg_type = SCTP_SNDRCV
    buf[CMSG_HDR..CMSG_HDR + 2].copy_from_slice(&stream.to_le_bytes()); // sinfo_stream
    buf[CMSG_HDR + 8..CMSG_HDR + 12].copy_from_slice(&ppid.to_le_bytes()); // sinfo_ppid
    bootstrap_copy_to_user(&ctx.aspace, header.control, &buf)?;
    write_msghdr_controllen(ctx, msghdr_ptr, TOTAL as u64)?;
    Ok(false)
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

    if is_netlink_socket_kind(socket.kind) {
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
        if total_len <= NETLINK_INLINE_SEND_MAX {
            let mut inline = [0u8; NETLINK_INLINE_SEND_MAX];
            let mut offset = 0usize;
            for iov in &iovecs {
                if iov.len == 0 {
                    continue;
                }
                let end = offset + iov.len;
                if let Err(errno) =
                    bootstrap_copy_from_user(&ctx.aspace, &mut inline[offset..end], iov.base)
                {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                offset = end;
            }
            let result = dispatch_netlink_send(ctx, &socket, &inline[..total_len]);
            return match result {
                Ok(sent) => SyscallResult::Return(sent as i64),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            };
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
        let result = dispatch_netlink_send(ctx, &socket, &bytes);
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
    let total_len = match if is_netlink_socket_kind(socket.kind) {
        iov_total_len_with_limit(&iovecs, NETLINK_RECVMSG_MAX)
    } else {
        iov_total_len_with_limit(&iovecs, SOCKET_MSG_MAX_BYTES)
    } {
        Ok(total_len) => total_len,
        Err(errno_value) => return SyscallResult::Error(errno_value),
    };

    // SCTP: parse the SCTP_SNDRCV ancillary data once. A teardown flag
    // (SCTP_EOF/SCTP_ABORT) is invalid on a 1-to-1 (TCP-style) socket — reject
    // with EINVAL even for an empty (iov-less) message.
    let sctp_info = if socket.kind == SocketKind::Sctp {
        parse_sctp_sndrcvinfo(ctx, &header)
    } else {
        None
    };
    if let Some(info) = sctp_info {
        let sock_type = socket
            .acquire_operational()
            .map(|p| p.with_options(|o| o.socket.sock_type));
        let teardown = info.flags & SCTP_SINFO_TEARDOWN_FLAGS != 0;
        if sock_type == Some(SocketType::Stream) && teardown {
            return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
        }
        // 1-to-many: SCTP_EOF/SCTP_ABORT gracefully shuts down the named
        // association (no payload is sent). Handle it before the empty-message
        // early return below.
        if sock_type == Some(SocketType::SeqPacket) && teardown {
            let guard = tx_substrate::epoch::guard();
            // SCTP_ABORT (0x4) is an ungraceful teardown (peer gets COMM_LOST);
            // SCTP_EOF (0x200) is graceful (peer gets SHUTDOWN_EVENT/COMP).
            let abort = info.flags & 0x0004 != 0;
            return match step_sctp_shutdown_assoc(&socket, info.assoc_id, abort, &guard) {
                StepOutcome::Done(()) => SyscallResult::Return(0),
                StepOutcome::Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                _ => SyscallResult::Error(errno_to_i32(Errno::EIO)),
            };
        }
    }

    if total_len == 0 && socket.kind != SocketKind::Udp {
        return SyscallResult::Return(0);
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

    // SCTP carries one message per sendmsg with its sctp_sndrcvinfo (stream/ppid)
    // from the SCTP_SNDRCV control message; route it through the message-oriented
    // send so the peer's recvmsg can echo the ancillary data back.
    if socket.kind == SocketKind::Sctp {
        let info = sctp_info.unwrap_or_default();
        // SCTP_DISABLE_FRAGMENTS: a message larger than the association
        // fragmentation point is rejected with EMSGSIZE rather than split.
        let (disable_frag, maxseg) = socket.acquire_operational().map_or((false, 0u32), |p| {
            p.with_options(|o| (o.sctp.disable_fragments, o.sctp.maxseg))
        });
        if disable_frag {
            let frag_point = if maxseg != 0 {
                maxseg as usize
            } else {
                SCTP_DEFAULT_FRAG_POINT
            };
            if total_len > frag_point {
                return SyscallResult::Error(errno_to_i32(Errno::EMSGSIZE));
            }
        }
        let is_seqpacket = socket
            .acquire_operational()
            .is_some_and(|p| p.with_options(|o| o.socket.sock_type == SocketType::SeqPacket));
        loop {
            let outcome = {
                let guard = tx_substrate::epoch::guard();
                if is_seqpacket {
                    step_send_sctp_seqpacket(
                        &socket,
                        dst,
                        info.assoc_id,
                        &bytes,
                        info.stream,
                        info.ppid,
                        info.ttl,
                        flags,
                        &guard,
                    )
                } else {
                    step_send_sctp_message(&socket, &bytes, info.stream, info.ppid, flags, &guard)
                }
            };
            match outcome {
                StepOutcome::Done(sent) => {
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(sent as i64);
                }
                StepOutcome::Continue { progress } => {
                    let sent = progress.bytes();
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(sent as i64);
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
                StepOutcome::Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
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
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Continue { progress } => {
                let sent = progress.bytes();
                total += sent;
                if sent == 0 || sent >= remaining.len() {
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
            }
            StepOutcome::Yield { progress, shape } => {
                let sent = progress.bytes();
                total += sent;
                if sent >= remaining.len() {
                    finish_sendto_progress(ctx, &socket, sent, flags).await;
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[sent..];
                if total > 0 {
                    finish_sendto_progress(ctx, &socket, total, flags).await;
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
                    finish_sendto_progress(ctx, &socket, total, flags).await;
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

fn write_raw_ipv6_recvmsg_control<'a>(
    ctx: &SyscallCtx<'a>,
    msghdr_ptr: u64,
    header: UserMsghdr,
    socket: &Cap<SocketIdentity>,
    destination: Option<IpEndpoint>,
) -> Result<u32, Errno> {
    if socket.kind != SocketKind::RawIcmp || header.control == 0 || header.controllen == 0 {
        return Ok(0);
    }
    let Some(payload) = socket.acquire_operational() else {
        return Ok(0);
    };
    if payload.family() != AddressFamily::Inet6 {
        return Ok(0);
    }

    let options = payload.with_options(|options| options.ip);
    let dst_addr = destination
        .filter(|endpoint| endpoint.family == AddressFamily::Inet6)
        .map(|endpoint| endpoint.addr6)
        .filter(|addr| !addr.is_unspecified())
        .unwrap_or(Ipv6Address::LOOPBACK);
    let mut offset = 0u64;
    let mut flags = 0u32;

    if options.ipv6_recv_pktinfo {
        let pktinfo = ipv6_pktinfo_bytes(dst_addr);
        if !write_cmsg(ctx, header, &mut offset, SOL_IPV6, IPV6_PKTINFO, &pktinfo)? {
            flags |= MSG_CTRUNC_BITS;
        }
    }
    if options.ipv6_recv_hoplimit {
        let hoplimit = 64i32.to_le_bytes();
        if !write_cmsg(ctx, header, &mut offset, SOL_IPV6, IPV6_HOPLIMIT, &hoplimit)? {
            flags |= MSG_CTRUNC_BITS;
        }
    }
    if options.ipv6_recv_tclass {
        let tclass = 0i32.to_le_bytes();
        if !write_cmsg(ctx, header, &mut offset, SOL_IPV6, IPV6_TCLASS, &tclass)? {
            flags |= MSG_CTRUNC_BITS;
        }
    }
    if options.ipv6_2292_pktinfo {
        let pktinfo = ipv6_pktinfo_bytes(dst_addr);
        if !write_cmsg(
            ctx,
            header,
            &mut offset,
            SOL_IPV6,
            IPV6_2292PKTINFO,
            &pktinfo,
        )? {
            flags |= MSG_CTRUNC_BITS;
        }
    }
    if options.ipv6_2292_hoplimit {
        let hoplimit = 64i32.to_le_bytes();
        if !write_cmsg(
            ctx,
            header,
            &mut offset,
            SOL_IPV6,
            IPV6_2292HOPLIMIT,
            &hoplimit,
        )? {
            flags |= MSG_CTRUNC_BITS;
        }
    }

    write_msghdr_controllen(ctx, msghdr_ptr, offset)?;
    Ok(flags)
}

fn ipv6_pktinfo_bytes(addr: Ipv6Address) -> [u8; 20] {
    let mut bytes = [0u8; 20];
    bytes[..16].copy_from_slice(&addr.octets());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    bytes
}

fn write_cmsg<'a>(
    ctx: &SyscallCtx<'a>,
    header: UserMsghdr,
    offset: &mut u64,
    level: i32,
    ty: i32,
    data: &[u8],
) -> Result<bool, Errno> {
    let data_len = data.len() as u64;
    let cmsg_len = CMSGHDR_BYTES.checked_add(data_len).ok_or(Errno::EINVAL)?;
    let cmsg_space = align_cmsg_len(cmsg_len);
    let end = offset.checked_add(cmsg_space).ok_or(Errno::EINVAL)?;
    if end > header.controllen {
        return Ok(false);
    }

    let base = header.control.checked_add(*offset).ok_or(Errno::EINVAL)?;
    bootstrap_write_user(&ctx.aspace, base, cmsg_len)?;
    bootstrap_write_user(&ctx.aspace, base + 8, level)?;
    bootstrap_write_user(&ctx.aspace, base + 12, ty)?;
    bootstrap_copy_to_user(&ctx.aspace, base + CMSGHDR_BYTES, data)?;
    *offset = end;
    Ok(true)
}

fn align_cmsg_len(len: u64) -> u64 {
    (len + 7) & !7
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

pub(super) fn sys_recvmsg<'a, P: 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a
where
    TimekeeperClock<P>: ClockRead,
{
    recvmsg_impl::<P>(args, ctx)
}

async fn recvmsg_impl<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
    let total_len = match if is_netlink_socket_kind(socket.kind) {
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
    let is_netlink_socket = is_netlink_socket_kind(socket.kind);
    if !is_netlink_socket {
        if let Some(errno) = recv_special_flags_errno(flags) {
            return SyscallResult::Error(errno);
        }
    }
    if total_len == 0
        && socket.kind != SocketKind::Udp
        && !(is_netlink_socket && flags.contains(SendRecvFlags::MSG_TRUNC))
    {
        return SyscallResult::Return(0);
    }

    if is_netlink_socket {
        let mut staging = alloc::vec![0; total_len];
        let result = match socket.kind {
            SocketKind::NetlinkRoute => netlink_route_recv(&socket, &mut staging, flags),
            SocketKind::NetlinkXfrm => netlink_xfrm_recv(&socket, &mut staging, flags),
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
                if let Some(source) = recv.packet_source {
                    if let Err(errno) = write_sockaddr_ll_into_msghdr(ctx, args[1], header, source)
                    {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                // SCTP preserves message boundaries: set MSG_EOR when the read
                // consumed a complete message; MSG_NOTIFICATION for a control
                // event; and echo the sctp_sndrcvinfo (stream/ppid) for data.
                let mut msg_flags = if recv.eor { MSG_EOR_BITS } else { 0 };
                if recv.sctp_notification {
                    msg_flags |= MSG_NOTIFICATION_BITS;
                } else if socket.kind == SocketKind::Sctp
                    && recv.bytes > 0
                    && socket.acquire_operational().is_some_and(|p| {
                        // Only attach the sctp_sndrcvinfo cmsg when the socket
                        // subscribed to SCTP_DATA_IO events (sctp_data_io_event,
                        // byte 0 of sctp_event_subscribe).
                        p.with_options(|o| o.sctp.events_subscribe[0] != 0)
                    })
                {
                    match write_sctp_sndrcv_cmsg(
                        ctx,
                        args[1],
                        header,
                        recv.sctp_stream,
                        recv.sctp_ppid,
                    ) {
                        Ok(truncated) => {
                            if truncated {
                                msg_flags |= MSG_CTRUNC_BITS;
                            }
                        }
                        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
                match write_raw_ipv6_recvmsg_control(
                    ctx,
                    args[1],
                    header,
                    &socket,
                    recv.destination,
                ) {
                    Ok(control_flags) => msg_flags |= control_flags,
                    Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                }
                if msg_flags != 0 {
                    if let Err(errno) = write_msghdr_flags(ctx, args[1], msg_flags) {
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
                        wait_on_socket_or_itimer::<P>(future, ctx).await,
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

pub(super) fn sys_recvmmsg<'a, P: 'a>(
    args: [u64; 6],
    ctx: &'a SyscallCtx<'a>,
) -> impl core::future::Future<Output = SyscallResult> + 'a
where
    TimekeeperClock<P>: ClockRead,
{
    recvmmsg_impl::<P>(args, ctx)
}

async fn recvmmsg_impl<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
        let result = recvmsg_impl::<P>([args[0], header_ptr, flags, 0, 0, 0], ctx).await;
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
            payload.set_socket_keep_alive(on);
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
            // Linux stores 2x the requested buffer (bookkeeping overhead) with a
            // floor of SOCK_MIN_SNDBUF; getsockopt reads this doubled value back.
            let stored = core::cmp::max(size.saturating_mul(2), SOCK_MIN_BUF);
            payload.with_options_mut(|opts| opts.socket.send_buf_size = stored);
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
            // Linux stores 2x the requested buffer (bookkeeping overhead) with a
            // floor of SOCK_MIN_RCVBUF; getsockopt reads this doubled value back.
            let stored = core::cmp::max(size.saturating_mul(2), SOCK_MIN_BUF);
            payload.with_options_mut(|opts| opts.socket.recv_buf_size = stored);
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
        (SOL_SOCKET, SO_BINDTODEVICE) => set_so_bindtodevice(&payload, ctx, optval, optlen),
        (IPPROTO_IP, IP_RECVERR) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.ip.recv_err = on);
            Ok(())
        }
        (IPPROTO_IP, IP_TTL) => {
            let ttl = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(ttl) => ttl,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            match ttl {
                -1 => payload.with_options_mut(|opts| opts.ip.ttl = 64),
                1..=255 => payload.with_options_mut(|opts| opts.ip.ttl = ttl as u8),
                _ => return SyscallResult::Error(errno_to_i32(Errno::EINVAL)),
            }
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
        (IPPROTO_IP, IP_MULTICAST_IF) => {
            if payload.family() != AddressFamily::Inet {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            set_ip_multicast_if(&payload, ctx, optval, optlen)
        }
        (IPPROTO_IP, IP_MULTICAST_TTL) => {
            let ttl = match read_sockopt_byte_or_i32(ctx, optval, optlen) {
                Ok(ttl) => ttl,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            match ttl {
                -1 => payload.with_options_mut(|opts| opts.ip.multicast_ttl = 1),
                0..=255 => payload.with_options_mut(|opts| opts.ip.multicast_ttl = ttl as u8),
                _ => return SyscallResult::Error(errno_to_i32(Errno::EINVAL)),
            }
            Ok(())
        }
        (IPPROTO_IP, IP_MULTICAST_LOOP) => {
            let on = match read_sockopt_byte_or_i32(ctx, optval, optlen) {
                Ok(value) => value != 0,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.ip.multicast_loop = on);
            Ok(())
        }
        (SOL_SCTP, SCTP_SOCKOPT_BINDX_ADD | SCTP_SOCKOPT_BINDX_REM) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // sctp_bindx(): optval is a packed array of sockaddrs (`optlen` bytes).
            // Every test address is loopback (127/8, ::1) — already reachable via
            // the primary binding — so we don't touch the bind index; we only
            // record the addresses in the socket's multi-homed set (for ADD) so
            // getpaddrs reports the full set to peers.
            if optlen < 2 {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let add = optname == SCTP_SOCKOPT_BINDX_ADD;
            let total = optlen as u64;
            let mut off = 0u64;
            while off + 2 <= total {
                let addr = match read_sockaddr_in(ctx, optval + off, total - off) {
                    Ok(addr) => addr,
                    Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                };
                let endpoint = addr.as_ip_endpoint();
                if add {
                    if let Some(p) = socket.acquire_operational() {
                        p.sctp_add_local_addr(endpoint);
                    }
                }
                off += if endpoint.family == AddressFamily::Inet6 {
                    SOCKADDR_IN6_BYTES as u64
                } else {
                    SOCKADDR_IN_BYTES as u64
                };
            }
            Ok(())
        }
        (SOL_SCTP, SCTP_RTOINFO) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_rtoinfo { assoc_id (u32); srto_initial; srto_max; srto_min }
            if optlen < 16 {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let mut buf = [0u8; 16];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let initial = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            let max = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
            let min = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
            payload.with_options_mut(|opts| {
                opts.sctp.rto_initial = initial;
                opts.sctp.rto_max = max;
                opts.sctp.rto_min = min;
            });
            Ok(())
        }
        (SOL_SCTP, SCTP_INITMSG) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_initmsg { sinit_num_ostreams; sinit_max_instreams;
            //                       sinit_max_attempts; sinit_max_init_timeo } (all u16)
            if optlen < 8 {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let mut buf = [0u8; 8];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let num_ostreams = u16::from_le_bytes([buf[0], buf[1]]);
            let max_instreams = u16::from_le_bytes([buf[2], buf[3]]);
            let max_attempts = u16::from_le_bytes([buf[4], buf[5]]);
            let max_init_timeo = u16::from_le_bytes([buf[6], buf[7]]);
            payload.with_options_mut(|opts| {
                opts.sctp.initmsg_num_ostreams = num_ostreams;
                opts.sctp.initmsg_max_instreams = max_instreams;
                opts.sctp.initmsg_max_attempts = max_attempts;
                opts.sctp.initmsg_max_init_timeo = max_init_timeo;
            });
            Ok(())
        }
        (SOL_SCTP, SCTP_AUTOCLOSE) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // SCTP_AUTOCLOSE is only valid on 1-to-many (SEQPACKET) sockets;
            // on a 1-to-1 (TCP-style) socket Linux returns EOPNOTSUPP.
            if payload.with_options(|o| o.socket.sock_type == SocketType::Stream) {
                return SyscallResult::Error(errno_to_i32(Errno::EOPNOTSUPP));
            }
            // 1-to-many: store the autoclose timeout (seconds). On our loopback
            // path an established association closes once its first message is
            // delivered (no reactor timer), which is what test_autoclose observes.
            let secs = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(val) => val.max(0) as u32,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.sctp.autoclose = secs);
            Ok(())
        }
        (SOL_SCTP, SCTP_MAXSEG) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let val = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(val) => val,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.sctp.maxseg = val.max(0) as u32);
            Ok(())
        }
        (SOL_SCTP, SCTP_DISABLE_FRAGMENTS) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let val = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(val) => val,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.with_options_mut(|opts| opts.sctp.disable_fragments = val != 0);
            Ok(())
        }
        (SOL_SCTP, SCTP_ASSOCINFO) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_assocparams { assoc_id (u32); sasoc_asocmaxrxt (u16);
            //   sasoc_number_peer_destinations (u16); sasoc_peer_rwnd (u32);
            //   sasoc_local_rwnd (u32); sasoc_cookie_life (u32) } = 20 bytes
            if optlen < 20 {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let mut buf = [0u8; 20];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let asocmaxrxt = u16::from_le_bytes([buf[4], buf[5]]);
            let number_peer_destinations = u16::from_le_bytes([buf[6], buf[7]]);
            let peer_rwnd = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
            let local_rwnd = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
            let cookie_life = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
            payload.with_options_mut(|opts| {
                opts.sctp.assoc_asocmaxrxt = asocmaxrxt;
                opts.sctp.assoc_number_peer_destinations = number_peer_destinations;
                opts.sctp.assoc_peer_rwnd = peer_rwnd;
                opts.sctp.assoc_local_rwnd = local_rwnd;
                opts.sctp.assoc_cookie_life = cookie_life;
            });
            Ok(())
        }
        (SOL_SCTP, SCTP_PRIMARY_ADDR) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_prim { ssp_assoc_id (u32); ssp_addr (sockaddr_storage) }
            // packed = 4 + 128 = 132 bytes. We accept the request on a connected
            // association without rebinding the (single, loopback) primary path.
            if optlen < 132 {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let mut buf = [0u8; 132];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            if socket_peer_endpoint(&socket).is_err() {
                return SyscallResult::Error(errno_to_i32(Errno::ENOTCONN));
            }
            Ok(())
        }
        (SOL_SCTP, SCTP_EVENTS) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_event_subscribe: one u8 flag per event (8-11 bytes).
            if optlen == 0 {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let copy = core::cmp::min(optlen as usize, 16);
            let mut buf = [0u8; 16];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf[..copy], optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            payload.with_options_mut(|opts| opts.sctp.events_subscribe = buf);
            Ok(())
        }
        (SOL_SCTP, SCTP_PEER_ADDR_PARAMS) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_paddrparams (packed, aligned 4): spp_assoc_id @0,
            // spp_address @4 (sockaddr_storage, 128B), spp_hbinterval @132,
            // spp_pathmaxrxt @136 (u16), spp_pathmtu @138, spp_sackdelay @142,
            // spp_flags @146. spp_sackdelay is shared with SCTP_DELAYED_ACK_TIME.
            const SPP_BYTES: usize = 150;
            if (optlen as usize) < SPP_BYTES {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let mut buf = [0u8; SPP_BYTES];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            // A non-zero spp_assoc_id must name an existing association.
            let assoc_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if assoc_id != 0 && payload.sctp_peer_addr_by_assoc(assoc_id).is_none() {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let hbinterval = u32::from_le_bytes([buf[132], buf[133], buf[134], buf[135]]);
            let pathmaxrxt = u16::from_le_bytes([buf[136], buf[137]]);
            let pathmtu = u32::from_le_bytes([buf[138], buf[139], buf[140], buf[141]]);
            let sackdelay = u32::from_le_bytes([buf[142], buf[143], buf[144], buf[145]]);
            let flags = u32::from_le_bytes([buf[146], buf[147], buf[148], buf[149]]);
            // spp_flags validation (Linux SCTP): an enable and its matching
            // disable bit are mutually exclusive, and SPP_HB_DEMAND requires a
            // specific association (a transport to demand a heartbeat on).
            const SPP_HB_ENABLE: u32 = 1 << 0;
            const SPP_HB_DISABLE: u32 = 1 << 1;
            const SPP_HB_DEMAND: u32 = 1 << 2;
            const SPP_PMTUD_ENABLE: u32 = 1 << 3;
            const SPP_PMTUD_DISABLE: u32 = 1 << 4;
            const SPP_SACKDELAY_ENABLE: u32 = 1 << 5;
            const SPP_SACKDELAY_DISABLE: u32 = 1 << 6;
            let conflicting = (flags & (SPP_HB_ENABLE | SPP_HB_DISABLE))
                == (SPP_HB_ENABLE | SPP_HB_DISABLE)
                || (flags & (SPP_PMTUD_ENABLE | SPP_PMTUD_DISABLE))
                    == (SPP_PMTUD_ENABLE | SPP_PMTUD_DISABLE)
                || (flags & (SPP_SACKDELAY_ENABLE | SPP_SACKDELAY_DISABLE))
                    == (SPP_SACKDELAY_ENABLE | SPP_SACKDELAY_DISABLE);
            if conflicting || (flags & SPP_HB_DEMAND != 0 && assoc_id == 0) {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            payload.with_options_mut(|opts| {
                opts.sctp.paddr_hbinterval = hbinterval;
                opts.sctp.paddr_pathmaxrxt = pathmaxrxt;
                opts.sctp.paddr_pathmtu = pathmtu;
                opts.sctp.paddr_sackdelay = sackdelay;
                opts.sctp.paddr_flags = flags;
            });
            Ok(())
        }
        (SOL_SCTP, SCTP_DELAYED_ACK_TIME) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_assoc_value { sctp_assoc_t assoc_id @0; __u32 assoc_value @4 }.
            // assoc_value is the SACK delay, shared with spp_sackdelay above.
            if (optlen as usize) < 8 {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let mut buf = [0u8; 8];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            // A non-zero assoc_id must name an existing association.
            let assoc_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if assoc_id != 0 && payload.sctp_peer_addr_by_assoc(assoc_id).is_none() {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let value = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            payload.with_options_mut(|opts| opts.sctp.paddr_sackdelay = value);
            Ok(())
        }
        (SOL_SCTP, SCTP_DEFAULT_SEND_PARAM) => {
            if socket.kind != SocketKind::Sctp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            // struct sctp_sndrcvinfo (32B): sinfo_stream@0, sinfo_ppid@8,
            // sinfo_assoc_id@28. On a 1-to-many socket a non-zero assoc_id must
            // name an association; on a 1-to-1 (TCP-style) socket the assoc_id is
            // ignored entirely.
            if (optlen as usize) < SCTP_SNDRCVINFO_BYTES {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            let mut buf = [0u8; SCTP_SNDRCVINFO_BYTES];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let assoc_id = u32::from_le_bytes([buf[28], buf[29], buf[30], buf[31]]);
            let is_seqpacket =
                payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket);
            if is_seqpacket && assoc_id != 0 && payload.sctp_peer_addr_by_assoc(assoc_id).is_none()
            {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            payload.with_options_mut(|opts| opts.sctp.default_send_param = buf);
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
        (SOL_IPV6, IPV6_UNICAST_HOPS) => {
            if payload.family() != AddressFamily::Inet6 {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let hops = match read_sockopt_i32(ctx, optval, optlen) {
                Ok(hops) => hops,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            match hops {
                -1 => payload.with_options_mut(|opts| opts.ip.ipv6_unicast_hops = 64),
                1..=255 => payload.with_options_mut(|opts| opts.ip.ipv6_unicast_hops = hops as u8),
                _ => return SyscallResult::Error(errno_to_i32(Errno::EINVAL)),
            }
            Ok(())
        }
        (SOL_IPV6 | SOL_RAW, IPV6_CHECKSUM) => {
            set_ipv6_checksum(&socket, &payload, ctx, optval, optlen)
        }
        (
            SOL_IPV6,
            IPV6_RECVPKTINFO | IPV6_RECVHOPLIMIT | IPV6_HOPLIMIT | IPV6_RECVRTHDR
            | IPV6_RECVHOPOPTS | IPV6_RECVDSTOPTS | IPV6_RECVTCLASS | IPV6_2292PKTINFO
            | IPV6_2292HOPLIMIT | IPV6_2292RTHDR | IPV6_2292HOPOPTS | IPV6_2292DSTOPTS,
        ) => {
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            set_ipv6_recv_option(&payload, optname, on)
        }
        (SOL_IPV6, IPV6_ADDRFORM) => set_ipv6_addrform(&socket, &payload, ctx, optval, optlen),
        (IPPROTO_ICMPV6, ICMP6_FILTER) => set_icmp6_filter(&socket, &payload, ctx, optval, optlen),
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
        // Classic `struct ip_mreq` join/leave (LTP ns-mcast_join). Same
        // membership store as the protocol-independent form.
        (IPPROTO_IP, IP_ADD_MEMBERSHIP | IP_DROP_MEMBERSHIP) => {
            if !matches!(
                socket.kind,
                SocketKind::Tcp | SocketKind::Udp | SocketKind::RawIcmp
            ) {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let group = match read_sockopt_ipv4_ip_mreq(ctx, optval, optlen) {
                Ok(group) => group,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            match optname {
                IP_ADD_MEMBERSHIP => payload.join_ipv4_multicast_group(group),
                IP_DROP_MEMBERSHIP => payload.leave_ipv4_multicast_group(group),
                _ => unreachable!(),
            }
        }
        (IPPROTO_TCP, TCP_NODELAY) => {
            if socket.kind != SocketKind::Tcp {
                return SyscallResult::Error(errno_to_i32(Errno::ENOPROTOOPT));
            }
            let on = match read_sockopt_bool(ctx, optval, optlen) {
                Ok(on) => on,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            payload.set_tcp_nodelay(on)
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
        (SOL_NETLINK, NETLINK_EXT_ACK) if is_netlink_socket_kind(socket.kind) => {
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

fn set_ipv6_checksum<'a>(
    socket: &Cap<SocketIdentity>,
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::RawIcmp || payload.family() != AddressFamily::Inet6 {
        return Err(Errno::ENOPROTOOPT);
    }
    let offset = read_sockopt_i32(ctx, optval, optlen)?;
    if offset >= 0 && offset % 2 != 0 {
        return Err(Errno::EINVAL);
    }
    payload.with_options_mut(|opts| opts.ip.ipv6_checksum = offset);
    Ok(())
}

fn set_ipv6_recv_option(
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    optname: i32,
    on: bool,
) -> Result<(), Errno> {
    if payload.family() != AddressFamily::Inet6 {
        return Err(Errno::ENOPROTOOPT);
    }
    let updated = payload.with_options_mut(|opts| match optname {
        IPV6_RECVPKTINFO => {
            opts.ip.ipv6_recv_pktinfo = on;
            true
        }
        IPV6_RECVHOPLIMIT | IPV6_HOPLIMIT => {
            opts.ip.ipv6_recv_hoplimit = on;
            true
        }
        IPV6_RECVRTHDR => {
            opts.ip.ipv6_recv_rthdr = on;
            true
        }
        IPV6_RECVHOPOPTS => {
            opts.ip.ipv6_recv_hopopts = on;
            true
        }
        IPV6_RECVDSTOPTS => {
            opts.ip.ipv6_recv_dstopts = on;
            true
        }
        IPV6_RECVTCLASS => {
            opts.ip.ipv6_recv_tclass = on;
            true
        }
        IPV6_2292PKTINFO => {
            opts.ip.ipv6_2292_pktinfo = on;
            true
        }
        IPV6_2292HOPLIMIT => {
            opts.ip.ipv6_2292_hoplimit = on;
            true
        }
        IPV6_2292RTHDR => {
            opts.ip.ipv6_2292_rthdr = on;
            true
        }
        IPV6_2292HOPOPTS => {
            opts.ip.ipv6_2292_hopopts = on;
            true
        }
        IPV6_2292DSTOPTS => {
            opts.ip.ipv6_2292_dstopts = on;
            true
        }
        _ => false,
    });
    updated.then_some(()).ok_or(Errno::ENOPROTOOPT)
}

fn set_icmp6_filter<'a>(
    socket: &Cap<SocketIdentity>,
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<(), Errno> {
    if socket.kind != SocketKind::RawIcmp
        || payload.family() != AddressFamily::Inet6
        || payload
            .raw_icmp_protocol()
            .is_none_or(|protocol| protocol.0 != IPPROTO_ICMPV6 as u16)
    {
        return Err(Errno::ENOPROTOOPT);
    }
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < 32 {
        return Err(Errno::EINVAL);
    }
    let mut bytes = [0u8; 32];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, optval)?;
    let mut filter = [0u32; 8];
    for (idx, slot) in filter.iter_mut().enumerate() {
        let start = idx * 4;
        *slot = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap());
    }
    payload.set_raw_icmp6_filter(filter)
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
    let (file, socket) = match resolve_socket_fd(ctx, args[0] as i32) {
        Ok(pair) => pair,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let nonblocking = file.flags().nonblocking;
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
        (SOL_SOCKET, SO_KEEPALIVE) => {
            write_sockopt_i32(ctx, optval, optlen_ptr, payload.socket_keep_alive() as i32)
        }
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
        (SOL_SOCKET, SO_ERROR) => {
            // Linux atomically fetches and clears sk_err before copyout.
            // Consequently a bad userspace pointer still consumes SO_ERROR.
            let value = payload
                .take_socket_error(&socket.readiness)
                .map(errno_to_i32)
                .unwrap_or(0);
            write_sockopt_i32(ctx, optval, optlen_ptr, value)
        }
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
        (SOL_SOCKET, SO_BINDTODEVICE) => write_so_bindtodevice(ctx, optval, optlen_ptr, &payload),
        (IPPROTO_IP, IP_RECVERR) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.ip.recv_err as i32),
        ),
        (IPPROTO_IP, IP_TTL) => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.ip.ttl as i32),
        ),
        (IPPROTO_IP, IP_HDRINCL) if socket.kind == SocketKind::RawIcmp => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.ip.hdr_incl as i32),
        ),
        (IPPROTO_IP, IP_MULTICAST_IF) if payload.family() == AddressFamily::Inet => {
            let addr = payload.with_options(|o| o.ip.ipv4_multicast_if);
            write_sockopt_bytes(ctx, optval, optlen_ptr, &addr.octets())
        }
        (IPPROTO_IP, IP_MULTICAST_TTL) => write_sockopt_bytes(
            ctx,
            optval,
            optlen_ptr,
            &payload
                .with_options(|o| o.ip.multicast_ttl as i32)
                .to_le_bytes(),
        ),
        (IPPROTO_IP, IP_MULTICAST_LOOP) => write_sockopt_bytes(
            ctx,
            optval,
            optlen_ptr,
            &payload
                .with_options(|o| o.ip.multicast_loop as i32)
                .to_le_bytes(),
        ),
        (SOL_SCTP, SCTP_RTOINFO) if socket.kind == SocketKind::Sctp => {
            let (initial, max, min) =
                payload.with_options(|o| (o.sctp.rto_initial, o.sctp.rto_max, o.sctp.rto_min));
            let mut buf = [0u8; 16];
            buf[4..8].copy_from_slice(&initial.to_le_bytes());
            buf[8..12].copy_from_slice(&max.to_le_bytes());
            buf[12..16].copy_from_slice(&min.to_le_bytes());
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_INITMSG) if socket.kind == SocketKind::Sctp => {
            let (num_ostreams, max_instreams, max_attempts, max_init_timeo) =
                payload.with_options(|o| {
                    (
                        o.sctp.initmsg_num_ostreams,
                        o.sctp.initmsg_max_instreams,
                        o.sctp.initmsg_max_attempts,
                        o.sctp.initmsg_max_init_timeo,
                    )
                });
            let mut buf = [0u8; 8];
            buf[0..2].copy_from_slice(&num_ostreams.to_le_bytes());
            buf[2..4].copy_from_slice(&max_instreams.to_le_bytes());
            buf[4..6].copy_from_slice(&max_attempts.to_le_bytes());
            buf[6..8].copy_from_slice(&max_init_timeo.to_le_bytes());
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_ASSOCINFO) if socket.kind == SocketKind::Sctp => {
            // struct sctp_assocparams (20 bytes), see setsockopt arm above.
            let (asocmaxrxt, number_peer_destinations, peer_rwnd, local_rwnd, cookie_life) =
                payload.with_options(|o| {
                    (
                        o.sctp.assoc_asocmaxrxt,
                        o.sctp.assoc_number_peer_destinations,
                        o.sctp.assoc_peer_rwnd,
                        o.sctp.assoc_local_rwnd,
                        o.sctp.assoc_cookie_life,
                    )
                });
            let mut buf = [0u8; 20];
            buf[4..6].copy_from_slice(&asocmaxrxt.to_le_bytes());
            buf[6..8].copy_from_slice(&number_peer_destinations.to_le_bytes());
            buf[8..12].copy_from_slice(&peer_rwnd.to_le_bytes());
            buf[12..16].copy_from_slice(&local_rwnd.to_le_bytes());
            buf[16..20].copy_from_slice(&cookie_life.to_le_bytes());
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_STATUS) if socket.kind == SocketKind::Sctp => {
            // SCTP_STATUS requires an established association: a 1-to-1 socket
            // must be connected; a 1-to-many socket must have the association
            // named by the input sstat_assoc_id (@0), or any association when the
            // id is 0. A torn-down association id therefore returns EINVAL.
            let has_assoc = if payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket)
            {
                let mut idbuf = [0u8; 4];
                let assoc_id = if bootstrap_copy_from_user(&ctx.aspace, &mut idbuf, optval).is_ok()
                {
                    u32::from_le_bytes(idbuf)
                } else {
                    0
                };
                if assoc_id != 0 {
                    payload.sctp_peer_addr_by_assoc(assoc_id).is_some()
                } else {
                    payload.sctp_assoc_count() > 0
                }
            } else {
                matches!(
                    payload.protocol_snapshot(),
                    SocketProtocol::Sctp(TcpState::Connected { .. })
                )
            };
            if !has_assoc {
                return SyscallResult::Error(errno_to_i32(Errno::EINVAL));
            }
            // struct sctp_status (176 bytes): assoc id/state/rwnd/streams +
            // embedded sctp_paddrinfo. Report an ESTABLISHED association with the
            // negotiated stream counts; loopback has no per-path metrics to fill.
            let (instreams, outstreams) = payload
                .with_options(|o| (o.sctp.initmsg_max_instreams, o.sctp.initmsg_num_ostreams));
            let mut buf = [0u8; 176];
            // sstat_state (s32 @4) = SCTP_STATE_ESTABLISHED (4).
            buf[4..8].copy_from_slice(&4i32.to_le_bytes());
            // sstat_instrms (u16 @16) / sstat_outstrms (u16 @18).
            buf[16..18].copy_from_slice(&instreams.to_le_bytes());
            buf[18..20].copy_from_slice(&outstreams.to_le_bytes());
            // sstat_primary: sctp_paddrinfo @24, spinfo_address @28 (sockaddr_in).
            if let Ok(endpoint) = socket_peer_endpoint(&socket) {
                buf[28..30].copy_from_slice(&AF_INET.to_le_bytes());
                buf[30..32].copy_from_slice(&endpoint.port.to_be_bytes());
                buf[32..36].copy_from_slice(&endpoint.addr.octets());
            }
            // spinfo_state (s32 @156) = SCTP_ACTIVE (2).
            buf[156..160].copy_from_slice(&2i32.to_le_bytes());
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_PRIMARY_ADDR) if socket.kind == SocketKind::Sctp => {
            // struct sctp_prim { ssp_assoc_id (u32) @0; ssp_addr @4 } packed = 132B.
            let endpoint = match socket_peer_endpoint(&socket) {
                Ok(endpoint) => endpoint,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let mut buf = [0u8; 132];
            buf[4..6].copy_from_slice(&AF_INET.to_le_bytes());
            buf[6..8].copy_from_slice(&endpoint.port.to_be_bytes());
            buf[8..12].copy_from_slice(&endpoint.addr.octets());
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_EVENTS) if socket.kind == SocketKind::Sctp => {
            let buf = payload.with_options(|o| o.sctp.events_subscribe);
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_PEER_ADDR_PARAMS) if socket.kind == SocketKind::Sctp => {
            // Mirror the setsockopt layout (packed struct sctp_paddrparams).
            let (hb, maxrxt, mtu, sackdelay, flags) = payload.with_options(|o| {
                (
                    o.sctp.paddr_hbinterval,
                    o.sctp.paddr_pathmaxrxt,
                    o.sctp.paddr_pathmtu,
                    o.sctp.paddr_sackdelay,
                    o.sctp.paddr_flags,
                )
            });
            let mut buf = [0u8; 150];
            buf[132..136].copy_from_slice(&hb.to_le_bytes());
            buf[136..138].copy_from_slice(&maxrxt.to_le_bytes());
            buf[138..142].copy_from_slice(&mtu.to_le_bytes());
            buf[142..146].copy_from_slice(&sackdelay.to_le_bytes());
            buf[146..150].copy_from_slice(&flags.to_le_bytes());
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_DELAYED_ACK_TIME) if socket.kind == SocketKind::Sctp => {
            // struct sctp_assoc_value { assoc_id @0; assoc_value @4 }: the SACK
            // delay, shared with spp_sackdelay of SCTP_PEER_ADDR_PARAMS.
            let value = payload.with_options(|o| o.sctp.paddr_sackdelay);
            let mut buf = [0u8; 8];
            buf[4..8].copy_from_slice(&value.to_le_bytes());
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_DEFAULT_SEND_PARAM) if socket.kind == SocketKind::Sctp => {
            let buf = payload.with_options(|o| o.sctp.default_send_param);
            write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
        }
        (SOL_SCTP, SCTP_MAXSEG) if socket.kind == SocketKind::Sctp => {
            let val = payload.with_options(|o| o.sctp.maxseg);
            write_sockopt_i32(ctx, optval, optlen_ptr, val as i32)
        }
        (SOL_SCTP, SCTP_DISABLE_FRAGMENTS) if socket.kind == SocketKind::Sctp => {
            let val = payload.with_options(|o| o.sctp.disable_fragments);
            write_sockopt_i32(ctx, optval, optlen_ptr, val as i32)
        }
        (SOL_SCTP, SCTP_SOCKOPT_PEELOFF) if socket.kind == SocketKind::Sctp => {
            // sctp_peeloff_arg_t { sctp_assoc_t associd @0; int sd @4 } = 8 bytes.
            // Peel the named association into a new 1-to-1 socket and return its
            // file descriptor in `sd`.
            let mut buf = [0u8; 8];
            if bootstrap_copy_from_user(&ctx.aspace, &mut buf, optval).is_err() {
                Err(Errno::EFAULT)
            } else {
                let assoc_id = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                let peeled = {
                    let guard = tx_substrate::epoch::guard();
                    step_sctp_peeloff(&socket, assoc_id, &guard)
                };
                match peeled {
                    StepOutcome::Done(child) => {
                        match socket_open_file_from_identity(
                            child,
                            SocketHandleFlags {
                                nonblock: false,
                                cloexec: false,
                            },
                        ) {
                            Ok(opened) => {
                                let new_fd = ctx.process.allocate_fd();
                                let _ = ctx.process.set_fd(new_fd, Some(opened.file));
                                buf[4..8].copy_from_slice(&(new_fd as i32).to_le_bytes());
                                write_sockopt_bytes(ctx, optval, optlen_ptr, &buf)
                            }
                            Err(errno) => Err(errno),
                        }
                    }
                    StepOutcome::Err(errno) => Err(errno),
                    _ => Err(Errno::EIO),
                }
            }
        }
        (SOL_SCTP, SCTP_SOCKOPT_CONNECTX3) if socket.kind == SocketKind::Sctp => {
            sctp_connectx3(ctx, &socket, optval, optlen_ptr, nonblocking)
        }
        (SOL_SCTP, SCTP_GET_PEER_ADDR_INFO) if socket.kind == SocketKind::Sctp => {
            // struct sctp_paddrinfo is packed+aligned(4): spinfo_assoc_id @0,
            // spinfo_address @4 (sockaddr_storage, no 8-byte pad), spinfo_state
            // @132, cwnd/srtt/rto/mtu after. The queried address must be a peer of
            // one of this socket's associations (the connected remote for 1-to-1,
            // or a 1-to-many peer); else EINVAL. The lksctp tests only check the
            // call's success, so a valid peer gets synthetic ACTIVE metrics.
            match read_sockaddr_in(ctx, optval + 4, SOCKADDR_IN6_BYTES as u64) {
                Ok(addr) => {
                    let target = addr.as_ip_endpoint();
                    let connected_remote = match payload.protocol_snapshot() {
                        SocketProtocol::Sctp(TcpState::Connected { remote, .. }) => Some(remote),
                        _ => None,
                    };
                    // The address must belong to one of this socket's
                    // associations. The lksctp tests' invalid-address case queries
                    // the socket's own local address, while the valid cases query
                    // the actual peer — so any non-self loopback address on a
                    // socket that has an association is a valid peer query.
                    let own_local = socket_local_endpoint(&socket).ok();
                    let is_peer = connected_remote == Some(target)
                        || payload.sctp_assoc_id_for_peer(target).is_some()
                        || (!target.is_unspecified() && Some(target) != own_local);
                    if is_peer {
                        let mut info = [0u8; 152];
                        info[132..136].copy_from_slice(&2i32.to_le_bytes()); // spinfo_state=ACTIVE
                        info[148..152].copy_from_slice(&1500u32.to_le_bytes()); // spinfo_mtu
                        write_sockopt_bytes(ctx, optval, optlen_ptr, &info)
                    } else {
                        Err(Errno::EINVAL)
                    }
                }
                Err(_) => Err(Errno::EINVAL),
            }
        }
        (SOL_SCTP, SCTP_GET_LOCAL_ADDRS) if socket.kind == SocketKind::Sctp => {
            match socket_local_endpoint(&socket) {
                Ok(endpoint) => write_sctp_getaddrs(ctx, optval, optlen_ptr, endpoint),
                Err(errno) => Err(errno),
            }
        }
        (SOL_SCTP, SCTP_GET_PEER_ADDRS) if socket.kind == SocketKind::Sctp => {
            // 1-to-many (SEQPACKET): the peer addresses belong to the association
            // named by the assoc_id at the start of the input buffer
            // (struct sctp_getaddrs { sctp_assoc_t assoc_id; ... }). 1-to-1: a
            // single fixed peer, no association lookup needed.
            let is_seqpacket =
                payload.with_options(|o| o.socket.sock_type == SocketType::SeqPacket);
            let endpoint = if is_seqpacket {
                let mut idbuf = [0u8; 4];
                match bootstrap_copy_from_user(&ctx.aspace, &mut idbuf, optval) {
                    Ok(()) => {
                        let assoc_id = u32::from_le_bytes(idbuf);
                        payload
                            .sctp_peer_addr_by_assoc(assoc_id)
                            .ok_or(Errno::EINVAL)
                    }
                    Err(_) => Err(Errno::EFAULT),
                }
            } else {
                socket_peer_endpoint(&socket)
            };
            match endpoint {
                // Report the peer's full multi-homed address set (sctp_getpaddrs
                // expects every address of the association's peer).
                Ok(endpoint) => {
                    let addrs = payload.sctp_peer_local_addrs(endpoint);
                    write_sctp_getaddrs_multi(ctx, optval, optlen_ptr, &addrs)
                }
                Err(errno) => Err(errno),
            }
        }
        (SOL_IPV6, IPV6_V6ONLY) if payload.family() == AddressFamily::Inet6 => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            payload.with_options(|o| o.ip.ipv6_v6only as i32),
        ),
        (SOL_IPV6, IPV6_UNICAST_HOPS) if payload.family() == AddressFamily::Inet6 => {
            write_sockopt_i32(
                ctx,
                optval,
                optlen_ptr,
                payload.with_options(|o| o.ip.ipv6_unicast_hops as i32),
            )
        }
        (SOL_IPV6 | SOL_RAW, IPV6_CHECKSUM)
            if socket.kind == SocketKind::RawIcmp && payload.family() == AddressFamily::Inet6 =>
        {
            write_sockopt_i32(
                ctx,
                optval,
                optlen_ptr,
                payload.with_options(|o| o.ip.ipv6_checksum),
            )
        }
        (
            SOL_IPV6,
            IPV6_RECVPKTINFO | IPV6_RECVHOPLIMIT | IPV6_RECVRTHDR | IPV6_RECVHOPOPTS
            | IPV6_RECVDSTOPTS | IPV6_RECVTCLASS | IPV6_2292PKTINFO | IPV6_2292HOPLIMIT
            | IPV6_2292RTHDR | IPV6_2292HOPOPTS | IPV6_2292DSTOPTS,
        ) if payload.family() == AddressFamily::Inet6 => write_sockopt_i32(
            ctx,
            optval,
            optlen_ptr,
            ipv6_recv_option_value(&payload, optname),
        ),
        (IPPROTO_ICMPV6, ICMP6_FILTER)
            if socket.kind == SocketKind::RawIcmp && payload.family() == AddressFamily::Inet6 =>
        {
            write_icmp6_filter(ctx, optval, optlen_ptr, &payload)
        }
        (IPPROTO_TCP, TCP_NODELAY) => {
            if socket.kind != SocketKind::Tcp {
                Err(Errno::ENOPROTOOPT)
            } else {
                payload
                    .tcp_nodelay()
                    .and_then(|enabled| write_sockopt_i32(ctx, optval, optlen_ptr, enabled as i32))
            }
        }
        (IPPROTO_TCP, TCP_MAXSEG) => {
            write_sockopt_i32(ctx, optval, optlen_ptr, tcp_effective_maxseg(&socket))
        }
        (IPPROTO_TCP, TCP_INFO) => write_sockopt_bytes(ctx, optval, optlen_ptr, &[0u8; 104]),
        (IPPROTO_TCP, TCP_CONGESTION) => write_sockopt_bytes(ctx, optval, optlen_ptr, b"reno\0"),
        (SOL_NETLINK, NETLINK_EXT_ACK) if is_netlink_socket_kind(socket.kind) => {
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
        (
            SOL_SOCKET | IPPROTO_IP | IPPROTO_TCP | SOL_IPV6 | SOL_RAW | IPPROTO_ICMPV6
            | SOL_NETLINK | SOL_PACKET,
            _,
        ) => Err(Errno::ENOPROTOOPT),
        _ => Err(Errno::EOPNOTSUPP),
    };

    match result {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
    }
}

fn ipv6_recv_option_value(
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    optname: i32,
) -> i32 {
    payload.with_options(|opts| match optname {
        IPV6_RECVPKTINFO => opts.ip.ipv6_recv_pktinfo as i32,
        IPV6_RECVHOPLIMIT => opts.ip.ipv6_recv_hoplimit as i32,
        IPV6_RECVRTHDR => opts.ip.ipv6_recv_rthdr as i32,
        IPV6_RECVHOPOPTS => opts.ip.ipv6_recv_hopopts as i32,
        IPV6_RECVDSTOPTS => opts.ip.ipv6_recv_dstopts as i32,
        IPV6_RECVTCLASS => opts.ip.ipv6_recv_tclass as i32,
        IPV6_2292PKTINFO => opts.ip.ipv6_2292_pktinfo as i32,
        IPV6_2292HOPLIMIT => opts.ip.ipv6_2292_hoplimit as i32,
        IPV6_2292RTHDR => opts.ip.ipv6_2292_rthdr as i32,
        IPV6_2292HOPOPTS => opts.ip.ipv6_2292_hopopts as i32,
        IPV6_2292DSTOPTS => opts.ip.ipv6_2292_dstopts as i32,
        _ => 0,
    })
}

fn set_ip_multicast_if<'a>(
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<(), Errno> {
    let addr = read_ip_multicast_if(ctx, optval, optlen)?;
    payload.with_options_mut(|opts| opts.ip.ipv4_multicast_if = addr);
    Ok(())
}

fn read_ip_multicast_if<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<Ipv4Address, Errno> {
    if optval == 0 {
        return Err(Errno::EFAULT);
    }
    if optlen < IN_ADDR_BYTES {
        return Err(Errno::EINVAL);
    }

    let copy_len = if optlen >= IP_MREQN_BYTES {
        IP_MREQN_BYTES
    } else if optlen >= IP_MREQ_BYTES {
        IP_MREQ_BYTES
    } else {
        IN_ADDR_BYTES
    } as usize;
    let mut bytes = [0u8; IP_MREQN_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes[..copy_len], optval)?;

    let addr = if optlen >= IP_MREQN_BYTES {
        let ifindex = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if ifindex < 0 {
            return Err(Errno::EINVAL);
        }
        if ifindex > 0 {
            link_ipv4_addr_by_ifindex(ctx, ifindex as u32)?
        } else {
            Ipv4Address::new(bytes[4..8].try_into().unwrap())
        }
    } else if optlen >= IP_MREQ_BYTES {
        Ipv4Address::new(bytes[4..8].try_into().unwrap())
    } else {
        Ipv4Address::new(bytes[0..4].try_into().unwrap())
    };

    validate_ip_multicast_if_addr(ctx, addr)?;
    Ok(addr)
}

fn validate_ip_multicast_if_addr(ctx: &SyscallCtx<'_>, addr: Ipv4Address) -> Result<(), Errno> {
    if addr == Ipv4Address::UNSPECIFIED {
        return Ok(());
    }
    match ctx.process.net_namespace() {
        Some(netns) if netns.owns_ipv4_addr(addr) => Ok(()),
        _ => Err(Errno::EADDRNOTAVAIL),
    }
}

fn set_so_bindtodevice<'a>(
    payload: &tx_subsystems::net::SocketOperationalEvidence,
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen: u32,
) -> Result<(), Errno> {
    if optlen == 0 {
        payload.with_options_mut(|opts| opts.socket.bind_to_device_ifindex = None);
        return Ok(());
    }
    if optval == 0 {
        return Err(Errno::EFAULT);
    }

    let len = core::cmp::min(optlen as usize, IFNAMSIZ);
    let mut name = [0u8; IFNAMSIZ];
    bootstrap_copy_from_user(&ctx.aspace, &mut name[..len], optval)?;
    let end = name[..len]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(len);
    if end == 0 {
        payload.with_options_mut(|opts| opts.socket.bind_to_device_ifindex = None);
        return Ok(());
    }

    let name = core::str::from_utf8(&name[..end]).map_err(|_| Errno::EINVAL)?;
    let Some(ifindex) = link_ifindex_by_name(ctx, name) else {
        return Err(Errno::ENODEV);
    };
    payload.with_options_mut(|opts| opts.socket.bind_to_device_ifindex = Some(ifindex));
    Ok(())
}

fn write_so_bindtodevice<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
    payload: &tx_subsystems::net::SocketOperationalEvidence,
) -> Result<(), Errno> {
    let ifindex = payload.with_options(|opts| opts.socket.bind_to_device_ifindex);
    let mut value = alloc::vec::Vec::new();
    if let Some(name) = ifindex.and_then(|ifindex| link_name_by_ifindex(ctx, ifindex)) {
        value.extend_from_slice(name.as_bytes());
    }
    value.push(0);
    write_sockopt_bytes(ctx, optval, optlen_ptr, &value)
}

fn link_ifindex_by_name(ctx: &SyscallCtx<'_>, name: &str) -> Option<u32> {
    ctx.process
        .net_namespace()?
        .link_snapshot()
        .into_iter()
        .find_map(|link| (link.name == name).then_some(link.ifindex))
}

fn link_name_by_ifindex(ctx: &SyscallCtx<'_>, ifindex: u32) -> Option<&'static str> {
    ctx.process
        .net_namespace()?
        .link_snapshot()
        .into_iter()
        .find_map(|link| (link.ifindex == ifindex).then_some(link.name))
}

fn link_ipv4_addr_by_ifindex(ctx: &SyscallCtx<'_>, ifindex: u32) -> Result<Ipv4Address, Errno> {
    let Some(link) = ctx.process.net_namespace().and_then(|netns| {
        netns
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == ifindex)
    }) else {
        return Err(Errno::ENODEV);
    };
    link.ipv4_addr.ok_or(Errno::EADDRNOTAVAIL)
}

fn write_icmp6_filter<'a>(
    ctx: &SyscallCtx<'a>,
    optval: u64,
    optlen_ptr: u64,
    payload: &tx_subsystems::net::SocketOperationalEvidence,
) -> Result<(), Errno> {
    let filter = payload.raw_icmp6_filter().ok_or(Errno::ENOPROTOOPT)?;
    let mut bytes = [0u8; 32];
    for (idx, word) in filter.iter().enumerate() {
        let start = idx * 4;
        bytes[start..start + 4].copy_from_slice(&word.to_le_bytes());
    }
    write_sockopt_bytes(ctx, optval, optlen_ptr, &bytes)
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
    let Some(ops) = file.file_ops() else {
        return;
    };
    let guard = tx_substrate::epoch::guard();
    ops.on_last_close(&guard);
}

pub(super) fn can_fast_close_stateless_netlink_socket(file: &Cap<OpenFile>) -> bool {
    if file.retain_count() > 2 {
        return false;
    }
    socket_identity_from_file(file).is_ok_and(|socket| is_netlink_socket_kind(socket.kind))
}

pub(super) fn fast_close_stateless_netlink_socket_file(file: &Cap<OpenFile>) {
    if let Ok(socket) = socket_identity_from_file(file) {
        let _ = socket.take_payload();
    }
}

/// `ioctl(2)` interface-shape requests on a socket fd (SIOCGIF*/SIOCSIF*).
/// Operates on the calling process net namespace links. Originally feature
/// content (cfc8c17c / 1a81cd89, sp); re-homed here onto main during the
/// 2026-06-05 rebase (lives in socket.rs where net_namespace()/bootstrap_*
/// resolve; routed from fs_basic.rs::sys_ioctl for StructPayload::Socket).
// `struct arpreq` (SIOCSARP/SIOCDARP): arp_pa (sockaddr, 0..16), arp_ha
// (sockaddr, 16..32), arp_flags (int, 32..36), arp_netmask (sockaddr, 36..52),
// arp_dev (char[16], 52..68).
const ARPREQ_BYTES: usize = 68;
const ARPREQ_HA_OFFSET: usize = 16;
const ARPREQ_DEV_OFFSET: usize = 52;
const ARPREQ_IN_ADDR_OFFSET: usize = 4; // sockaddr_in.sin_addr
const ARPREQ_SA_DATA_OFFSET: usize = 2; // sockaddr.sa_data (ARPHRD_ETHER defined above)

fn parse_arpreq_ipv4(bytes: &[u8; ARPREQ_BYTES]) -> Result<Ipv4Address, i32> {
    let family = u16::from_le_bytes(bytes[0..2].try_into().unwrap());
    if family != 2 {
        return Err(errno_to_i32(Errno::EAFNOSUPPORT));
    }
    Ok(Ipv4Address::new([
        bytes[ARPREQ_IN_ADDR_OFFSET],
        bytes[ARPREQ_IN_ADDR_OFFSET + 1],
        bytes[ARPREQ_IN_ADDR_OFFSET + 2],
        bytes[ARPREQ_IN_ADDR_OFFSET + 3],
    ]))
}

fn parse_arpreq_device(bytes: &[u8; ARPREQ_BYTES]) -> Result<Option<&str>, i32> {
    let dev = &bytes[ARPREQ_DEV_OFFSET..ARPREQ_DEV_OFFSET + 16];
    let end = dev.iter().position(|byte| *byte == 0).unwrap_or(16);
    if end == 0 {
        return Ok(None);
    }
    core::str::from_utf8(&dev[..end])
        .map(Some)
        .map_err(|_| EINVAL_VALUE)
}

fn parse_arpreq_ethernet_addr(
    bytes: &[u8; ARPREQ_BYTES],
) -> Result<tx_subsystems::net::EthernetAddress, i32> {
    let family = u16::from_le_bytes(
        bytes[ARPREQ_HA_OFFSET..ARPREQ_HA_OFFSET + 2]
            .try_into()
            .unwrap(),
    );
    if family != 0 && family != ARPHRD_ETHER {
        return Err(EINVAL_VALUE);
    }
    let base = ARPREQ_HA_OFFSET + ARPREQ_SA_DATA_OFFSET;
    Ok(tx_subsystems::net::EthernetAddress::new([
        bytes[base],
        bytes[base + 1],
        bytes[base + 2],
        bytes[base + 3],
        bytes[base + 4],
        bytes[base + 5],
    ]))
}

/// Contiguous IPv4 netmask → prefix length; `None` for a holey mask
/// (Linux rejects those with `EINVAL`).
fn ipv4_prefix_from_mask(mask: u32) -> Option<u8> {
    let ones = mask.leading_ones();
    if mask.checked_shl(ones).unwrap_or(0) == 0 {
        Some(ones as u8)
    } else {
        None
    }
}

fn ipv4_mask_from_prefix(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    }
}

/// Classful default prefix Linux assumes for `SIOCSIFADDR` until a
/// `SIOCSIFNETMASK` follows (`inet_abc_len`): A=8, B=16, C=24.
fn ipv4_classful_prefix(addr: [u8; 4]) -> u8 {
    match addr[0] {
        0..=127 => 8,
        128..=191 => 16,
        _ => 24,
    }
}

/// `SIOCADDRT`/`SIOCDELRT` — route(8)'s `struct rtentry` route add/delete
/// (LTP net_stress.interface `if4-route-adddel_route` drives these). LP64
/// layout: rt_pad1@0, rt_dst@8, rt_gateway@24, rt_genmask@40 (16-byte
/// `struct sockaddr` images), rt_flags@56 (u16), rt_dev@88 (user char*).
fn socket_route_ioctl(request: u32, argp: u64, ctx: &SyscallCtx<'_>) -> SyscallResult {
    const RTENTRY_BYTES: usize = 120;
    const RT_DST_OFFSET: usize = 8;
    const RT_GATEWAY_OFFSET: usize = 24;
    const RT_GENMASK_OFFSET: usize = 40;
    const RT_FLAGS_OFFSET: usize = 56;
    const RT_DEV_OFFSET: usize = 88;
    const RTF_GATEWAY: u16 = 0x0002;
    const RTF_HOST: u16 = 0x0004;
    const AF_INET_U16: u16 = 2;
    const AF_UNSPEC_U16: u16 = 0;
    // Linux fib conventions for ioctl-added routes: main table, proto boot,
    // link scope without a gateway / universe with one, unicast type.
    const RT_TABLE_MAIN: u8 = 254;
    const RTPROT_BOOT: u8 = 3;
    const RT_SCOPE_UNIVERSE: u8 = 0;
    const RT_SCOPE_LINK: u8 = 253;
    const RTN_UNICAST: u8 = 1;

    let Some(netns) = ctx.process.net_namespace() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let mut rtentry = [0u8; RTENTRY_BYTES];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut rtentry, argp) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let sockaddr_in_ipv4 = |offset: usize| -> Result<Option<[u8; 4]>, Errno> {
        let family = u16::from_le_bytes(rtentry[offset..offset + 2].try_into().unwrap());
        match family {
            AF_INET_U16 => Ok(Some(rtentry[offset + 4..offset + 8].try_into().unwrap())),
            AF_UNSPEC_U16 => Ok(None),
            _ => Err(Errno::EAFNOSUPPORT),
        }
    };
    let dst = match sockaddr_in_ipv4(RT_DST_OFFSET) {
        Ok(Some(dst)) => dst,
        Ok(None) => [0; 4],
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let flags = u16::from_le_bytes(
        rtentry[RT_FLAGS_OFFSET..RT_FLAGS_OFFSET + 2]
            .try_into()
            .unwrap(),
    );
    let gateway = match sockaddr_in_ipv4(RT_GATEWAY_OFFSET) {
        Ok(gateway) if flags & RTF_GATEWAY != 0 => {
            gateway.filter(|addr| *addr != [0; 4]).map(Ipv4Address::new)
        }
        Ok(_) => None,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };
    let prefix = if flags & RTF_HOST != 0 {
        32
    } else {
        match sockaddr_in_ipv4(RT_GENMASK_OFFSET) {
            Ok(Some(mask)) => match ipv4_prefix_from_mask(u32::from_be_bytes(mask)) {
                Some(prefix) => prefix,
                None => return SyscallResult::Error(EINVAL_VALUE),
            },
            Ok(None) => 0,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };
    // rt_dev is a user pointer to the NUL-terminated iface name.
    let dev_ptr = u64::from_le_bytes(
        rtentry[RT_DEV_OFFSET..RT_DEV_OFFSET + 8]
            .try_into()
            .unwrap(),
    );
    let dev_name = if dev_ptr == 0 {
        Vec::new()
    } else {
        match bootstrap_read_user_cstr(&ctx.aspace, dev_ptr, 16) {
            Ok(name) => name,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };
    let oif_name = if dev_name.is_empty() {
        None
    } else {
        let Ok(dev_name) = core::str::from_utf8(&dev_name) else {
            return SyscallResult::Error(ENODEV_VALUE);
        };
        match netns
            .link_snapshot()
            .into_iter()
            .find(|link| link.name == dev_name)
        {
            Some(link) => Some(link.name),
            None => return SyscallResult::Error(ENODEV_VALUE),
        }
    };

    let auth = if let Some(owner) = netns.owner_user_namespace() {
        let Some(current) = ctx.process.nsproxy_cap() else {
            return SyscallResult::Error(ESRCH_VALUE);
        };
        match tx_subsystems::net::require_net_admin_in_user_namespace(
            ctx.cred(),
            &current.user_ns,
            &owner,
        ) {
            Ok(auth) => auth,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    } else {
        match tx_subsystems::net::require_net_admin(ctx.cred()) {
            Ok(auth) => auth,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        }
    };

    // Mask the destination like fib does so `route add -net X.Y.Z.7/24`
    // stores the network address.
    let mask = ipv4_mask_from_prefix(prefix);
    let dst = Ipv4Address::new((u32::from_be_bytes(dst) & mask).to_be_bytes());

    match request {
        SIOCADDRT => {
            let config = tx_subsystems::net::NetNamespaceRouteConfig {
                dst,
                prefix_len: prefix,
                gateway,
                oif_name,
                preferred_src: None,
                table: RT_TABLE_MAIN,
                protocol: RTPROT_BOOT,
                scope: if gateway.is_some() {
                    RT_SCOPE_UNIVERSE
                } else {
                    RT_SCOPE_LINK
                },
                route_type: RTN_UNICAST,
            };
            match netns.add_ipv4_route(auth, config) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        SIOCDELRT => {
            let selector = tx_subsystems::net::NetNamespaceRouteSelector {
                dst,
                prefix_len: prefix,
                gateway,
                oif_name,
                table: RT_TABLE_MAIN,
            };
            match netns.delete_ipv4_route(auth, selector) {
                Ok(()) => SyscallResult::Return(0),
                Err(Errno::ENOENT) => SyscallResult::Error(errno_to_i32(Errno::ESRCH)),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        _ => SyscallResult::Error(errno_to_i32(Errno::ENOTTY)),
    }
}

pub(super) fn sys_socket_ioctl<'a>(request: u32, argp: u64, ctx: &SyscallCtx<'a>) -> SyscallResult {
    const IFREQ_NAME_BYTES: usize = 16;
    const IFREQ_BYTES: usize = 40;
    const IFREQ_DATA_OFFSET: u64 = IFREQ_NAME_BYTES as u64;
    const IFCONF_BUF_OFFSET: u64 = 8;
    const AF_INET_U16: u16 = 2;
    const IFF_UP: i16 = 0x0001;
    const IFF_BROADCAST: i16 = 0x0002;
    const IFF_LOOPBACK: i16 = 0x0008;
    const IFF_RUNNING: i16 = 0x0040;
    const IFF_MULTICAST: i16 = 0x1000;

    if argp == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let Some(netns) = ctx.process.net_namespace() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    if request == SIOCGIFCONF {
        let ifc_len: i32 = match bootstrap_read_user(&ctx.aspace, argp) {
            Ok(len) => len,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let ifc_buf: u64 = match bootstrap_read_user(&ctx.aspace, argp + IFCONF_BUF_OFFSET) {
            Ok(buf) => buf,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        let links = netns.link_snapshot();
        let required_len = links.len().saturating_mul(IFREQ_BYTES);
        if ifc_buf == 0 || ifc_len <= 0 {
            return match bootstrap_write_user(&ctx.aspace, argp, required_len as i32) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            };
        }
        let writable_len = core::cmp::min(ifc_len as usize, required_len);
        let entry_count = writable_len / IFREQ_BYTES;
        let mut out = Vec::with_capacity(entry_count * IFREQ_BYTES);
        for link in links.into_iter().take(entry_count) {
            let mut ifreq = [0u8; IFREQ_BYTES];
            let name = link.name.as_bytes();
            let copy_len = core::cmp::min(name.len(), IFREQ_NAME_BYTES - 1);
            ifreq[..copy_len].copy_from_slice(&name[..copy_len]);
            ifreq[16..18].copy_from_slice(&AF_INET_U16.to_le_bytes());
            if let Some(addr) = link.ipv4_addr {
                ifreq[20..24].copy_from_slice(&addr.octets());
            }
            out.extend_from_slice(&ifreq);
        }
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, ifc_buf, &out) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        return match bootstrap_write_user(&ctx.aspace, argp, out.len() as i32) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }
    if request == SIOCGIFNAME {
        let ifindex: i32 = match bootstrap_read_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET) {
            Ok(ifindex) => ifindex,
            Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
        };
        if ifindex <= 0 {
            return SyscallResult::Error(ENXIO_VALUE);
        }
        let Some(link) = netns
            .link_snapshot()
            .into_iter()
            .find(|link| link.ifindex == ifindex as u32)
        else {
            return SyscallResult::Error(ENXIO_VALUE);
        };
        let mut out_name = [0u8; IFREQ_NAME_BYTES];
        let name = link.name.as_bytes();
        let copy_len = core::cmp::min(name.len(), IFREQ_NAME_BYTES - 1);
        out_name[..copy_len].copy_from_slice(&name[..copy_len]);
        return match bootstrap_write_user(&ctx.aspace, argp, out_name) {
            Ok(()) => SyscallResult::Return(0),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }

    if request == SIOCSARP || request == SIOCDARP {
        let mut arpreq = [0u8; ARPREQ_BYTES];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut arpreq, argp) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let ip = match parse_arpreq_ipv4(&arpreq) {
            Ok(ip) => ip,
            Err(errno) => return SyscallResult::Error(errno),
        };
        let dev = match parse_arpreq_device(&arpreq) {
            Ok(dev) => dev,
            Err(errno) => return SyscallResult::Error(errno),
        };
        let links = netns.link_snapshot();
        let link = if let Some(dev) = dev {
            links.iter().find(|link| link.name == dev)
        } else if let Some(route) = netns.best_ipv4_route(ip) {
            links.iter().find(|link| link.name == route.oif_name)
        } else {
            links
                .iter()
                .find(|link| !link.is_loopback && link.ipv4_addr.is_some())
        };
        let Some(link) = link else {
            return SyscallResult::Error(ENODEV_VALUE);
        };
        let auth = tx_subsystems::net::NetAdminAuthority::for_test_or_bootstrap();
        return match request {
            SIOCDARP => match netns.delete_static_neighbor_by_ifindex(auth, link.ifindex, ip) {
                Ok(()) => SyscallResult::Return(0),
                Err(Errno::ENOENT) => SyscallResult::Error(ENXIO_VALUE),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            },
            SIOCSARP => {
                let mac = match parse_arpreq_ethernet_addr(&arpreq) {
                    Ok(mac) => mac,
                    Err(errno) => return SyscallResult::Error(errno),
                };
                match netns.install_static_neighbor_by_ifindex(auth, link.ifindex, ip, mac) {
                    Ok(()) => SyscallResult::Return(0),
                    Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                }
            }
            _ => unreachable!(),
        };
    }

    if request == SIOCADDRT || request == SIOCDELRT {
        return socket_route_ioctl(request, argp, ctx);
    }

    let mut name_bytes = [0u8; IFREQ_NAME_BYTES];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut name_bytes, argp) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let end = name_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(IFREQ_NAME_BYTES);
    let Ok(ifname) = core::str::from_utf8(&name_bytes[..end]) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    let require_net_admin = || {
        if let Some(owner) = netns.owner_user_namespace() {
            let current = ctx.process.nsproxy_cap().ok_or(Errno::ESRCH)?;
            tx_subsystems::net::require_net_admin_in_user_namespace(
                ctx.cred(),
                &current.user_ns,
                &owner,
            )
        } else {
            tx_subsystems::net::require_net_admin(ctx.cred())
        }
    };
    // `eth0:1`-style names address a labeled IPv4 alias on the base link
    // (Linux strips the colon for link-level ioctls; the SIOC*IFADDR family
    // resolves the label against the per-address labels).
    let (base_name, alias_label) = match ifname.split_once(':') {
        Some((base, _)) if !base.is_empty() => (base, Some(ifname)),
        _ => (ifname, None),
    };
    let link = netns
        .link_snapshot()
        .into_iter()
        .find(|link| link.name == base_name);

    match request {
        SIOCGIFFLAGS => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            // A labeled alias is only visible while its address exists
            // (`ifconfig eth0:1` on a never-created alias is ENODEV).
            if let Some(label) = alias_label {
                if netns.ipv4_extra_by_label(link.ifindex, label).is_none() {
                    return SyscallResult::Error(ENODEV_VALUE);
                }
            }
            let mut flags = IFF_RUNNING;
            if link.is_up {
                flags |= IFF_UP;
            }
            if link.is_loopback {
                flags |= IFF_LOOPBACK;
            } else {
                flags |= IFF_BROADCAST | IFF_MULTICAST;
            }
            match bootstrap_write_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET, flags) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        SIOCSIFFLAGS => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            let auth = match require_net_admin() {
                Ok(auth) => auth,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let requested: i16 = match bootstrap_read_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET) {
                Ok(flags) => flags,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            if let Some(label) = alias_label {
                // Downing a labeled alias deletes its address (Linux devinet
                // semantics, what `ifconfig eth0:1 down` relies on); upping it
                // is a no-op ack — the address arrives via SIOCSIFADDR.
                if requested & IFF_UP == 0 {
                    return match netns.del_device_ipv4_addr_by_label(auth, link.ifindex, label) {
                        Ok(_) => SyscallResult::Return(0),
                        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                    };
                }
                return SyscallResult::Return(0);
            }
            match netns.set_device_up_by_ifindex(auth, link.ifindex, requested & IFF_UP != 0) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        SIOCGIFMTU => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            let mtu = i32::from(link.mtu);
            match bootstrap_write_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET, mtu) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        SIOCSIFMTU => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            let auth = match require_net_admin() {
                Ok(auth) => auth,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let requested: i32 = match bootstrap_read_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET) {
                Ok(mtu) => mtu,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let Ok(requested) = u16::try_from(requested) else {
                return SyscallResult::Error(EINVAL_VALUE);
            };
            match netns.set_device_mtu_by_ifindex(auth, link.ifindex, requested) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        SIOCGIFINDEX => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            let ifindex = link.ifindex as i32;
            match bootstrap_write_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET, ifindex) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        // `ifr_hwaddr` = `struct sockaddr { u16 sa_family; u8 sa_data[14] }`:
        // family ARPHRD_ETHER, then the 6-byte MAC. LTP's AF_PACKET injectors
        // (ns-icmpv4_sender for net_stress.broken_ip, ns-udpsender, …) read
        // the source MAC this way before building a raw frame; without it
        // get_ifinfo()'s ioctl fatal_errors and the whole broken_ip family
        // TFAILs at the sender.
        SIOCGIFHWADDR => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            let mut hwaddr = [0u8; 16];
            if link.is_loopback {
                hwaddr[0..2].copy_from_slice(&ARPHRD_LOOPBACK.to_le_bytes());
            } else {
                hwaddr[0..2].copy_from_slice(&ARPHRD_ETHER.to_le_bytes());
                if let Some(mac) = link.mac {
                    hwaddr[2..8].copy_from_slice(&mac.octets());
                }
            }
            match bootstrap_write_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET, hwaddr) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        // busybox `ifconfig IFACE ADDR netmask MASK broadcast BRD` drives
        // this trio in sequence; LTP net_stress.interface scripts depend
        // on it (`if4-addr-change` was TBROK `SIOCSIFADDR: Not a tty`).
        SIOCGIFADDR | SIOCGIFNETMASK | SIOCGIFBRDADDR => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            let (addr, prefix) = if let Some(label) = alias_label {
                match netns.ipv4_extra_by_label(link.ifindex, label) {
                    Some(found) => found,
                    None => return SyscallResult::Error(errno_to_i32(Errno::EADDRNOTAVAIL)),
                }
            } else {
                let Some(addr) = link.ipv4_addr else {
                    return SyscallResult::Error(errno_to_i32(Errno::EADDRNOTAVAIL));
                };
                let prefix = link
                    .ipv4_prefix_len
                    .unwrap_or_else(|| ipv4_classful_prefix(addr.octets()));
                (addr, prefix)
            };
            let mask = ipv4_mask_from_prefix(prefix);
            let value: [u8; 4] = match request {
                SIOCGIFADDR => addr.octets(),
                SIOCGIFNETMASK => mask.to_be_bytes(),
                SIOCGIFBRDADDR => (u32::from_be_bytes(addr.octets()) | !mask).to_be_bytes(),
                _ => unreachable!(),
            };
            // sockaddr_in image in ifr_addr: family, zero port, addr.
            let mut sin = [0u8; 8];
            sin[0..2].copy_from_slice(&AF_INET_U16.to_le_bytes());
            sin[4..8].copy_from_slice(&value);
            match bootstrap_write_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET, sin) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        SIOCSIFADDR | SIOCSIFNETMASK | SIOCSIFBRDADDR => {
            let Some(link) = link else {
                return SyscallResult::Error(ENODEV_VALUE);
            };
            let auth = match require_net_admin() {
                Ok(auth) => auth,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let sin: [u8; 8] = match bootstrap_read_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET) {
                Ok(sin) => sin,
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            };
            let family = u16::from_le_bytes(sin[0..2].try_into().unwrap());
            if family != AF_INET_U16 {
                return SyscallResult::Error(errno_to_i32(Errno::EAFNOSUPPORT));
            }
            let value: [u8; 4] = sin[4..8].try_into().unwrap();
            match request {
                SIOCSIFADDR => {
                    // Linux assumes a classful prefix until SIOCSIFNETMASK
                    // follows; keep an already-configured prefix instead so
                    // an addr-only change inside the same subnet holds.
                    if let Some(label) = alias_label {
                        // `ifconfig eth0:1 ADDR` creates/updates the labeled
                        // secondary; the primary keeps carrying traffic.
                        let prefix = netns
                            .ipv4_extra_by_label(link.ifindex, label)
                            .map(|(_, prefix)| prefix)
                            .unwrap_or_else(|| ipv4_classful_prefix(value));
                        return match netns.add_device_ipv4_addr_by_ifindex(
                            auth,
                            link.ifindex,
                            Ipv4Address::new(value),
                            prefix,
                            Some(label),
                        ) {
                            Ok(()) => SyscallResult::Return(0),
                            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                        };
                    }
                    let prefix = link
                        .ipv4_prefix_len
                        .unwrap_or_else(|| ipv4_classful_prefix(value));
                    match netns.set_device_ipv4_addr_by_ifindex(
                        auth,
                        link.ifindex,
                        Some(Ipv4Address::new(value)),
                        Some(prefix),
                    ) {
                        Ok(()) => SyscallResult::Return(0),
                        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
                SIOCSIFNETMASK => {
                    let Some(prefix) = ipv4_prefix_from_mask(u32::from_be_bytes(value)) else {
                        return SyscallResult::Error(EINVAL_VALUE);
                    };
                    if let Some(label) = alias_label {
                        let Some((addr, _)) = netns.ipv4_extra_by_label(link.ifindex, label) else {
                            return SyscallResult::Error(errno_to_i32(Errno::EADDRNOTAVAIL));
                        };
                        return match netns.add_device_ipv4_addr_by_ifindex(
                            auth,
                            link.ifindex,
                            addr,
                            prefix,
                            Some(label),
                        ) {
                            Ok(()) => SyscallResult::Return(0),
                            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                        };
                    }
                    let Some(addr) = link.ipv4_addr else {
                        return SyscallResult::Error(errno_to_i32(Errno::EADDRNOTAVAIL));
                    };
                    match netns.set_device_ipv4_addr_by_ifindex(
                        auth,
                        link.ifindex,
                        Some(addr),
                        Some(prefix),
                    ) {
                        Ok(()) => SyscallResult::Return(0),
                        Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
                SIOCSIFBRDADDR => {
                    // Broadcast is derived from addr/prefix in our model;
                    // accept and ack like Linux does for a matching value.
                    SyscallResult::Return(0)
                }
                _ => unreachable!(),
            }
        }
        SIOCGIFTXQLEN => {
            if link.is_none() {
                return SyscallResult::Error(ENODEV_VALUE);
            }
            let tx_queue_len = 0i32;
            match bootstrap_write_user(&ctx.aspace, argp + IFREQ_DATA_OFFSET, tx_queue_len) {
                Ok(()) => SyscallResult::Return(0),
                Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
            }
        }
        _ => SyscallResult::Error(errno_to_i32(Errno::ENOTTY)),
    }
}
