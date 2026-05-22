//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::reactor_entry;
use crate::adapter::step_engine::Cap;
use alloc::vec::Vec;

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
    for raw in 1..=Signum::MAX {
        let Some(sig) = Signum::new(raw) else {
            continue;
        };
        if mask_bits & sig.bit() != 0 {
            payload.pending().clear(sig);
        }
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

pub(super) async fn sys_pselect6<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let nfds_signed = args[0] as i64;
    if nfds_signed < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let nfds = nfds_signed as u64;
    if nfds > FD_SETSIZE_MAX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let timeout_ns = match read_pselect_timeout_ns(&ctx.aspace, args[4]) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };

    let read_ptr = args[1];
    let write_ptr = args[2];
    let except_ptr = args[3];
    let mut readfds = match read_fdset(&ctx.aspace, read_ptr, nfds) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let mut writefds = match read_fdset(&ctx.aspace, write_ptr, nfds) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let mut exceptfds = match read_fdset(&ctx.aspace, except_ptr, nfds) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };

    let mut ready = 0i64;
    for fd in 0..nfds {
        let want_read = fdset_has(readfds.as_deref(), fd);
        let want_write = fdset_has(writefds.as_deref(), fd);
        let want_except = fdset_has(exceptfds.as_deref(), fd);
        if !want_read && !want_write && !want_except {
            continue;
        }
        let Some(file) = resolve_fd(&ctx.process, fd as u32) else {
            return SyscallResult::Error(EBADF_VALUE);
        };
        let (read_ready, write_ready, except_ready) =
            pselect_fd_ready(&file, want_read, want_write, want_except);
        if !read_ready {
            fdset_clear(readfds.as_deref_mut(), fd);
        } else {
            ready += 1;
        }
        if !write_ready {
            fdset_clear(writefds.as_deref_mut(), fd);
        } else {
            ready += 1;
        }
        if !except_ready {
            fdset_clear(exceptfds.as_deref_mut(), fd);
        } else {
            ready += 1;
        }
    }

    if ready == 0 {
        if let Some(ns) = timeout_ns {
            match sleep_timeout_ns::<P>(ns, ctx).await {
                SyscallResult::Return(_) => {}
                other => return other,
            }
        }
    }

    if let Err(errno) = write_fdset(&ctx.aspace, read_ptr, readfds.as_deref()) {
        return SyscallResult::Error(errno);
    }
    if let Err(errno) = write_fdset(&ctx.aspace, write_ptr, writefds.as_deref()) {
        return SyscallResult::Error(errno);
    }
    if let Err(errno) = write_fdset(&ctx.aspace, except_ptr, exceptfds.as_deref()) {
        return SyscallResult::Error(errno);
    }

    SyscallResult::Return(ready)
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
    let fd = args[0] as i32;
    let stdio_tty_fast_path = (fd == 1 || fd == 2)
        && resolve_fd(&ctx.process, fd as u32)
            .map(|file| {
                matches!(
                    file.rnode().backing(),
                    tx_subsystems::vfs::RNodeBacking::StructBacked {
                        payload: tx_subsystems::vfs::StructPayload::Tty(_)
                    }
                )
            })
            .unwrap_or(false);
    if stdio_tty_fast_path {
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

            let old_len = combined.len();
            combined.resize(old_len + len as usize, 0);
            if let Err(errno) =
                bootstrap_copy_from_user(&ctx.aspace, &mut combined[old_len..], base)
            {
                return SyscallResult::error_from(errno);
            }
        }

        let write_args = [
            args[0],
            combined.as_ptr() as u64,
            combined.len() as u64,
            0,
            0,
            0,
        ];
        return sys_write(write_args, ctx).await;
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

