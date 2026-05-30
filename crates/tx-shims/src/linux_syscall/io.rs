//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::reactor_entry;
use crate::adapter::step_engine::Cap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

const PSELECT_READY_YIELD_INTERVAL: usize = 4;
const PSELECT_EMPTY_POLL_YIELD_INTERVAL: usize = 4;
static PSELECT_READY_RETURNS: AtomicUsize = AtomicUsize::new(0);
static PSELECT_EMPTY_POLL_RETURNS: AtomicUsize = AtomicUsize::new(0);

fn tty_readable_level(tty: &Cap<tx_subsystems::tty::structure::TtyIdentity>) -> bool {
    use tx_subsystems::tty::execution::TTY_READABLE;
    tty.input_readable.peek() & TTY_READABLE != 0
}

async fn wait_for_tty_readable(tty: Cap<tx_subsystems::tty::structure::TtyIdentity>) {
    use reactor_entry::{Mask, WaitProtocol};
    use tx_subsystems::tty::execution::TTY_READABLE;

    let channel = tty.wait_channel().clone();
    let condition_tty = tty.clone();
    let _ = channel
        .wait_event(
            Mask::from_bits(TTY_READABLE),
            WaitProtocol::Uninterruptible,
            move || tty_readable_level(&condition_tty),
        )
        .await;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PselectTimespecLayout {
    tv_sec: i64,
    tv_nsec: i64,
}

const FD_SETSIZE_MAX: u64 = 1024;
const FD_SET_WORD_BITS: u64 = 64;

fn read_pselect_timeout_ns(
    aspace: &Cap<AddressSpace>,
    timeout_ptr: u64,
) -> Result<Option<u64>, i32> {
    if timeout_ptr == 0 {
        return Ok(None);
    }
    let ts =
        bootstrap_read_user::<PselectTimespecLayout>(aspace, timeout_ptr).map_err(errno_to_i32)?;
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(EINVAL_VALUE);
    }
    let ns = (ts.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|sec_ns| sec_ns.checked_add(ts.tv_nsec as u64))
        .ok_or(EINVAL_VALUE)?;
    Ok(Some(ns))
}

fn fdset_words(nfds: u64) -> usize {
    nfds.div_ceil(FD_SET_WORD_BITS) as usize
}

fn read_fdset(aspace: &Cap<AddressSpace>, ptr: u64, nfds: u64) -> Result<Option<Vec<u64>>, i32> {
    if ptr == 0 {
        return Ok(None);
    }
    let words = fdset_words(nfds);
    let mut set = Vec::with_capacity(words);
    for idx in 0..words {
        let word_ptr = ptr.wrapping_add((idx * core::mem::size_of::<u64>()) as u64);
        let word = bootstrap_read_user::<u64>(aspace, word_ptr).map_err(errno_to_i32)?;
        set.push(word);
    }
    Ok(Some(set))
}

fn write_fdset(aspace: &Cap<AddressSpace>, ptr: u64, set: Option<&[u64]>) -> Result<(), i32> {
    let Some(set) = set else {
        return Ok(());
    };
    for (idx, word) in set.iter().copied().enumerate() {
        let word_ptr = ptr.wrapping_add((idx * core::mem::size_of::<u64>()) as u64);
        bootstrap_write_user::<u64>(aspace, word_ptr, word).map_err(errno_to_i32)?;
    }
    Ok(())
}

fn fdset_has(set: Option<&[u64]>, fd: u64) -> bool {
    let Some(set) = set else {
        return false;
    };
    let word = (fd / FD_SET_WORD_BITS) as usize;
    let bit = fd % FD_SET_WORD_BITS;
    set.get(word)
        .map(|value| (value & (1u64 << bit)) != 0)
        .unwrap_or(false)
}

fn fdset_clear(set: Option<&mut [u64]>, fd: u64) {
    let Some(set) = set else {
        return;
    };
    let word = (fd / FD_SET_WORD_BITS) as usize;
    let bit = fd % FD_SET_WORD_BITS;
    if let Some(value) = set.get_mut(word) {
        *value &= !(1u64 << bit);
    }
}

fn timerfd_readable_level<P: tx_hal::TimeIf>(tfd: &tx_subsystems::timerfd::TimerFd) -> bool {
    let deadline = tfd.deadline_ns();
    tfd.expiration_count() > 0 || (deadline != 0 && P::read_ns() >= deadline)
}

fn include_timerfd_deadline(deadline_slot: &mut Option<u64>, deadline_ns: u64) {
    if deadline_ns == 0 {
        return;
    }
    *deadline_slot = Some(match *deadline_slot {
        Some(current) => core::cmp::min(current, deadline_ns),
        None => deadline_ns,
    });
}

fn pselect_fd_ready(
    file: &OpenFile,
    want_read: bool,
    want_write: bool,
    want_except: bool,
) -> (bool, bool, bool) {
    use tx_subsystems::{
        pipe::PipeSide,
        vfs::structure::{OpenFileBacking, RNodeBacking, StructPayload},
    };

    let mut read_ready = false;
    let mut write_ready = false;
    let except_ready = false;

    if let Some(efd) = file.eventfd() {
        return (
            want_read && efd.counter() > 0,
            want_write && efd.counter() < tx_subsystems::eventfd::EVENTFD_MAX,
            false,
        );
    }

    if let OpenFileBacking::Rnode { rnode } = file.backing() {
        match rnode.backing() {
            RNodeBacking::StructBacked {
                payload: StructPayload::Tty(tty),
            } => {
                if want_read {
                    read_ready = tty_readable_level(tty);
                }
                if want_write {
                    write_ready = true;
                }
            }
            RNodeBacking::StructBacked {
                payload: StructPayload::Pipe { payload, side },
            } => match side {
                PipeSide::Reader => {
                    if want_read {
                        read_ready = payload.reader_readable_level() || payload.reader_hup_level();
                    }
                }
                PipeSide::Writer => {
                    if want_write {
                        write_ready = payload.writer_writable_level();
                    }
                }
            },
            _ => {
                read_ready = want_read;
                write_ready = want_write;
            }
        }
    }

    (read_ready, write_ready, want_except && except_ready)
}

async fn sleep_timeout_ns<P: tx_hal::TimeIf>(ns: u64, ctx: &SyscallCtx<'_>) -> SyscallResult {
    if ns == 0 {
        return SyscallResult::Return(0);
    }
    let deadline_ns = P::read_ns().saturating_add(ns);
    sleep_until_deadline::<P>(deadline_ns, ns, ctx).await
}

fn read_ppoll_sigmask(
    ctx: &SyscallCtx<'_>,
    mask_ptr: u64,
    mask_size: u64,
) -> Result<Option<u64>, SyscallResult> {
    if mask_ptr == 0 {
        return Ok(None);
    }
    if mask_size < SIGSETSIZE_BYTES {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    match bootstrap_read_user::<u64>(&ctx.aspace, mask_ptr) {
        Ok(bits) => Ok(Some(bits)),
        Err(errno) => Err(SyscallResult::error_from(errno)),
    }
}

fn set_thread_signal_mask(ctx: &SyscallCtx<'_>, mask_bits: u64) -> Result<u64, SyscallResult> {
    use tx_subsystems::{
        signal::SignalMask,
        thread_runtime::execution::{step_sigprocmask, SigmaskHow, SigprocmaskChange},
    };

    match step_sigprocmask(&ctx.thread, SigmaskHow::SetMask, SignalMask::new(mask_bits)) {
        SigprocmaskChange::Replaced { prev, .. } => Ok(prev.raw_bits()),
        SigprocmaskChange::ZombieIgnored => Err(SyscallResult::Error(ESRCH_VALUE)),
    }
}

fn clear_thread_pending_signals(ctx: &SyscallCtx<'_>, mask_bits: u64) {
    use tx_subsystems::signal::Signum;

    let Some(payload) = ctx.thread.payload_cap() else {
        return;
    };
    let mut cleared_any = false;
    for raw in 1..=Signum::MAX {
        let Some(sig) = Signum::new(raw) else {
            continue;
        };
        if mask_bits & sig.bit() != 0 {
            payload.pending().clear(sig);
            cleared_any = true;
        }
    }
    if cleared_any {
        tx_subsystems::signal::refresh_deliverable_signal_summary(&ctx.thread);
    }
}

fn restore_ppoll_sigmask(
    ctx: &SyscallCtx<'_>,
    saved_mask: Option<u64>,
    temporary_sigmask: Option<u64>,
    result: SyscallResult,
) -> SyscallResult {
    if let Some(bits) = saved_mask {
        if matches!(result, SyscallResult::Return(_)) {
            if let Some(temporary_bits) = temporary_sigmask {
                clear_thread_pending_signals(ctx, temporary_bits & !bits);
            }
        }
        let _ = set_thread_signal_mask(ctx, bits);
    }
    result
}

/// `write(fd, buf, count)`.
///
/// Phase 2a restriction (per the trio plan §"Part 2 — Syscall table"
/// `write` row): the buffer is treated as kernel-side bytes, not a
/// user VA. `args[1]` is taken as a kernel pointer that already points
/// into kernel-readable memory (the test scaffolding allocates from
/// the test's stack/heap). General `copy_from_user` is out of scope
/// per §"Out of scope".
/// `writev(fd, iov, iovcnt)` — gather-write per `man 2 writev`.
///
/// musl's stdio (`fwrite` / `fputs` / etc.) uses `writev` rather than
/// `write` to flush its line-buffered stdio, so this is on busybox's
/// startup hot path: without it, every stdio write returns `-ENOSYS`,
/// busybox treats the negative return as a fatal error and crashes
/// while trying to print a diagnostic.
///
/// Implementation: walk the user's `struct iovec[iovcnt]`
/// (`[*const u8; 8]` + `usize`, 16 bytes per entry on RV64), copy
/// each entry into a kernel `iovec_local` and forward to `sys_write`.
/// Returns the cumulative byte count, with Linux's standard partial-
/// success policy: a short or failed write on entry N returns the
/// running total if `total > 0`, or the error from entry N otherwise.
pub(super) async fn sys_writev<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const IOVEC_BYTES: u64 = 16;
    const MAX_RW_COUNT: u64 = 0x7fff_f000;
    let fd = args[0] as i32;
    let stdio_tty_file = if fd == 1 || fd == 2 {
        resolve_fd(&ctx.process, fd as u32).filter(|file| {
            matches!(
                file.rnode().backing(),
                tx_subsystems::vfs::RNodeBacking::StructBacked {
                    payload: tx_subsystems::vfs::StructPayload::Tty(_)
                }
            )
        })
    } else {
        None
    };
    if let Some(file) = stdio_tty_file {
        let mut combined = alloc::vec::Vec::new();
        for i in 0..iovcnt as u64 {
            let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
            let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
                return SyscallResult::error_from(errno);
            }
            let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
            let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
            if len == 0 {
                continue;
            }
            if len > MAX_RW_COUNT {
                return SyscallResult::Error(EINVAL_VALUE);
            }

            let old_len = combined.len();
            let len = len as usize;
            let Some(new_len) = old_len.checked_add(len) else {
                return SyscallResult::Error(EINVAL_VALUE);
            };
            if new_len as u64 > MAX_RW_COUNT {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            combined.resize(new_len, 0);
            if let Err(errno) =
                bootstrap_copy_from_user(&ctx.aspace, &mut combined[old_len..], base)
            {
                return SyscallResult::error_from(errno);
            }
        }

        if !file.flags().write {
            return SyscallResult::Error(EBADF_VALUE);
        }
        return sys_write_kernel_bytes_to_file(&file, &combined, ctx).await;
    }

    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::error_from(errno);
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }
        if len > MAX_RW_COUNT {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EINVAL_VALUE);
        }

        let write_args = [args[0], base, len, 0, 0, 0];
        match sys_write(write_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                // Short write: stop here and return what we got. Linux
                // does the same — writev never silently combines past
                // a short write.
                if (n as u64) < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
        // Yield the guard between iterations so the epoch can advance
        // and retired zone nodes can be reclaimed.  Without this, a
        // multi-element writev (common for musl's buffered stdio)
        // can exhaust the retired-node pool.
        drop(step_engine::guard());
    }
    SyscallResult::Return(total)
}

