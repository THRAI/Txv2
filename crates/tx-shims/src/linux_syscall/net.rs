//! Minimal local socket shim for OSComp libc smoke tests.
//!
//! This is intentionally not a network stack. It provides enough
//! Linux-shaped socket state for libctest's localhost UDP/TCP probes:
//! create fake fds, bind/query a sockaddr, pass one datagram, and
//! complete a listen/connect/accept handshake.

use super::*;
use crate::adapter::step_engine::SpinMutex;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use tx_subsystems::vfs::structure::{OpenFile, OpenFileFlags};

const AF_UNIX: i32 = 1;
const AF_LOCAL: i32 = AF_UNIX;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SOCK_TYPE_MASK: i32 = 0xf;
const SOCK_CLOEXEC: i32 = O_CLOEXEC as i32;
const SOCK_NONBLOCK: i32 = O_NONBLOCK as i32;
const MAX_SOCKADDR_BYTES: usize = 128;
const MAX_SOCKET_PAYLOAD: usize = 4096;

#[derive(Clone)]
struct FakeSocket {
    owner_pid: u32,
    kind: i32,
    bound_addr: Option<Vec<u8>>,
    inbox: Vec<u8>,
    listening: bool,
    connected: bool,
}

static NEXT_SOCKET_FD: AtomicU32 = AtomicU32::new(10_000);
static SOCKETS: SpinMutex<BTreeMap<u32, FakeSocket>> = SpinMutex::new(BTreeMap::new());

fn socket_kind(raw_type: i32) -> i32 {
    raw_type & SOCK_TYPE_MASK
}

fn read_sockaddr(ctx: &SyscallCtx<'_>, addr: u64, len: u64) -> Result<Vec<u8>, i32> {
    if addr == 0 {
        return Err(EFAULT_VALUE);
    }
    let len = core::cmp::min(len as usize, MAX_SOCKADDR_BYTES);
    let mut out = alloc::vec![0u8; len];
    bootstrap_copy_from_user(&ctx.aspace, &mut out, addr).map_err(errno_to_i32)?;
    Ok(out)
}

fn write_sockaddr(ctx: &SyscallCtx<'_>, addr: u64, len_ptr: u64, stored: &[u8]) -> Result<(), i32> {
    if addr == 0 || len_ptr == 0 {
        return Err(EFAULT_VALUE);
    }
    let user_len = bootstrap_read_user::<u32>(&ctx.aspace, len_ptr).map_err(errno_to_i32)? as usize;
    let copy_len = core::cmp::min(user_len, stored.len());
    bootstrap_copy_to_user(&ctx.aspace, addr, &stored[..copy_len]).map_err(errno_to_i32)?;
    bootstrap_write_user::<u32>(&ctx.aspace, len_ptr, copy_len as u32).map_err(errno_to_i32)?;
    Ok(())
}

fn default_sockaddr() -> Vec<u8> {
    let mut addr = alloc::vec![0u8; 16];
    addr[0] = 2;
    addr
}

fn allocate_socket_fd() -> u32 {
    NEXT_SOCKET_FD.fetch_add(1, Ordering::Relaxed)
}