/// `ppoll(fds, nfds, tmo_p, sigmask)` — minimal v1 stub for
/// interactive `busybox sh` so its read loop doesn't trap with
/// `-ENOSYS`.
///
/// `struct pollfd { int fd; short events; short revents; }`
/// (8 bytes on RV64). For each entry with `fd >= 0`, set
/// `revents = events` (i.e., mark every requested event "ready").
/// Return `nfds`.
///
/// Why this is sufficient for busybox sh:
///
/// - Interactive `sh` calls `ppoll([{fd=0, events=POLLIN}], 1,
///   NULL, NULL)` and then `read(0, ...)`. Our stub says
///   "ready"; busybox calls `read`; our `sys_read` blocks on the
///   TTY wait source until UART RX delivers bytes. End-to-end
///   semantics match Linux.
///
/// - For polls with `nfds > 1`, every fd appears ready; busybox
///   then individually reads each and observes which are actually
///   ready (or blocks).
///
/// Limitations: timeout is currently ignored (we don't sleep to
/// the deadline; we return "ready" immediately). For shell
/// interactive use this is fine — the timeout is typically NULL
/// (block indefinitely, which is what `read` does anyway in the
/// follow-up). For non-blocking polls (timeout = 0) this would
/// busy-loop in userspace; address it if/when a real workload hits
/// it.
/// `ppoll(fds, nfds, timeout_ptr, sigmask_ptr)`.
///
/// Minimal implementation that supports the busybox interactive-shell
/// pattern (single fd, POLLIN, blocking wait). For each pollfd we
/// peek at the fd's TTY backing readability; if no fd is currently
/// ready and the timeout is non-zero, we park on the first TTY fd's
/// wait source and re-poll on wake. Returns the number of fds with
/// non-zero `revents`.
///
/// Behaviour gaps (called out so a future caller doesn't trip on
/// them):
/// - Only POLLIN is honoured; POLLOUT / POLLERR / etc. are reported
///   verbatim from `events` if any fd is found to be ready, otherwise
///   suppressed. POLLOUT-only polls on TTY backings still return
///   "ready" eagerly to match the legacy stub semantics.
/// - Multi-fd waits park on the FIRST TTY POLLIN fd only. If a
///   different fd becomes readable while we're parked on the first,
///   we wake on the next ingest event regardless (the wait_source
///   carrier fires from any TTY ingest path); the re-check loop then
///   notices the other fd. Cross-fd starvation is theoretically
///   possible but not observed in practice for the busybox flows.
/// - The `timeout_ptr` is read but a non-NULL timeout uses the
///   timeout-elapsed branch only as an upper bound; the actual
///   timer hookup ships with the OnTimer wave (deferred).
/// - The signal mask is ignored.
pub(super) async fn sys_ppoll<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_subsystems::{
        pipe::PipeSide,
        vfs::structure::{OpenFileBacking, RNodeBacking, StructPayload},
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
            None => SyscallResult::Return(0),
        };
        return restore_ppoll_sigmask(ctx, saved_mask, temporary_sigmask, result);
    }

    const POLLFD_BYTES: u64 = 8;
    const POLLIN: i16 = 0x0001;
    const POLLOUT: i16 = 0x0004;
    const POLLERR: i16 = 0x0008;
    const POLLHUP: i16 = 0x0010;
    const POLLNVAL: i16 = 0x0020;

    let wait_allowed = timeout_ns.is_none(); // NULL = infinite wait

    // Track the first TTY fd's WaitSourceId for parking.
    let mut park_source: Option<(u64, u64)> = None; // (source_id_raw, interests_raw)

    let ready = loop {
        let mut ready: i64 = 0;
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
                    let mut handled = false;
                    if let OpenFileBacking::Rnode { rnode } = file.backing() {
                        match rnode.backing() {
                            RNodeBacking::StructBacked {
                                payload: StructPayload::Tty(tty),
                            } => {
                                handled = true;
                                if events & POLLIN != 0 {
                                    if tty_readable_level(tty) {
                                        revents |= POLLIN;
                                    } else if park_source.is_none() {
                                        park_source = Some((tty.wait_source_id(), POLLIN as u64));
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
                                            } else if park_source.is_none() {
                                                park_source = Some((
                                                    payload.reader_source_id(),
                                                    POLLIN as u64,
                                                ));
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
                                            } else if park_source.is_none() {
                                                park_source = Some((
                                                    payload.writer_source_id(),
                                                    POLLOUT as u64,
                                                ));
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
                    }
                    if !handled {
                        if events & POLLIN != 0 {
                            revents |= POLLIN;
                        }
                        if events & POLLOUT != 0 {
                            revents |= POLLOUT;
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
        if let Some(ns) = timeout_ns {
            if ns != 0 {
                match sleep_timeout_ns::<P>(ns, ctx).await {
                    SyscallResult::Return(_) => {}
                    other => {
                        return restore_ppoll_sigmask(ctx, saved_mask, temporary_sigmask, other);
                    }
                }
            }
            break 0;
        }
        if !wait_allowed {
            break 0;
        }
        let Some((source_id, interests)) = park_source.take() else {
            break 0;
        };

        // drive-taskmb: park on the fd's WaitSource via drive() +
        // PpollOp. The driver registers the task mailbox with the
        // WaitSource, parks, and wakes when the fd fires.
        use crate::adapter::step_engine;
        use step_engine::{InterestMask, WaitSourceId};
        use tx_scripts::drive;
        use tx_substrate::step::DriveMode;

        let mut script_ctx = build_subject_script_ctx(ctx);
        let mailbox_arc = script_ctx.mailbox().cloned();
        let timer_wheel_arc = script_ctx.timer_wheel().cloned();
        let delegate_registry_arc = script_ctx.delegate_registry().cloned();
        // PpollOp does not need an epoch guard (`yield → park` only).
        let op = tx_subsystems::vfs::composite::PpollOp {
            wait_source_id: WaitSourceId::new(source_id),
            interests: InterestMask::new(interests),
            timeout_ms: None,
            started: false,
        };
        match drive(
            op,
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_wheel_arc.as_ref(),
        )
        .await
        {
            Ok(1) => {
                // Fd is ready; re-scan to update revents.
            }
            Ok(_) => break 0,
            Err(_e) => break 0,
        }
    };

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
                let errno: tx_subsystems::execution::Errno = e.into();
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
            let errno: tx_subsystems::execution::Errno = v3errno.into();
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

    // eventfd fds carry their own `write(2)` arm — add a 64-bit
    // value to the counter. Dispatch before the generic VFS path.
    if file.eventfd().is_some() {
        return super::eventfd::sys_eventfd_write(&file, args[1], len, ctx).await;
    }

    let len = if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        len
    } else {
        core::cmp::min(len, TTY_WRITE_MAX_INLINE)
    };

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
            let errno: tx_subsystems::execution::Errno = v3errno.into();
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
                let errno: tx_subsystems::execution::Errno = e.into();
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
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            SyscallResult::error_from(errno)
        }
    }
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

    if len == 0 {
        return SyscallResult::Return(0);
    }

    // PR-10 phase 5: userfaultfd fds carry their own `read(2)` arm
    // (drain a fault message off the pending queue, serialize 32-byte
    // `struct uffd_msg`). The VFS-shaped `OpenFile::step_read` returns
    // EINVAL for ufd backings, so dispatch here before the generic
    // path.
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

    let len = if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        len
    } else {
        core::cmp::min(len, TTY_WRITE_MAX_INLINE)
    };

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
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];

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
            let errno: tx_subsystems::execution::Errno = v3errno.into();
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
    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_read::<P>(args, ctx).await;
    file.set_offset(saved);
    result
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
        in_offset = u64::from_le_bytes(off_bytes);
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
            let errno: tx_subsystems::execution::Errno = e.into();
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