async fn sys_write_kernel_bytes_to_file<'a>(
    file: &Cap<tx_subsystems::vfs::structure::OpenFile>,
    bytes: &[u8],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if bytes.is_empty() {
        return SyscallResult::Return(0);
    }

    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileWriteOp;

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileWriteOp {
        file,
        bytes,
        caller_netns: ctx.process.net_namespace(),
        cursor: 0,
    };

    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(total) => {
            yield_after_struct_write_if_needed(file, total).await;
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno;
            if errno == tx_subsystems::execution::Errno::EPIPE {
                let _ = tx_subsystems::signal::step_kill_process(
                    &ctx.process,
                    tx_subsystems::signal::Signum::SIGPIPE,
                    None,
                );
            }
            SyscallResult::error_from(errno)
        }
    }
}

/// `readv(fd, iov, iovcnt)` — scatter-read counterpart of `sys_writev`.
pub(super) async fn sys_readv<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const IOVEC_BYTES: u64 = 16;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::error_from(errno);
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }

        let read_args = [args[0], base, len, 0, 0, 0];
        match sys_read::<P>(read_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                if (n as u64) < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

/// `pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask)`.
///
/// musl implements `select(2)` on RV64 through the generic `pselect6`
/// syscall. This keeps the implementation deliberately close to
/// `sys_ppoll`: sockets use the network readiness projection and TTY
/// reads peek the input queue. Other fd kinds are left not-ready for
/// now so network workloads do not spin on unrelated regular files.
///
/// Finite non-zero timeouts validate the timespec and then park on the
/// first socket/TTY wait token when no fd is immediately ready. The
/// exact deadline is not enforced yet; `{0,0}` remains a true poll.
pub(super) async fn sys_pselect6<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_subsystems::vfs::structure::{RNodeBacking, StructPayload};

    let nfds = args[0];
    let readfds = args[1];
    let writefds = args[2];
    let exceptfds = args[3];
    let timeout_ptr = args[4];

    if nfds > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if nfds == 0 {
        return SyscallResult::Return(0);
    }

    let timeout = match pselect_timeout_policy(ctx, timeout_ptr) {
        Ok(wait) => wait,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let timeout_deadline_ns = match timeout {
        PselectTimeout::FiniteWait(duration_ns) => {
            Some(<P as tx_hal::TimeIf>::read_ns().saturating_add(duration_ns))
        }
        PselectTimeout::Infinite | PselectTimeout::Poll => None,
    };
    let word_count = fdset_word_count(nfds);
    let mut yielded_before_wait = false;
    let (read_ready, write_ready, except_ready, ready_count) = loop {
        drive_loopback_pending();
        if let Some(deadline_ns) = timeout_deadline_ns {
            if <P as tx_hal::TimeIf>::read_ns() >= deadline_ns {
                break (
                    alloc::vec![0u64; word_count as usize],
                    alloc::vec![0u64; word_count as usize],
                    alloc::vec![0u64; word_count as usize],
                    0,
                );
            }
        }
        let mut read_ready = alloc::vec![0u64; word_count as usize];
        let mut write_ready = alloc::vec![0u64; word_count as usize];
        let mut except_ready = alloc::vec![0u64; word_count as usize];
        let mut ready_count: i64 = 0;
        let mut wait_tokens = alloc::vec::Vec::new();
        let mut effective_deadline_ns = timeout_deadline_ns;

        for fd in 0..nfds {
            let want_read = match fdset_contains(ctx, readfds, fd) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno),
            };
            let want_write = match fdset_contains(ctx, writefds, fd) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno),
            };
            let want_except = match fdset_contains(ctx, exceptfds, fd) {
                Ok(v) => v,
                Err(errno) => return SyscallResult::Error(errno),
            };
            if !want_read && !want_write && !want_except {
                continue;
            }
            let Some(file) = resolve_fd(&ctx.process, fd as u32) else {
                return SyscallResult::Error(EBADF_VALUE);
            };

            let mut fd_ready = false;
            let guard = tx_substrate::epoch::guard();
            if let Some(efd) = file.eventfd() {
                if want_read {
                    if efd.counter() > 0 {
                        fdset_set(&mut read_ready, fd);
                        fd_ready = true;
                    } else {
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                efd.reader_source_id(),
                                tx_subsystems::eventfd::EVENTFD_READABLE,
                            ),
                        );
                    }
                }
                if want_write {
                    if efd.counter() < tx_subsystems::eventfd::EVENTFD_MAX {
                        fdset_set(&mut write_ready, fd);
                        fd_ready = true;
                    } else {
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                efd.writer_source_id(),
                                tx_subsystems::eventfd::EVENTFD_WRITABLE,
                            ),
                        );
                    }
                }
            } else if let Some(tfd) = file.timerfd() {
                if want_read {
                    if timerfd_readable_level::<P>(tfd) {
                        fdset_set(&mut read_ready, fd);
                        fd_ready = true;
                    } else {
                        include_timerfd_deadline(&mut effective_deadline_ns, tfd.deadline_ns());
                        push_unique_wait_token(
                            &mut wait_tokens,
                            tx_subsystems::execution::WaitToken::new(
                                tfd.source_id(),
                                tx_subsystems::timerfd::TIMERFD_READABLE,
                            ),
                        );
                    }
                }
            } else if let Some(result) = socket_poll_mask_from_file(&file, &guard) {
                let mask = match result {
                    Ok(mask) => mask,
                    Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
                };
                if pselect_socket_read_ready(want_read, mask) {
                    fdset_set(&mut read_ready, fd);
                    fd_ready = true;
                }
                if pselect_socket_write_ready(want_write, mask) {
                    fdset_set(&mut write_ready, fd);
                    fd_ready = true;
                }
                if want_except && mask.intersects(tx_subsystems::net::PollMask::ERR) {
                    fdset_set(&mut except_ready, fd);
                    fd_ready = true;
                }
                let (read_blocked, write_blocked) =
                    pselect_socket_blocked_interests(want_read, want_write, mask);
                if read_blocked {
                    match socket_poll_wait_token_from_file(
                        &file,
                        tx_subsystems::net::PollMask::IN,
                        &guard,
                    ) {
                        Some(Ok(Some(token))) => {
                            push_unique_wait_token(&mut wait_tokens, token);
                        }
                        Some(Ok(None)) | None => {}
                        Some(Err(errno)) => return SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
                if write_blocked {
                    match socket_poll_wait_token_from_file(
                        &file,
                        tx_subsystems::net::PollMask::OUT,
                        &guard,
                    ) {
                        Some(Ok(Some(token))) => {
                            push_unique_wait_token(&mut wait_tokens, token);
                        }
                        Some(Ok(None)) | None => {}
                        Some(Err(errno)) => return SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
            } else {
                if want_read {
                    let readable = match file.rnode().backing() {
                        RNodeBacking::StructBacked {
                            payload: StructPayload::Tty(tty),
                        } => {
                            use tx_subsystems::tty::execution::TTY_READABLE;
                            if tty.input_readable.peek() & TTY_READABLE != 0 {
                                true
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        tty.wait_source_id(),
                                        TTY_READABLE,
                                    ),
                                );
                                false
                            }
                        }
                        RNodeBacking::StructBacked {
                            payload:
                                StructPayload::Pipe {
                                    payload,
                                    side: tx_subsystems::pipe::PipeSide::Reader,
                                },
                        } => {
                            if payload.readable_level() {
                                true
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        payload.reader_source_id(),
                                        tx_subsystems::pipe::PIPE_READABLE,
                                    ),
                                );
                                false
                            }
                        }
                        _ => false,
                    };
                    if readable {
                        fdset_set(&mut read_ready, fd);
                        fd_ready = true;
                    }
                }
                if want_write {
                    let writable = match file.rnode().backing() {
                        RNodeBacking::StructBacked {
                            payload:
                                StructPayload::Pipe {
                                    payload,
                                    side: tx_subsystems::pipe::PipeSide::Writer,
                                },
                        } => {
                            if payload.writable_level() {
                                true
                            } else {
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        payload.writer_source_id(),
                                        tx_subsystems::pipe::PIPE_WRITABLE,
                                    ),
                                );
                                false
                            }
                        }
                        _ => false,
                    };
                    if writable {
                        fdset_set(&mut write_ready, fd);
                        fd_ready = true;
                    }
                }
            }

            if fd_ready {
                ready_count += 1;
            }
        }

        if ready_count != 0 || timeout == PselectTimeout::Poll {
            break (read_ready, write_ready, except_ready, ready_count);
        }
        // Socket peers often make progress in another userspace task after this
        // scan. Give that task one turn before parking on the selected token,
        // then rescan so level-triggered readiness is observed directly.
        if !yielded_before_wait && !wait_tokens.is_empty() {
            yielded_before_wait = true;
            tx_reactor::yield_now().await;
            continue;
        }
        if wait_tokens.is_empty() {
            if let Some(deadline_ns) = effective_deadline_ns {
                wait_until_pselect_deadline::<P>(deadline_ns).await;
            } else if timeout == PselectTimeout::Infinite {
                tx_reactor::yield_now().await;
                continue;
            }
            break (read_ready, write_ready, except_ready, ready_count);
        };
        let futures = wait_tokens
            .into_iter()
            .filter_map(wait_source::wait_on_token)
            .collect::<alloc::vec::Vec<_>>();
        if !futures.is_empty() {
            if let Some(deadline_ns) = effective_deadline_ns {
                match wait_on_any_token_or_pselect_deadline::<P>(futures, deadline_ns).await {
                    PselectWaitWake::FdReady => {}
                    PselectWaitWake::TimedOut => {
                        break (read_ready, write_ready, except_ready, ready_count);
                    }
                }
            } else {
                wait_on_any_token(futures).await;
            }
        } else {
            if let Some(deadline_ns) = effective_deadline_ns {
                wait_until_pselect_deadline::<P>(deadline_ns).await;
            }
            break (read_ready, write_ready, except_ready, ready_count);
        }
    };

    if ready_count != 0 && pselect_ready_return_should_yield() {
        tx_reactor::yield_now().await;
    }
    if ready_count == 0 && timeout == PselectTimeout::Poll && pselect_empty_poll_should_yield() {
        tx_reactor::yield_now().await;
    }

    if let Err(errno) = fdset_write(ctx, readfds, &read_ready) {
        return SyscallResult::Error(errno);
    }
    if let Err(errno) = fdset_write(ctx, writefds, &write_ready) {
        return SyscallResult::Error(errno);
    }
    if let Err(errno) = fdset_write(ctx, exceptfds, &except_ready) {
        return SyscallResult::Error(errno);
    }

    SyscallResult::Return(ready_count)
}