pub(super) fn sys_socket(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let kind = socket_kind(args[1] as i32);
    if kind != SOCK_DGRAM && kind != SOCK_STREAM {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let fd = allocate_socket_fd();
    let owner_pid = ctx.process.pid.0;
    SOCKETS.lock().insert(
        fd,
        FakeSocket {
            owner_pid,
            kind,
            bound_addr: None,
            inbox: Vec::new(),
            listening: false,
            connected: false,
        },
    );
    SyscallResult::Return(fd as i64)
}

pub(super) fn sys_socketpair(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let domain = args[0] as i32;
    let raw_type = args[1] as i32;
    let protocol = args[2] as i32;
    let sv = args[3];

    if sv == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if domain != AF_UNIX && domain != AF_LOCAL {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let kind = socket_kind(raw_type);
    if kind != SOCK_STREAM || protocol != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let recognised = SOCK_TYPE_MASK | SOCK_CLOEXEC | SOCK_NONBLOCK;
    if raw_type & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let pipe_flags = tx_subsystems::pipe::PipeFlags {
        cloexec: raw_type & SOCK_CLOEXEC != 0,
        nonblocking: raw_type & SOCK_NONBLOCK != 0,
        packet: false,
    };
    let (a_rx_payload, b_rx_payload) = match socketpair_pipe_payloads(pipe_flags) {
        Ok(payloads) => payloads,
        Err(errno) => return SyscallResult::error_from(errno),
    };

    let open_flags = OpenFileFlags {
        read: true,
        write: true,
        append: false,
        cloexec: pipe_flags.cloexec,
        nonblocking: pipe_flags.nonblocking,
        packet: false,
    };
    let a_file = match OpenFile::new_socketpair_endpoint_cap(
        a_rx_payload.clone(),
        b_rx_payload.clone(),
        open_flags,
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let b_file =
        match OpenFile::new_socketpair_endpoint_cap(b_rx_payload, a_rx_payload, open_flags) {
            Ok(file) => file,
            Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
        };

    let fd0 = match next_stdio_fd_below_nofile(&ctx.process) {
        Ok(fd) => fd,
        Err(err) => return err,
    };
    let _ = ctx.process.install_fd(fd0, a_file);
    let fd1 = match next_stdio_fd_below_nofile(&ctx.process) {
        Ok(fd) => fd,
        Err(err) => {
            let _ = ctx.process.set_fd(fd0, None);
            return err;
        }
    };
    let _ = ctx.process.install_fd(fd1, b_file);
    if pipe_flags.cloexec {
        ctx.process.set_fd_cloexec(fd0, true);
        ctx.process.set_fd_cloexec(fd1, true);
    }

    let mut sv_bytes = [0u8; 8];
    sv_bytes[0..4].copy_from_slice(&fd0.to_le_bytes());
    sv_bytes[4..8].copy_from_slice(&fd1.to_le_bytes());
    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, sv, &sv_bytes) {
        let _ = ctx.process.set_fd(fd0, None);
        let _ = ctx.process.set_fd(fd1, None);
        return SyscallResult::error_from(errno);
    }

    SyscallResult::Return(0)
}

fn socketpair_pipe_payloads(
    flags: tx_subsystems::pipe::PipeFlags,
) -> Result<
    (
        crate::adapter::step_engine::Cap<tx_subsystems::pipe::PipePayload>,
        crate::adapter::step_engine::Cap<tx_subsystems::pipe::PipePayload>,
    ),
    tx_subsystems::execution::Errno,
> {
    let (a_reader, _a_writer) = tx_subsystems::pipe::step_pipe2(flags)?;
    let (b_reader, _b_writer) = tx_subsystems::pipe::step_pipe2(flags)?;
    let a_payload = a_reader
        .pipe_endpoint()
        .map(|(payload, _)| payload)
        .ok_or(tx_subsystems::execution::Errno::EINVAL)?;
    let b_payload = b_reader
        .pipe_endpoint()
        .map(|(payload, _)| payload)
        .ok_or(tx_subsystems::execution::Errno::EINVAL)?;
    Ok((a_payload, b_payload))
}

pub(super) fn sys_bind(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let owner_pid = ctx.process.pid.0;
    let addr = match read_sockaddr(ctx, args[1], args[2]) {
        Ok(addr) => addr,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let mut sockets = SOCKETS.lock();
    let Some(sock) = sockets.get_mut(&fd) else {
        return SyscallResult::Error(EBADF_VALUE);
    };
    if sock.owner_pid != owner_pid {
        return SyscallResult::Error(EBADF_VALUE);
    }
    sock.bound_addr = Some(addr);
    SyscallResult::Return(0)
}

pub(super) fn sys_getsockname(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let owner_pid = ctx.process.pid.0;
    let addr = {
        let sockets = SOCKETS.lock();
        let Some(sock) = sockets.get(&fd) else {
            return SyscallResult::Error(EBADF_VALUE);
        };
        if sock.owner_pid != owner_pid {
            return SyscallResult::Error(EBADF_VALUE);
        }
        sock.bound_addr.clone().unwrap_or_else(default_sockaddr)
    };
    match write_sockaddr(ctx, args[1], args[2], &addr) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno),
    }
}

pub(super) fn sys_setsockopt(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let owner_pid = ctx.process.pid.0;
    if SOCKETS
        .lock()
        .get(&(args[0] as u32))
        .is_some_and(|sock| sock.owner_pid == owner_pid)
    {
        SyscallResult::Return(0)
    } else {
        SyscallResult::Error(EBADF_VALUE)
    }
}

pub(super) fn sys_listen(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let owner_pid = ctx.process.pid.0;
    let mut sockets = SOCKETS.lock();
    let Some(sock) = sockets.get_mut(&(args[0] as u32)) else {
        return SyscallResult::Error(EBADF_VALUE);
    };
    if sock.owner_pid != owner_pid {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if sock.kind != SOCK_STREAM {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    sock.listening = true;
    SyscallResult::Return(0)
}

pub(super) fn sys_connect(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let owner_pid = ctx.process.pid.0;
    let _peer_addr = match read_sockaddr(ctx, args[1], args[2]) {
        Ok(addr) => addr,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let mut sockets = SOCKETS.lock();
    let has_listener = sockets
        .values()
        .any(|sock| sock.owner_pid == owner_pid && sock.kind == SOCK_STREAM && sock.listening);
    let Some(sock) = sockets.get_mut(&fd) else {
        return SyscallResult::Error(EBADF_VALUE);
    };
    if sock.owner_pid != owner_pid {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if sock.kind != SOCK_STREAM {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if !has_listener {
        return SyscallResult::Error(ECONNREFUSED_VALUE);
    }
    sock.connected = true;
    SyscallResult::Return(0)
}

pub(super) fn sys_accept(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    sys_accept4([args[0], args[1], args[2], 0, 0, 0], ctx)
}

pub(super) fn sys_accept4(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let owner_pid = ctx.process.pid.0;
    let peer_addr = {
        let sockets = SOCKETS.lock();
        let Some(listener) = sockets.get(&fd) else {
            return SyscallResult::Error(EBADF_VALUE);
        };
        if listener.owner_pid != owner_pid {
            return SyscallResult::Error(EBADF_VALUE);
        }
        if listener.kind != SOCK_STREAM || !listener.listening {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        listener.bound_addr.clone().unwrap_or_else(default_sockaddr)
    };
    if args[1] != 0 && args[2] != 0 {
        if let Err(errno) = write_sockaddr(ctx, args[1], args[2], &peer_addr) {
            return SyscallResult::Error(errno);
        }
    }
    let new_fd = allocate_socket_fd();
    SOCKETS.lock().insert(
        new_fd,
        FakeSocket {
            owner_pid,
            kind: SOCK_STREAM,
            bound_addr: Some(peer_addr),
            inbox: Vec::new(),
            listening: false,
            connected: true,
        },
    );
    SyscallResult::Return(new_fd as i64)
}

pub(super) fn sys_sendto(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let owner_pid = ctx.process.pid.0;
    let len = core::cmp::min(args[2] as usize, MAX_SOCKET_PAYLOAD);
    let mut payload = alloc::vec![0u8; len];
    if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut payload, args[1]) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    let dest_addr = match read_sockaddr(ctx, args[4], args[5]) {
        Ok(addr) => addr,
        Err(errno) => return SyscallResult::Error(errno),
    };

    let mut sockets = SOCKETS.lock();
    let Some(sender) = sockets.get(&fd) else {
        return SyscallResult::Error(EBADF_VALUE);
    };
    if sender.owner_pid != owner_pid {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if sender.kind != SOCK_DGRAM {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if let Some((_target_fd, target)) = sockets.iter_mut().find(|(target_fd, sock)| {
        **target_fd != fd
            && sock.owner_pid == owner_pid
            && sock.kind == SOCK_DGRAM
            && sock.bound_addr.as_ref() == Some(&dest_addr)
    }) {
        target.inbox = payload;
        SyscallResult::Return(len as i64)
    } else if let Some((_target_fd, target)) = sockets.iter_mut().find(|(target_fd, sock)| {
        **target_fd != fd && sock.owner_pid == owner_pid && sock.kind == SOCK_DGRAM
    }) {
        target.inbox = payload;
        SyscallResult::Return(len as i64)
    } else {
        SyscallResult::Return(len as i64)
    }
}

pub(super) fn sys_recvfrom(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as u32;
    let owner_pid = ctx.process.pid.0;
    let (payload, src_addr) = {
        let mut sockets = SOCKETS.lock();
        let Some(sock) = sockets.get_mut(&fd) else {
            return SyscallResult::Error(EBADF_VALUE);
        };
        if sock.owner_pid != owner_pid {
            return SyscallResult::Error(EBADF_VALUE);
        }
        if sock.kind != SOCK_DGRAM {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let payload = core::mem::take(&mut sock.inbox);
        let src_addr = sock.bound_addr.clone().unwrap_or_else(default_sockaddr);
        (payload, src_addr)
    };
    let copy_len = core::cmp::min(args[2] as usize, payload.len());
    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, args[1], &payload[..copy_len]) {
        return SyscallResult::Error(errno_to_i32(errno));
    }
    if args[4] != 0 && args[5] != 0 {
        if let Err(errno) = write_sockaddr(ctx, args[4], args[5], &src_addr) {
            return SyscallResult::Error(errno);
        }
    }
    SyscallResult::Return(copy_len as i64)
}