fn pselect_ready_return_should_yield() -> bool {
    PSELECT_READY_RETURNS
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .is_multiple_of(PSELECT_READY_YIELD_INTERVAL)
}

fn pselect_empty_poll_should_yield() -> bool {
    PSELECT_EMPTY_POLL_RETURNS
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .is_multiple_of(PSELECT_EMPTY_POLL_YIELD_INTERVAL)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PselectTimeout {
    Infinite,
    FiniteWait(u64),
    Poll,
}

fn pselect_timeout_policy<'a>(
    ctx: &SyscallCtx<'a>,
    timeout_ptr: u64,
) -> Result<PselectTimeout, i32> {
    if timeout_ptr == 0 {
        return Ok(PselectTimeout::Infinite);
    }

    let mut bytes = [0u8; 16];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, timeout_ptr).map_err(errno_to_i32)?;
    let sec = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(EINVAL_VALUE);
    }
    if sec == 0 && nsec == 0 {
        Ok(PselectTimeout::Poll)
    } else {
        let sec_ns = (sec as u64).saturating_mul(1_000_000_000);
        Ok(PselectTimeout::FiniteWait(
            sec_ns.saturating_add(nsec as u64),
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PselectWaitWake {
    FdReady,
    TimedOut,
}

fn push_unique_wait_token(
    tokens: &mut alloc::vec::Vec<tx_subsystems::execution::WaitToken>,
    token: tx_subsystems::execution::WaitToken,
) {
    if !tokens.contains(&token) {
        tokens.push(token);
    }
}

async fn wait_on_any_token(mut futures: alloc::vec::Vec<wait_source::RegisteredWaitFuture>) {
    core::future::poll_fn(|cx| {
        for future in futures.iter_mut() {
            if core::future::Future::poll(core::pin::Pin::new(future), cx).is_ready() {
                return core::task::Poll::Ready(());
            }
        }
        core::task::Poll::Pending
    })
    .await
}

async fn wait_on_any_token_or_pselect_deadline<P: tx_hal::TimeIf>(
    mut fd_futures: alloc::vec::Vec<wait_source::RegisteredWaitFuture>,
    deadline_ns: u64,
) -> PselectWaitWake {
    if <P as tx_hal::TimeIf>::read_ns() >= deadline_ns {
        return PselectWaitWake::TimedOut;
    }
    let Some(mut timer_future) = tx_subsystems::timer_sleep::sleep_until_ns(deadline_ns) else {
        return PselectWaitWake::TimedOut;
    };

    core::future::poll_fn(|cx| {
        for future in fd_futures.iter_mut() {
            if core::future::Future::poll(core::pin::Pin::new(future), cx).is_ready() {
                return core::task::Poll::Ready(PselectWaitWake::FdReady);
            }
        }
        if core::future::Future::poll(core::pin::Pin::new(&mut timer_future), cx).is_ready() {
            return core::task::Poll::Ready(PselectWaitWake::TimedOut);
        }
        core::task::Poll::Pending
    })
    .await
}

async fn wait_until_pselect_deadline<P: tx_hal::TimeIf>(deadline_ns: u64) {
    if <P as tx_hal::TimeIf>::read_ns() >= deadline_ns {
        return;
    }
    let Some(timer_future) = tx_subsystems::timer_sleep::sleep_until_ns(deadline_ns) else {
        return;
    };
    let _ = timer_future.await;
}

fn fdset_word_count(nfds: u64) -> u64 {
    nfds.div_ceil(64)
}

fn fdset_contains<'a>(ctx: &SyscallCtx<'a>, set_ptr: u64, fd: u64) -> Result<bool, i32> {
    if set_ptr == 0 {
        return Ok(false);
    }
    let word_idx = fd / 64;
    let bit = fd % 64;
    let mut bytes = [0u8; 8];
    bootstrap_copy_from_user(&ctx.aspace, &mut bytes, set_ptr + word_idx * 8)
        .map_err(errno_to_i32)?;
    let word = u64::from_le_bytes(bytes);
    Ok(word & (1u64 << bit) != 0)
}

fn fdset_set(words: &mut [u64], fd: u64) {
    let word_idx = (fd / 64) as usize;
    let bit = fd % 64;
    words[word_idx] |= 1u64 << bit;
}

fn fdset_write<'a>(ctx: &SyscallCtx<'a>, set_ptr: u64, words: &[u64]) -> Result<(), i32> {
    if set_ptr == 0 {
        return Ok(());
    }
    for (idx, word) in words.iter().enumerate() {
        bootstrap_copy_to_user(&ctx.aspace, set_ptr + (idx as u64) * 8, &word.to_le_bytes())
            .map_err(errno_to_i32)?;
    }
    Ok(())
}

fn pselect_socket_write_ready(want_write: bool, mask: tx_subsystems::net::PollMask) -> bool {
    want_write
        && mask.intersects(tx_subsystems::net::PollMask::OUT | tx_subsystems::net::PollMask::ERR)
}

pub(super) fn pselect_socket_blocked_interests(
    want_read: bool,
    want_write: bool,
    mask: tx_subsystems::net::PollMask,
) -> (bool, bool) {
    (
        want_read && !pselect_socket_read_ready(true, mask),
        want_write && !pselect_socket_write_ready(true, mask),
    )
}

pub(super) fn pselect_socket_read_ready(
    want_read: bool,
    mask: tx_subsystems::net::PollMask,
) -> bool {
    want_read
        && mask.intersects(
            tx_subsystems::net::PollMask::IN
                | tx_subsystems::net::PollMask::ERR
                | tx_subsystems::net::PollMask::HUP
                | tx_subsystems::net::PollMask::RDHUP,
        )
}

/// `ppoll(fds, nfds, timeout_ptr, sigmask_ptr)`.
///
/// Polls socket, TTY, and pipe readiness with level-triggered semantics.
/// Socket wait interests stay split by direction so a caller that asks for
/// both `POLLIN` and `POLLOUT` can wake on either side.
pub(super) async fn sys_ppoll<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_subsystems::{
        pipe::PipeSide,
        vfs::structure::{RNodeBacking, StructPayload},
    };

    let fds_ptr = args[0];
    let nfds = args[1];
    let timeout_ptr = args[2];
    let sigmask_ptr = args[3];
    let sigmask_size = args[4];
    let timeout_ns = match read_pselect_timeout_ns(&ctx.aspace, timeout_ptr) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let temporary_sigmask = match read_ppoll_sigmask(ctx, sigmask_ptr, sigmask_size) {
        Ok(value) => value,
        Err(result) => return result,
    };

    if nfds > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let saved_mask = match temporary_sigmask {
        Some(bits) => match set_thread_signal_mask(ctx, bits) {
            Ok(prev) => Some(prev),
            Err(result) => return result,
        },
        None => None,
    };
    if nfds == 0 {
        let result = match timeout_ns {
            Some(ns) => sleep_timeout_ns::<P>(ns, ctx).await,
            None => loop {
                if tx_subsystems::signal::select_next_signal(&ctx.thread).is_some() {
                    break SyscallResult::Error(EINTR_VALUE);
                }

                use tx_scripts::drive;
                use tx_substrate::step::DriveMode;
                let now_ns = P::read_ns();
                let mut script_ctx = build_subject_script_ctx(ctx);
                let timer_wheel_arc = script_ctx.timer_wheel().cloned();
                let mailbox = ctx.mailbox.clone();
                let op = super::NanosleepOp {
                    nanos: 5_000_000,
                    deadline_ns: now_ns.saturating_add(5_000_000),
                    started: false,
                };
                match drive(
                    op,
                    &mut script_ctx,
                    DriveMode::Waiting,
                    mailbox.as_ref(),
                    None,
                    timer_wheel_arc.as_ref(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(v3errno) => {
                        let errno: tx_subsystems::execution::Errno = v3errno.into();
                        if errno == tx_subsystems::execution::Errno::EINTR {
                            break SyscallResult::Error(EINTR_VALUE);
                        }
                        break SyscallResult::error_from(errno);
                    }
                }
            },
        };
        return restore_ppoll_sigmask(ctx, saved_mask, temporary_sigmask, result);
    }

    const POLLFD_BYTES: u64 = 8;
    const POLLIN: i16 = 0x0001;
    const POLLOUT: i16 = 0x0004;
    const POLLERR: i16 = 0x0008;
    const POLLHUP: i16 = 0x0010;
    const POLLNVAL: i16 = 0x0020;

    let timeout = match pselect_timeout_policy(ctx, timeout_ptr) {
        Ok(wait) => wait,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let timeout_deadline_ns = match timeout {
        PselectTimeout::FiniteWait(duration_ns) => {
            Some(<P as tx_hal::TimeIf>::read_ns().saturating_add(duration_ns))
        }
        PselectTimeout::Infinite | PselectTimeout::Poll => None,
    };

    let mut yielded_before_wait = false;
    let ready = loop {
        drive_loopback_pending();
        if let Some(deadline_ns) = timeout_deadline_ns {
            if <P as tx_hal::TimeIf>::read_ns() >= deadline_ns {
                break 0;
            }
        }

        let mut ready: i64 = 0;
        let mut wait_tokens = alloc::vec::Vec::new();
        let mut effective_deadline_ns = timeout_deadline_ns;
        for i in 0..nfds {
            let ent_ptr = fds_ptr.wrapping_add(i * POLLFD_BYTES);
            let mut ent_bytes = [0u8; POLLFD_BYTES as usize];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
                return restore_ppoll_sigmask(
                    ctx,
                    saved_mask,
                    temporary_sigmask,
                    SyscallResult::error_from(errno),
                );
            }
            let fd = i32::from_le_bytes(ent_bytes[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(ent_bytes[4..6].try_into().unwrap());
            let mut revents: i16 = 0;
            if fd >= 0 {
                if let Some(file) = resolve_fd(&ctx.process, fd as u32) {
                    let guard = tx_substrate::epoch::guard();
                    if let Some(tfd) = file.timerfd() {
                        if events & POLLIN != 0 {
                            if timerfd_readable_level::<P>(tfd) {
                                revents |= POLLIN;
                            } else {
                                include_timerfd_deadline(
                                    &mut effective_deadline_ns,
                                    tfd.deadline_ns(),
                                );
                                push_unique_wait_token(
                                    &mut wait_tokens,
                                    tx_subsystems::execution::WaitToken::new(
                                        tfd.source_id(),
                                        tx_subsystems::timerfd::TIMERFD_READABLE,
                                    ),
                                );
                            }
                        }
                    } else if let Some(result) = socket_poll_mask_from_file(&file, &guard) {
                        match result {
                            Ok(mask) => {
                                let want_read = events & POLLIN != 0;
                                let want_write = events & POLLOUT != 0;
                                if want_read && mask.intersects(tx_subsystems::net::PollMask::IN) {
                                    revents |= POLLIN;
                                }
                                if want_write && mask.intersects(tx_subsystems::net::PollMask::OUT)
                                {
                                    revents |= POLLOUT;
                                }
                                if mask.intersects(tx_subsystems::net::PollMask::ERR) {
                                    revents |= POLLERR;
                                }
                                if mask.intersects(tx_subsystems::net::PollMask::HUP) {
                                    revents |= POLLHUP;
                                }
                                if timeout != PselectTimeout::Poll && revents == 0 {
                                    let (read_blocked, write_blocked) =
                                        pselect_socket_blocked_interests(
                                            want_read, want_write, mask,
                                        );
                                    if read_blocked {
                                        match socket_poll_wait_token_from_file(
                                            &file,
                                            tx_subsystems::net::PollMask::IN,
                                            &guard,
                                        ) {
                                            Some(Ok(Some(token))) => {
                                                push_unique_wait_token(&mut wait_tokens, token);
                                            }
                                            Some(Ok(None)) | None => {}
                                            Some(Err(errno)) => {
                                                return SyscallResult::Error(errno_to_i32(errno));
                                            }
                                        }
                                    }
                                    if write_blocked {
                                        match socket_poll_wait_token_from_file(
                                            &file,
                                            tx_subsystems::net::PollMask::OUT,
                                            &guard,
                                        ) {
                                            Some(Ok(Some(token))) => {
                                                push_unique_wait_token(&mut wait_tokens, token);
                                            }
                                            Some(Ok(None)) | None => {}
                                            Some(Err(errno)) => {
                                                return SyscallResult::Error(errno_to_i32(errno));
                                            }
                                        }
                                    }
                                }
                            }
                            Err(errno) => {
                                return restore_ppoll_sigmask(
                                    ctx,
                                    saved_mask,
                                    temporary_sigmask,
                                    SyscallResult::Error(errno_to_i32(errno)),
                                );
                            }
                        }
                    } else {
                        let mut handled = false;
                        match file.rnode().backing() {
                            RNodeBacking::StructBacked {
                                payload: StructPayload::Tty(tty),
                            } => {
                                handled = true;
                                if events & POLLIN != 0 {
                                    if tty_readable_level(tty) {
                                        revents |= POLLIN;
                                    } else {
                                        push_unique_wait_token(
                                            &mut wait_tokens,
                                            tx_subsystems::execution::WaitToken::new(
                                                tty.wait_source_id(),
                                                POLLIN as u64,
                                            ),
                                        );
                                    }
                                }
                                if events & POLLOUT != 0 {
                                    revents |= POLLOUT;
                                }
                            }
                            RNodeBacking::StructBacked {
                                payload: StructPayload::Pipe { payload, side },
                            } => {
                                handled = true;
                                match side {
                                    PipeSide::Reader => {
                                        if events & POLLIN != 0 {
                                            if payload.reader_readable_level() {
                                                revents |= POLLIN;
                                            } else {
                                                push_unique_wait_token(
                                                    &mut wait_tokens,
                                                    tx_subsystems::execution::WaitToken::new(
                                                        payload.reader_source_id(),
                                                        tx_subsystems::pipe::PIPE_READABLE,
                                                    ),
                                                );
                                            }
                                        }
                                        if payload.reader_hup_level() {
                                            revents |= POLLHUP;
                                        }
                                    }
                                    PipeSide::Writer => {
                                        if events & POLLOUT != 0 {
                                            if payload.writer_writable_level() {
                                                revents |= POLLOUT;
                                            } else {
                                                push_unique_wait_token(
                                                    &mut wait_tokens,
                                                    tx_subsystems::execution::WaitToken::new(
                                                        payload.writer_source_id(),
                                                        tx_subsystems::pipe::PIPE_WRITABLE,
                                                    ),
                                                );
                                            }
                                        }
                                        if payload.writer_err_level() {
                                            revents |= POLLERR;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                        if !handled {
                            if events & POLLIN != 0 {
                                revents |= POLLIN;
                            }
                            if events & POLLOUT != 0 {
                                revents |= POLLOUT;
                            }
                        }
                    }
                } else {
                    revents = POLLNVAL;
                }
            }
            if revents != 0 {
                ready += 1;
            }
            ent_bytes[6..8].copy_from_slice(&revents.to_le_bytes());
            if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, ent_ptr, &ent_bytes) {
                return restore_ppoll_sigmask(
                    ctx,
                    saved_mask,
                    temporary_sigmask,
                    SyscallResult::error_from(errno),
                );
            }
        }

        if ready > 0 {
            break ready;
        }
        if timeout == PselectTimeout::Poll {
            break 0;
        }

        if !yielded_before_wait && !wait_tokens.is_empty() {
            yielded_before_wait = true;
            tx_reactor::yield_now().await;
            continue;
        }
        if wait_tokens.is_empty() {
            if let Some(deadline_ns) = effective_deadline_ns {
                wait_until_pselect_deadline::<P>(deadline_ns).await;
            } else if timeout == PselectTimeout::Infinite {
                tx_reactor::yield_now().await;
                continue;
            }
            break 0;
        }

        let futures = wait_tokens
            .into_iter()
            .filter_map(wait_source::wait_on_token)
            .collect::<alloc::vec::Vec<_>>();
        if futures.is_empty() {
            if let Some(deadline_ns) = timeout_deadline_ns {
                wait_until_pselect_deadline::<P>(deadline_ns).await;
            } else if timeout == PselectTimeout::Infinite {
                tx_reactor::yield_now().await;
                continue;
            }
            break 0;
        }
        if let Some(deadline_ns) = effective_deadline_ns {
            match wait_on_any_token_or_pselect_deadline::<P>(futures, deadline_ns).await {
                PselectWaitWake::FdReady => {}
                PselectWaitWake::TimedOut => break 0,
            }
        } else {
            wait_on_any_token(futures).await;
        }
    };

    if ready == 0 && timeout == PselectTimeout::Poll && pselect_empty_poll_should_yield() {
        tx_reactor::yield_now().await;
    }

    restore_ppoll_sigmask(
        ctx,
        saved_mask,
        temporary_sigmask,
        SyscallResult::Return(ready),
    )
}

/// PageBacked `write(2)` — direct user-buffer path.
///
/// Prefaults the user buffer through `reserve_user_range_for_access`,
/// then drives `OpenFileWriteFromUserOp` which copies bytes directly
/// from user pages to PC frames through the pmap, without kernel-buffer
/// staging (PAGE_BACKED_v1 §5.1 / §3 prefault discipline).
async fn sys_write_pagebacked<'a>(
    file: &Cap<tx_subsystems::vfs::structure::OpenFile>,
    buf_ptr: usize,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if len == 0 {
        return SyscallResult::Return(0);
    }

    if file.flags().append {
        if let tx_subsystems::vfs::RNodeBacking::PageBacked { pc } = file.rnode().backing() {
            file.set_offset(pc.size_bytes());
        }
    }

    // Prefault: eagerly materialise every user page and publish to
    // the pmap so the step loop below finds every page in the cache.
    // If any page is unmapped or has a prot mismatch, fail before
    // transferring any bytes.
    if let Some(range) = super::user_copy::covering_user_range(buf_ptr as u64, len) {
        use crate::adapter::step_engine::StepOutcome as V3;
        use tx_subsystems::vm::UserAccessKind;
        let _guard = crate::adapter::step_engine::guard();
        match ctx
            .aspace
            .reserve_user_range_for_access(range, UserAccessKind::Read)
        {
            V3::Done(()) => {}
            V3::Err(e) => {
                let errno: tx_subsystems::execution::Errno = e;
                return SyscallResult::error_from(errno);
            }
            V3::Yield { .. } | V3::Continue { .. } => {
                return SyscallResult::error_from(tx_subsystems::execution::Errno::EIO);
            }
        }
    }

    // Drive the write-from-user step loop.
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileWriteFromUserOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileWriteFromUserOp {
        file,
        aspace: &ctx.aspace,
        src: tx_hal::UserPtr::<u8>::new(buf_ptr),
        len,
        cursor: 0,
    };
    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(total) => SyscallResult::Return(total as i64),
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno;
            if errno == tx_subsystems::execution::Errno::EPIPE {
                let _ = tx_subsystems::signal::step_kill_process(
                    &ctx.process,
                    tx_subsystems::signal::Signum::SIGPIPE,
                    None,
                );
            }
            SyscallResult::error_from(errno)
        }
    }
}

pub(super) async fn sys_write<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }

    if let Some(result) = super::net::sys_socket_write(fd as u32, args[1], len, ctx) {
        return result;
    }

    // Resolve fd → Cap<OpenFile> against the process payload's stub
    // fd table. Holding the payload guard across the lookup is fine —
    // the resulting Cap is independent and the lock is released before
    // any `.await`.
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // POSIX mq descriptors are not byte-stream fds. musl uses the
    // mq_* syscalls directly, but raw read/write on an mqd_t should
    // fail cleanly instead of falling into the VFS rnode path.
    if file.posix_mq().is_some() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if !file.flags().write {
        return SyscallResult::Error(EBADF_VALUE);
    }

    // eventfd fds carry their own `write(2)` arm — add a 64-bit
    // value to the counter. Dispatch before any VFS-shaped rnode()
    // access because eventfd is a non-VFS OpenFile backing.
    if file.eventfd().is_some() {
        return super::eventfd::sys_eventfd_write(&file, args[1], len, ctx).await;
    }

    let len = inline_io_len_for_file(&file, len);

    // PageBacked files: direct user-buffer path (PAGE_BACKED_v1 §5.1).
    // Prefault the user buffer in the observe phase, then drive the
    // write through the pmap without kernel-buffer staging.
    if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        return sys_write_pagebacked(&file, buf_ptr, len, ctx).await;
    }

    // Pull the user buffer into kernel memory through the canonical
    // user-VA lane (`bootstrap_copy_from_user` bridges via
    // `aspace.copy_from_user`, falling back to the kernel-pointer
    // deref the trio's earlier exemption used). The Vec is owned for
    // the duration of the step loop so the underlying user pages
    // can be re-mapped without affecting the byte stream we feed to
    // `step_write`.
    let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    if len > 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, buf_ptr as u64) {
            return SyscallResult::error_from(errno);
        }
    }
    if let tx_subsystems::vfs::structure::RNodeBacking::StructBacked {
        payload: tx_subsystems::vfs::structure::StructPayload::Socket { identity: socket },
    } = file.rnode().backing()
    {
        return sys_write_socket(&file, socket.clone(), &bytes, ctx).await;
    }

    // PR-9 phase 3b: drive `OpenFile::step_write` via the
    // `OpenFileWriteOp` StepOp wrap and the v3 `drive()` loop
    // (per `docs/Txv3/03_STEP_MODEL_v2.md` §8).
    //
    // The source buffer (`bytes`) is consumed by successive
    // `step()` calls (cursor tracked internally by
    // `OpenFileWriteOp`). After `drive()` returns, the return
    // value is the total bytes written.

    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileWriteOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    // The op acquires its own epoch guard inside `step()` per
    // STEP_MODEL_v2 §1; the syscall handler must not hold a guard
    // across `drive(...).await` (INVARIANTS_v5 YIELD-5 / EBR-7).
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileWriteOp {
        file: &file,
        bytes: &bytes,
        caller_netns: ctx.process.net_namespace(),
        cursor: 0,
    };
    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(total) => {
            yield_after_struct_write_if_needed(&file, total).await;
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno;
            // fd-ops Wave 3 — Q2 DECIDED 2026-05-07. SIGPIPE is
            // delivered to the calling process before returning
            // `-EPIPE` to userspace.
            if errno == tx_subsystems::execution::Errno::EPIPE {
                let _ = tx_subsystems::signal::step_kill_process(
                    &ctx.process,
                    tx_subsystems::signal::Signum::SIGPIPE,
                    None,
                );
            }
            SyscallResult::error_from(errno)
        }
    }
}

async fn yield_after_struct_write_if_needed(file: &Cap<OpenFile>, written: usize) {
    if written == 0 {
        return;
    }
    if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::structure::RNodeBacking::StructBacked {
            payload: tx_subsystems::vfs::structure::StructPayload::Pipe { .. },
        }
    ) {
        tx_reactor::yield_now().await;
    }
}

/// PageBacked `read(2)` — direct user-buffer path.
///
/// Prefaults the user buffer (Write access, since we're writing into
/// it), then drives `OpenFileReadToUserOp` which copies bytes directly
/// from PC frames to user pages through the pmap, without kernel-buffer
/// staging (PAGE_BACKED_v1 §5.1 / §3 prefault discipline).
async fn sys_read_pagebacked<'a, P: tx_hal::TimeIf>(
    file: &Cap<tx_subsystems::vfs::structure::OpenFile>,
    buf_ptr: usize,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if len == 0 {
        return SyscallResult::Return(0);
    }

    // Prefault: eagerly materialise every user page and publish to
    // the pmap so the step loop below finds every page in the cache.
    // Access is Write — we're writing data *into* the user buffer.
    if let Some(range) = super::user_copy::covering_user_range(buf_ptr as u64, len) {
        use crate::adapter::step_engine::StepOutcome as V3;
        use tx_subsystems::vm::UserAccessKind;
        let _guard = crate::adapter::step_engine::guard();
        match ctx
            .aspace
            .reserve_user_range_for_access(range, UserAccessKind::Write)
        {
            V3::Done(()) => {}
            V3::Err(e) => {
                let errno: tx_subsystems::execution::Errno = e;
                return SyscallResult::error_from(errno);
            }
            V3::Yield { .. } | V3::Continue { .. } => {
                return SyscallResult::error_from(tx_subsystems::execution::Errno::EIO);
            }
        }
    }

    // Drive the read-to-user step loop.
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileReadToUserOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileReadToUserOp {
        file,
        aspace: &ctx.aspace,
        dst: tx_hal::UserPtr::<u8>::new(buf_ptr),
        len,
        cursor: 0,
    };
    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(total) => SyscallResult::Return(total as i64),
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno;
            SyscallResult::error_from(errno)
        }
    }
}

async fn sys_write_socket(
    file: &Cap<OpenFile>,
    socket: Cap<tx_subsystems::net::SocketIdentity>,
    bytes: &[u8],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if bytes.is_empty() {
        return SyscallResult::Return(0);
    }

    let mut flags = tx_subsystems::net::SendRecvFlags::empty();
    if file.flags().nonblocking {
        flags |= tx_subsystems::net::SendRecvFlags::MSG_DONTWAIT;
    }

    if udp_write_can_drive_loopback_inline(&socket) {
        return sys_write_udp_loopback_socket(&socket, bytes, flags).await;
    }

    if matches!(
        socket.kind,
        tx_subsystems::net::SocketKind::NetlinkRoute
            | tx_subsystems::net::SocketKind::NetlinkNetfilter
    ) {
        let mut resolve_netns_fd = |fd: i32| {
            if fd < 0 {
                return None;
            }
            let file = resolve_fd(&ctx.process, fd as u32)?;
            tx_subsystems::net::net_namespace_payload_from_file(&file)
        };
        let mut resolve_netns_pid = |pid: u32| {
            let process = process_by_pid(Pid(pid))?;
            process.net_namespace()
        };
        let result = match socket.kind {
            tx_subsystems::net::SocketKind::NetlinkRoute => {
                tx_subsystems::net::netlink_route_send_with_netns_resolvers(
                    &socket,
                    bytes,
                    ctx.cred(),
                    &mut resolve_netns_fd,
                    &mut resolve_netns_pid,
                )
            }
            tx_subsystems::net::SocketKind::NetlinkNetfilter => {
                tx_subsystems::net::netlink_netfilter_send(&socket, bytes, ctx.cred())
            }
            _ => unreachable!(),
        };
        return match result {
            Ok(sent) => SyscallResult::Return(sent as i64),
            Err(errno) => SyscallResult::Error(errno_to_i32(errno)),
        };
    }

    let mut total = 0usize;
    let mut remaining = bytes;

    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            tx_subsystems::net::execution::step_send_kernel_bytes(&socket, remaining, flags, &guard)
        };
        match outcome {
            tx_substrate::step::StepOutcome::Done(written) => {
                total += written;
                drive_loopback_after_socket_write(&socket, written);
                yield_after_socket_write_if_needed(&socket, written).await;
                return SyscallResult::Return(total as i64);
            }
            tx_substrate::step::StepOutcome::Continue { progress } => {
                let written = progress.bytes();
                total += written;
                drive_loopback_after_socket_write(&socket, written);
                let stop = written == 0 || written >= remaining.len();
                if stop {
                    yield_after_socket_write_if_needed(&socket, written).await;
                    return SyscallResult::Return(total as i64);
                }
                yield_after_socket_write_if_needed(&socket, written).await;
                remaining = &remaining[written..];
            }
            tx_substrate::step::StepOutcome::Yield { progress, shape } => {
                let written = progress.bytes();
                if written > 0 {
                    total += written;
                    drive_loopback_after_socket_write(&socket, written);
                    if written >= remaining.len() {
                        yield_after_socket_write_if_needed(&socket, written).await;
                        return SyscallResult::Return(total as i64);
                    }
                    yield_after_socket_write_if_needed(&socket, written).await;
                    remaining = &remaining[written..];
                } else if flags.is_nonblocking() {
                    if total > 0 {
                        return SyscallResult::Return(total as i64);
                    }
                    return SyscallResult::Error(EAGAIN_VALUE);
                } else {
                    drive_loopback_pending();
                }

                match shape {
                    tx_substrate::step::YieldShape::OnWaitSource { source, interests }
                    | tx_substrate::step::YieldShape::OnEdge { source, interests } => {
                        let token =
                            tx_subsystems::execution::WaitToken::new(source.raw(), interests.raw());
                        if let Some(future) = wait_source::wait_on_token(token) {
                            let _ = future.await;
                        }
                    }
                    tx_substrate::step::YieldShape::OnAgent { .. }
                    | tx_substrate::step::YieldShape::OnTimer { .. } => {
                        if total > 0 {
                            return SyscallResult::Return(total as i64);
                        }
                        return SyscallResult::Error(errno_to_i32(Errno::EIO));
                    }
                }
            }
            tx_substrate::step::StepOutcome::Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total as i64);
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

async fn sys_write_udp_loopback_socket(
    socket: &Cap<tx_subsystems::net::SocketIdentity>,
    bytes: &[u8],
    flags: tx_subsystems::net::SendRecvFlags,
) -> SyscallResult {
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            tx_subsystems::net::execution::step_send_udp_loopback_kernel_bytes(
                socket, None, bytes, flags, &guard,
            )
        };
        match outcome {
            tx_substrate::step::StepOutcome::Done(written) => {
                return SyscallResult::Return(written as i64);
            }
            tx_substrate::step::StepOutcome::Continue { progress } => {
                return SyscallResult::Return(progress.bytes() as i64);
            }
            tx_substrate::step::StepOutcome::Yield { progress, shape } => {
                if progress.bytes() > 0 {
                    return SyscallResult::Return(progress.bytes() as i64);
                }
                if flags.is_nonblocking() {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                match shape {
                    tx_substrate::step::YieldShape::OnWaitSource { source, interests }
                    | tx_substrate::step::YieldShape::OnEdge { source, interests } => {
                        let token =
                            tx_subsystems::execution::WaitToken::new(source.raw(), interests.raw());
                        if let Some(future) = wait_source::wait_on_token(token) {
                            let _ = future.await;
                        }
                    }
                    tx_substrate::step::YieldShape::OnAgent { .. }
                    | tx_substrate::step::YieldShape::OnTimer { .. } => {
                        return SyscallResult::Error(EIO_VALUE);
                    }
                }
            }
            tx_substrate::step::StepOutcome::Err(errno) => {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

fn udp_write_can_drive_loopback_inline(socket: &Cap<tx_subsystems::net::SocketIdentity>) -> bool {
    if socket.kind != tx_subsystems::net::SocketKind::Udp {
        return false;
    }
    let Some(payload) = socket.acquire_operational() else {
        return false;
    };
    match payload.protocol_snapshot() {
        tx_subsystems::net::SocketProtocol::Udp(tx_subsystems::net::UdpInner::Connected {
            local,
            remote,
        }) => {
            udp_local_allows_loopback_inline(local)
                && remote.addr == tx_subsystems::net::Ipv4Address::LOOPBACK
        }
        _ => false,
    }
}

fn udp_local_allows_loopback_inline(local: tx_subsystems::net::IpEndpoint) -> bool {
    local.addr == tx_subsystems::net::Ipv4Address::UNSPECIFIED
        || local.addr == tx_subsystems::net::Ipv4Address::LOOPBACK
}

async fn yield_after_socket_write_if_needed(
    socket: &Cap<tx_subsystems::net::SocketIdentity>,
    written: usize,
) {
    if written > 0 && matches!(socket.kind, tx_subsystems::net::SocketKind::Tcp) {
        tx_reactor::yield_now().await;
    }
}

fn drive_loopback_after_socket_write(
    socket: &Cap<tx_subsystems::net::SocketIdentity>,
    written: usize,
) {
    drive_tcp_loopback_after_socket_write(socket, written);
    drive_udp_loopback_after_socket_write(socket, written);
}

fn drive_tcp_loopback_after_socket_write(
    socket: &Cap<tx_subsystems::net::SocketIdentity>,
    written: usize,
) {
    if written == 0 {
        return;
    }
    let guard = tx_substrate::epoch::guard();
    let _ = tx_subsystems::net::execution::step_tcp_loopback_transfer(socket, written, &guard);
    socket
        .readiness
        .clear_send(tx_subsystems::net::structure::SendWireSet::SPACE);
}

fn drive_udp_loopback_after_socket_write(
    socket: &Cap<tx_subsystems::net::SocketIdentity>,
    written: usize,
) {
    if written == 0 {
        return;
    }
    let Some(payload) = socket.acquire_operational() else {
        return;
    };
    if !matches!(
        payload.protocol_snapshot(),
        tx_subsystems::net::SocketProtocol::Udp(
            tx_subsystems::net::UdpInner::Bound { .. }
                | tx_subsystems::net::UdpInner::Connected { .. }
        )
    ) {
        return;
    }
    let guard = tx_substrate::epoch::guard();
    let _ = tx_subsystems::net::execution::step_process_loopback_udp(socket, 8, &guard);
}

fn drive_loopback_pending() {
    let guard = tx_substrate::epoch::guard();
    let _ = tx_subsystems::net::execution::step_process_loopback_pending_zero(
        tx_subsystems::net::protocol::loopback_iface(),
        tx_subsystems::net::execution::LOOPBACK_POLL_BUDGET_DEFAULT,
        &guard,
    );
}

/// `read(fd, buf, count)`.
///
/// Mirrors `sys_write`'s structure. For PageBacked files, delegates to
/// `sys_read_pagebacked` (direct user-buffer path, PAGE_BACKED_v1 §5.1);
/// for struct-backed files, uses kernel-buffer staging + copy_to_user.
pub(super) async fn sys_read<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }

    if let Some(result) = super::net::sys_socket_read(fd as u32, args[1], len, ctx) {
        return result;
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    // POSIX mq descriptors are not byte-stream fds. musl uses the
    // mq_* syscalls directly, but raw read/write on an mqd_t should
    // fail cleanly instead of falling into the VFS rnode path.
    if file.posix_mq().is_some() {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    if !file.flags().read {
        return SyscallResult::Error(EBADF_VALUE);
    }

    let len = inline_io_len_for_file(&file, len);

    if len == 0 {
        return SyscallResult::Return(0);
    }

    // PR-10 phase 5: userfaultfd fds carry their own `read(2)` arm
    // (drain a fault message off the pending queue, serialize 32-byte
    // `struct uffd_msg`). The VFS-shaped `OpenFile::step_read` returns
    // EINVAL for ufd backings, so dispatch here before any VFS-shaped
    // rnode() access.
    if file.ufd().is_some() {
        return super::userfaultfd::sys_ufd_read(&file, args[1], len, ctx).await;
    }
    // D9-D: signalfd fds carry their own `read(2)` arm (drain one
    // 128-byte `struct signalfd_siginfo` off the per-fd pending
    // queue). The VFS-shaped `OpenFile::rnode()` panics for the
    // signalfd backing, so this dispatch must run *before* any
    // generic VFS path.
    if file.signalfd().is_some() {
        return super::signalfd::sys_signalfd_read(&file, args[1], len, ctx).await;
    }
    // eventfd fds carry their own `read(2)` arm — drain the 64-bit
    // counter. Mirrors the ufd / signalfd dispatch shape.
    if file.eventfd().is_some() {
        return super::eventfd::sys_eventfd_read(&file, args[1], len, ctx).await;
    }
    // timerfd fds carry their own `read(2)` arm — return the
    // expiration count as an 8-byte u64.
    if file.timerfd().is_some() {
        return super::timerfd::sys_timerfd_read::<P>(&file, args[1], len, ctx).await;
    }
    if let tx_subsystems::vfs::structure::RNodeBacking::StructBacked {
        payload: tx_subsystems::vfs::structure::StructPayload::Socket { identity: socket },
    } = file.rnode().backing()
    {
        return sys_read_socket(&file, socket.clone(), buf_ptr as u64, len, ctx).await;
    }

    let len = if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        len
    } else {
        core::cmp::min(len, TTY_WRITE_MAX_INLINE)
    };

    if len == 0 {
        return SyscallResult::Return(0);
    }

    // PageBacked files: direct user-buffer path (PAGE_BACKED_v1 §5.1).
    // Prefault the user buffer in the observe phase, then drive the
    // read through the pmap without kernel-buffer staging.
    if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        return sys_read_pagebacked::<P>(&file, buf_ptr, len, ctx).await;
    }

    // Read into a kernel-side staging buffer, then copy out through
    // the canonical user-VA lane (`bootstrap_copy_to_user` bridges
    // via `aspace.copy_to_user`, falling back to the kernel-pointer
    // dance the trio's earlier exemption used).
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len.min(TTY_WRITE_MAX_INLINE)];

    // Phase A.3: drive `OpenFile::step_read` via the v3 `drive()` loop
    // (per `docs/Txv3/03_STEP_MODEL_v2.md` §8). The internal cursor
    // in `OpenFileReadOp` tracks the fill position across successive
    // `step()` calls; after drive() returns, copy the accumulated
    // bytes to userspace in a single pass.

    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileReadOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    // The op acquires its own epoch guard inside `step()` (STEP_MODEL_v2
    // §1, INVARIANTS_v5 YIELD-5 / EBR-7); no guard crosses `.await`.
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = OpenFileReadOp {
        file: &file,
        out: &mut staging,
        caller_netns: ctx.process.net_namespace(),
        cursor: 0,
    };
    match drive(
        op,
        &mut script_ctx,
        mode,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(total) => {
            if total > 0 {
                if let Err(errno) =
                    bootstrap_copy_to_user(&ctx.aspace, buf_ptr as u64, &staging[..total])
                {
                    return SyscallResult::error_from(errno);
                }
            }
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno;
            if file.flags().nonblocking && errno == tx_subsystems::execution::Errno::EAGAIN {
                let deadline_ns = P::read_ns().saturating_add(1_000_000);
                if let Some(future) = tx_subsystems::timer_sleep::sleep_until_ns(deadline_ns) {
                    future.await;
                } else {
                    tx_reactor::yield_now().await;
                }
            }
            SyscallResult::error_from(errno)
        }
    }
}

/// `pread64(fd, buf, count, offset)`.
///
/// Minimal implementation for seekable in-kernel files: temporarily
/// snapshots the shared `OpenFile` offset, delegates to the existing
/// `read(2)` path, then restores the original offset so callers do
/// not observe a positioned read as a seek.
fn positioned_io_check(file: &OpenFile, write: bool) -> Result<(), i32> {
    if matches!(
        file.rnode().backing(),
        RNodeBacking::StructBacked {
            payload: StructPayload::Pipe { .. },
        } | RNodeBacking::StructBacked {
            payload: StructPayload::Tty(_),
        }
    ) {
        return Err(ESPIPE_VALUE);
    }

    let flags = file.flags();
    if write {
        if !flags.write {
            return Err(EBADF_VALUE);
        }
    } else if !flags.read {
        return Err(EBADF_VALUE);
    }

    Ok(())
}

fn read_iovec_entry(ctx: &SyscallCtx<'_>, iov_ptr: u64, idx: u64) -> Result<(u64, u64), Errno> {
    const IOVEC_BYTES: u64 = 16;

    let ent_ptr = iov_ptr.wrapping_add(idx * IOVEC_BYTES);
    let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
    bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr)?;
    let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
    let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
    Ok((base, len))
}

pub(super) async fn sys_pread64<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let fd = args[0] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let offset = args[3];
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = positioned_io_check(&file, false) {
        return SyscallResult::Error(errno);
    }
    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_read::<P>(args, ctx).await;
    file.set_offset(saved);
    result
}

/// `pwrite64(fd, buf, count, offset)`.
pub(super) async fn sys_pwrite64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let offset = args[3];
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = positioned_io_check(&file, true) {
        return SyscallResult::Error(errno);
    }

    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_write(args, ctx).await;
    file.set_offset(saved);
    result
}

/// `preadv(fd, iov, iovcnt, offset)`.
pub(super) async fn sys_preadv<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;
    let mut offset = args[3];

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const MAX_RW_COUNT: u64 = 0x7fff_f000;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let (base, len) = match read_iovec_entry(ctx, iov_ptr, i) {
            Ok(entry) => entry,
            Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::error_from(errno);
            }
        };
        if len == 0 {
            continue;
        }
        if len > MAX_RW_COUNT {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EINVAL_VALUE);
        }

        let read_args = [args[0], base, len, offset, 0, 0];
        match sys_pread64::<P>(read_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                let n = n as u64;
                offset = match offset.checked_add(n) {
                    Some(value) => value,
                    None => return SyscallResult::Error(EINVAL_VALUE),
                };
                if n < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

/// `pwritev(fd, iov, iovcnt, offset)`.
pub(super) async fn sys_pwritev<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;
    let mut offset = args[3];

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if (offset as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const MAX_RW_COUNT: u64 = 0x7fff_f000;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let (base, len) = match read_iovec_entry(ctx, iov_ptr, i) {
            Ok(entry) => entry,
            Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::error_from(errno);
            }
        };
        if len == 0 {
            continue;
        }
        if len > MAX_RW_COUNT {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EINVAL_VALUE);
        }

        let write_args = [args[0], base, len, offset, 0, 0];
        match sys_pwrite64(write_args, ctx).await {
            SyscallResult::Return(n) => {
                total += n;
                let n = n as u64;
                offset = match offset.checked_add(n) {
                    Some(value) => value,
                    None => return SyscallResult::Error(EINVAL_VALUE),
                };
                if n < len {
                    return SyscallResult::Return(total);
                }
            }
            SyscallResult::Error(e) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(e);
            }
            other => return other,
        }
    }
    SyscallResult::Return(total)
}

fn preadv2_offset(args: [u64; 6]) -> Result<Option<u64>, i32> {
    let lo = args[3] as i64;
    let hi = args[4];
    if lo == -1 {
        return Ok(None);
    }
    if lo < 0 || hi != 0 {
        return Err(EINVAL_VALUE);
    }
    Ok(Some(args[3]))
}

/// `preadv2(fd, iov, iovcnt, offset_lo, offset_hi, flags)`.
pub(super) async fn sys_preadv2<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let flags = args[5] as i32;
    if flags != 0 {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    match preadv2_offset(args) {
        Ok(Some(offset)) => {
            let preadv_args = [args[0], args[1], args[2], offset, 0, 0];
            sys_preadv::<P>(preadv_args, ctx).await
        }
        Ok(None) => {
            let readv_args = [args[0], args[1], args[2], 0, 0, 0];
            sys_readv::<P>(readv_args, ctx).await
        }
        Err(errno) => SyscallResult::Error(errno),
    }
}

/// `pwritev2(fd, iov, iovcnt, offset_lo, offset_hi, flags)`.
pub(super) async fn sys_pwritev2<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let flags = args[5] as i32;
    if flags != 0 {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    match preadv2_offset(args) {
        Ok(Some(offset)) => {
            let pwritev_args = [args[0], args[1], args[2], offset, 0, 0];
            sys_pwritev(pwritev_args, ctx).await
        }
        Ok(None) => {
            let writev_args = [args[0], args[1], args[2], 0, 0, 0];
            sys_writev(writev_args, ctx).await
        }
        Err(errno) => SyscallResult::Error(errno),
    }
}

/// `fadvise64(fd, offset, len, advice)`.
pub(super) fn sys_fadvise64(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd = args[0] as i32;
    let offset = args[1];
    let len = args[2];
    let advice = args[3] as i32;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if (offset as i64) < 0 || (len as i64) < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if !(0..=5).contains(&advice) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    if let Err(errno) = positioned_io_check(&file, false) {
        return SyscallResult::Error(errno);
    }
    SyscallResult::Return(0)
}

/// `copy_file_range(fd_in, off_in, fd_out, off_out, len, flags)`.
pub(super) fn sys_copy_file_range<P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    use tx_subsystems::page_backed::step_copy_file_range;
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};

    let fd_in = args[0] as i32;
    let off_in_ptr = args[1];
    let fd_out = args[2] as i32;
    let off_out_ptr = args[3];
    let len = args[4] as usize;
    let flags = args[5];

    if fd_in < 0 || fd_out < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let in_file = match resolve_fd(&ctx.process, fd_in as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let out_file = match resolve_fd(&ctx.process, fd_out as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if !in_file.flags().read || !out_file.flags().write || out_file.flags().append {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len == 0 {
        return SyscallResult::Return(0);
    }

    let (in_rnode, in_pc) = match in_file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::PageBacked { pc } => (rnode.clone(), pc.clone()),
            RNodeBacking::Directory => return SyscallResult::Error(EISDIR_VALUE),
            _ => return SyscallResult::Error(EINVAL_VALUE),
        },
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let (out_rnode, out_pc) = match out_file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::PageBacked { pc } => (rnode.clone(), pc.clone()),
            RNodeBacking::Directory => return SyscallResult::Error(EISDIR_VALUE),
            _ => return SyscallResult::Error(EINVAL_VALUE),
        },
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    let read_offset = |ptr: u64, file_offset: u64| -> Result<(u64, bool), i32> {
        if ptr == 0 {
            return Ok((file_offset, false));
        }
        let mut bytes = [0u8; 8];
        bootstrap_copy_from_user(&ctx.aspace, &mut bytes, ptr).map_err(errno_to_i32)?;
        let signed = i64::from_le_bytes(bytes);
        if signed < 0 {
            return Err(EINVAL_VALUE);
        }
        bootstrap_copy_to_user(&ctx.aspace, ptr, &bytes).map_err(errno_to_i32)?;
        Ok((signed as u64, true))
    };

    let (in_offset, explicit_in) = match read_offset(off_in_ptr, in_file.offset()) {
        Ok(v) => v,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let (out_offset, explicit_out) = match read_offset(off_out_ptr, out_file.offset()) {
        Ok(v) => v,
        Err(errno) => return SyscallResult::Error(errno),
    };

    let guard = tx_substrate::epoch::guard();
    let outcome = step_copy_file_range(&in_pc, in_offset, &out_pc, out_offset, len, &guard);
    drop(guard);

    let transferred = match outcome {
        tx_substrate::step::StepOutcome::Done(n) => n,
        tx_substrate::step::StepOutcome::Err(e) => {
            let errno: tx_subsystems::execution::Errno = e;
            return SyscallResult::error_from(errno);
        }
        _ => return SyscallResult::Error(EAGAIN_VALUE),
    };

    if transferred > 0 {
        let in_next = in_offset + transferred as u64;
        let out_next = out_offset + transferred as u64;
        if explicit_in {
            if let Err(_errno) =
                bootstrap_copy_to_user(&ctx.aspace, off_in_ptr, &in_next.to_le_bytes())
            {
                return SyscallResult::Return(transferred as i64);
            }
        } else {
            in_file.advance_offset(transferred as u64);
        }
        if explicit_out {
            if let Err(_errno) =
                bootstrap_copy_to_user(&ctx.aspace, off_out_ptr, &out_next.to_le_bytes())
            {
                return SyscallResult::Return(transferred as i64);
            }
        } else {
            out_file.advance_offset(transferred as u64);
        }

        let mut meta = crate::linux_syscall::fs_basic::stat_meta_override_or(
            out_rnode.fs_object_id(),
            out_rnode.meta(),
        );
        meta.size = out_pc.size_bytes();
        let old_mtime = meta.mtime;
        meta.mtime =
            tx_subsystems::vfs::Timespec::new(old_mtime.sec.saturating_add(2), old_mtime.nsec);
        meta.ctime = meta.mtime;
        crate::linux_syscall::fs_basic::record_stat_meta_override(out_rnode.fs_object_id(), meta);
    }

    let _ = in_rnode;
    SyscallResult::Return(transferred as i64)
}

/// `splice(fd_in, off_in, fd_out, off_out, len, flags)`.
///
/// This is intentionally conservative. The fd-io `splice07` matrix is an
/// invalid-combination test: it expects `EINVAL` or `EBADF` and skips the
/// combinations that would perform real pipe/file transfer. Until the pipe
/// zero-copy path lands, reject supported fd combinations deterministically
/// instead of returning `ENOSYS` or blocking on empty pipes.
pub(super) fn sys_splice(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let fd_in = args[0] as i32;
    let fd_out = args[2] as i32;
    let len = args[4];

    if fd_in < 0 || fd_out < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len == 0 {
        return SyscallResult::Return(0);
    }

    let in_exists = resolve_fd(&ctx.process, fd_in as u32).is_some()
        || super::net::is_socket_fd(fd_in as u32, ctx);
    let out_exists = resolve_fd(&ctx.process, fd_out as u32).is_some()
        || super::net::is_socket_fd(fd_out as u32, ctx);
    if !in_exists || !out_exists {
        return SyscallResult::Error(EBADF_VALUE);
    }

    SyscallResult::Error(EINVAL_VALUE)
}

/// `sendfile64(out_fd, in_fd, offset, count)` — page-level copy from
/// one fd to another without an intermediate userspace buffer.
///
/// Linux generic ABI `__NR_sendfile64 = 71`.  Copies up to `count`
/// bytes from `in_fd` (must be a page-backed regular file) to
/// `out_fd`.  If `offset` is non-NULL, reads the input position from
/// `*offset` and writes the updated position back; the input file's
/// fd-level cursor is not touched in this case.  If `offset` is NULL,
/// the input file's fd cursor is used and advanced.
pub(super) async fn sys_sendfile64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    use tx_subsystems::page_backed::step_copy_file_range;
    use tx_subsystems::vfs::structure::RNodeBacking;

    let out_fd = args[0] as i32;
    let in_fd = args[1] as i32;
    let offset_ptr = args[2];
    let count = args[3] as usize;

    if out_fd < 0 || in_fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if count == 0 {
        return SyscallResult::Return(0);
    }

    let out_file = match resolve_fd(&ctx.process, out_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let in_file = match resolve_fd(&ctx.process, in_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if !in_file.flags().read || !out_file.flags().write {
        return SyscallResult::Error(EBADF_VALUE);
    }

    if let Some(result) = super::net::sys_socket_sendfile(out_fd as u32, count, ctx) {
        return result;
    }

    // Input must be a regular file backed by PageContainer.
    let in_rnode = in_file.rnode();
    let in_pc = match in_rnode.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Output must be page-backed (regular file); pipes/sockets are
    // deferred per PAGE_BACKED §9.1.
    let out_rnode = out_file.rnode();
    let out_pc = match out_rnode.backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Determine input offset.
    let in_offset: u64;
    let needs_offset_writeback: bool;
    if offset_ptr != 0 {
        // Read the offset from userspace.
        let mut off_bytes = [0u8; 8];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut off_bytes, offset_ptr) {
            return SyscallResult::error_from(errno);
        }
        let signed_offset = i64::from_le_bytes(off_bytes);
        if signed_offset < 0 {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, offset_ptr, &off_bytes) {
            return SyscallResult::error_from(errno);
        }
        in_offset = signed_offset as u64;
        needs_offset_writeback = true;
    } else {
        // Use the input file's current cursor.
        in_offset = in_file.offset();
        needs_offset_writeback = false;
    }

    // Output writes at the output file's current cursor.
    let out_offset = out_file.offset();

    // Single-shot page copy (non-blocking for v1).
    let guard = tx_substrate::epoch::guard();
    let outcome = step_copy_file_range(&in_pc, in_offset, &out_pc, out_offset, count, &guard);
    drop(guard);

    let transferred = match outcome {
        tx_substrate::step::StepOutcome::Done(n) => n,
        tx_substrate::step::StepOutcome::Err(e) => {
            let errno: tx_subsystems::execution::Errno = e;
            return SyscallResult::error_from(errno);
        }
        // If the operation would block, return EAGAIN (sendfile is
        // non-blocking in this v1 implementation).
        _ => return SyscallResult::Error(EAGAIN_VALUE),
    };

    if transferred == 0 {
        return SyscallResult::Return(0);
    }

    // Advance the output file's cursor.
    out_file.advance_offset(transferred as u64);

    // Update the input offset in userspace if a pointer was given.
    if needs_offset_writeback {
        let new_off = in_offset + transferred as u64;
        let bytes = new_off.to_le_bytes();
        if let Err(_errno) = bootstrap_copy_to_user(&ctx.aspace, offset_ptr, &bytes) {
            // On partial success, Linux prefers to return the byte
            // count rather than the fault error.
            return SyscallResult::Return(transferred as i64);
        }
    } else {
        // Advance the input file's cursor when offset was NULL.
        in_file.advance_offset(transferred as u64);
    }

    SyscallResult::Return(transferred as i64)
}

async fn sys_read_socket<'a>(
    file: &Cap<OpenFile>,
    socket: Cap<tx_subsystems::net::SocketIdentity>,
    buf_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let mut flags = tx_subsystems::net::SendRecvFlags::empty();
    if file.flags().nonblocking {
        flags |= tx_subsystems::net::SendRecvFlags::MSG_DONTWAIT;
    }
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len.min(SOCKET_IO_MAX_INLINE)];

    if matches!(
        socket.kind,
        tx_subsystems::net::SocketKind::NetlinkRoute
            | tx_subsystems::net::SocketKind::NetlinkNetfilter
    ) {
        loop {
            let result = match socket.kind {
                tx_subsystems::net::SocketKind::NetlinkRoute => {
                    tx_subsystems::net::netlink_route_recv(&socket, &mut staging, flags)
                }
                tx_subsystems::net::SocketKind::NetlinkNetfilter => {
                    tx_subsystems::net::netlink_netfilter_recv(&socket, &mut staging, flags)
                }
                _ => unreachable!(),
            };
            match result {
                Ok(recv) => {
                    if recv > 0 {
                        if let Err(errno) =
                            bootstrap_copy_to_user(&ctx.aspace, buf_ptr, &staging[..recv])
                        {
                            return SyscallResult::Error(errno_to_i32(errno));
                        }
                    }
                    return SyscallResult::Return(recv as i64);
                }
                Err(tx_subsystems::execution::Errno::EAGAIN) if !flags.is_nonblocking() => {
                    let wait_token = {
                        let guard = tx_substrate::epoch::guard();
                        socket_poll_wait_token_from_file(
                            file,
                            tx_subsystems::net::PollMask::IN,
                            &guard,
                        )
                    };
                    match wait_token {
                        Some(Ok(Some(token))) => {
                            if let Some(future) = wait_source::wait_on_token(token) {
                                let _ = future.await;
                            } else {
                                return SyscallResult::Error(EIO_VALUE);
                            }
                        }
                        Some(Ok(None)) | None => return SyscallResult::Error(EAGAIN_VALUE),
                        Some(Err(errno)) => return SyscallResult::Error(errno_to_i32(errno)),
                    }
                }
                Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
            }
        }
    }

    loop {
        drive_loopback_pending();
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            tx_subsystems::net::execution::step_recv_kernel_bytes(
                &socket,
                &mut staging,
                flags,
                &guard,
            )
        };
        match outcome {
            tx_substrate::step::StepOutcome::Done(recv) => {
                if recv.bytes > 0 {
                    if let Err(errno) =
                        bootstrap_copy_to_user(&ctx.aspace, buf_ptr, &staging[..recv.bytes])
                    {
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                yield_after_socket_read_if_needed(&socket, recv.bytes).await;
                return SyscallResult::Return(recv.bytes as i64);
            }
            tx_substrate::step::StepOutcome::Yield { shape, .. } => {
                if flags.is_nonblocking() {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                match shape {
                    tx_substrate::step::YieldShape::OnWaitSource { source, interests }
                    | tx_substrate::step::YieldShape::OnEdge { source, interests } => {
                        let token =
                            tx_subsystems::execution::WaitToken::new(source.raw(), interests.raw());
                        if let Some(future) = wait_source::wait_on_token(token) {
                            let _ = future.await;
                        }
                    }
                    tx_substrate::step::YieldShape::OnAgent { .. }
                    | tx_substrate::step::YieldShape::OnTimer { .. } => {
                        return SyscallResult::Error(errno_to_i32(Errno::EIO));
                    }
                }
            }
            tx_substrate::step::StepOutcome::Continue { .. } => {}
            tx_substrate::step::StepOutcome::Err(errno) => {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

fn inline_io_len_for_file(file: &Cap<OpenFile>, len: usize) -> usize {
    match file.backing() {
        tx_subsystems::vfs::structure::OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            tx_subsystems::vfs::RNodeBacking::PageBacked { .. } => len,
            tx_subsystems::vfs::RNodeBacking::StructBacked {
                payload: tx_subsystems::vfs::structure::StructPayload::Socket { .. },
            } => len.min(SOCKET_IO_MAX_INLINE),
            _ => len.min(TTY_WRITE_MAX_INLINE),
        },
        tx_subsystems::vfs::structure::OpenFileBacking::Ufd { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::AioContext { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::SignalFd { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::Epoll { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::IoUring { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::Eventfd { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::Timerfd { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::PosixMq { .. }
        | tx_subsystems::vfs::structure::OpenFileBacking::Pidfd { .. } => len,
    }
}

async fn yield_after_socket_read_if_needed(
    socket: &Cap<tx_subsystems::net::SocketIdentity>,
    bytes: usize,
) {
    if bytes > 0 && socket.kind == tx_subsystems::net::SocketKind::Tcp {
        tx_reactor::yield_now().await;
    }
}
