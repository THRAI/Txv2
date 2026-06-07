//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::reactor_entry;
use crate::adapter::step_engine::Cap;

fn tty_readable_level(tty: &Cap<tx_subsystems::tty::structure::TtyIdentity>) -> bool {
    use tx_subsystems::tty::execution::TTY_READABLE;
    tty.input_readable.peek() & TTY_READABLE != 0
}

#[derive(Clone, Copy)]
enum SelectDir {
    Read,
    Write,
}

/// Where a not-yet-ready fd parks. Pipe/TTY/socketpair carriers live in the
/// substrate `reactor_entry` wait-source registry (parked via
/// `await_wait_source`); real network sockets live in the subsystems
/// `wait_source` registry (parked via `wait_on_token`). Routing a socket token
/// through `await_wait_source` silently no-ops (different registry) and
/// busy-loops, so the two must be kept distinct.
enum SelectPark {
    Reactor(
        crate::adapter::step_engine::WaitSourceId,
        crate::adapter::step_engine::InterestMask,
    ),
    Socket(tx_subsystems::execution::WaitToken),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaticCharDevice {
    Null,
    Zero,
}

static STATIC_ZERO_READ_BUF: [u8; TTY_WRITE_MAX_INLINE] = [0u8; TTY_WRITE_MAX_INLINE];

fn static_char_device(file: &Cap<OpenFile>) -> Option<StaticCharDevice> {
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking, StructPayload};

    let OpenFileBacking::Rnode { rnode } = file.backing() else {
        return None;
    };
    let RNodeBacking::StructBacked {
        payload: StructPayload::CharDevice(binding),
    } = rnode.backing()
    else {
        return None;
    };

    match (binding.name, binding.devt.major(), binding.devt.minor()) {
        ("null", 1, 3) => Some(StaticCharDevice::Null),
        ("zero", 1, 5) => Some(StaticCharDevice::Zero),
        _ => None,
    }
}

pub(super) fn dispatch_static_chardev_immediate(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    match req.nr {
        NR_READ => read_static_chardev_immediate(req.args, process, aspace),
        NR_WRITE => write_static_chardev_immediate(req.args, process),
        _ => None,
    }
}

pub(super) fn dispatch_static_chardev_cap_immediate(
    req: &SyscallRequest,
    process: &Cap<ProcessIdentity>,
) -> Option<SyscallResult> {
    match req.nr {
        NR_WRITE => write_static_chardev_immediate(req.args, process),
        _ => None,
    }
}

fn write_static_chardev_immediate(
    args: [u64; 6],
    process: &Cap<ProcessIdentity>,
) -> Option<SyscallResult> {
    let fd = args[0] as i32;
    let len = args[2] as usize;
    if fd < 0 {
        return None;
    }

    let file = match resolve_fd(process, fd as u32) {
        Some(file) => file,
        None => return None,
    };
    if static_char_device(&file) != Some(StaticCharDevice::Null) {
        return None;
    }
    if !file.flags().write {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }

    let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
    Some(SyscallResult::Return(len as i64))
}

fn read_static_chardev_immediate(
    args: [u64; 6],
    process: &Cap<ProcessIdentity>,
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;
    if fd < 0 {
        return None;
    }

    let file = match resolve_fd(process, fd as u32) {
        Some(file) => file,
        None => return None,
    };
    if static_char_device(&file) != Some(StaticCharDevice::Zero) {
        return None;
    }
    if !file.flags().read {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }

    let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
    if len == 0 {
        return Some(SyscallResult::Return(0));
    }
    if let Err(errno) = bootstrap_copy_to_user(aspace, buf_ptr as u64, &STATIC_ZERO_READ_BUF[..len])
    {
        return Some(SyscallResult::error_from(errno));
    }
    Some(SyscallResult::Return(len as i64))
}

fn fdset_bytes(nfds: u64) -> Result<usize, SyscallResult> {
    if nfds > 1024 {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    Ok(nfds.div_ceil(64).saturating_mul(8) as usize)
}

fn fdset_get(bits: &[u8], fd: u64) -> bool {
    let byte = (fd / 8) as usize;
    let bit = (fd % 8) as u8;
    bits.get(byte)
        .map(|b| (b & (1u8 << bit)) != 0)
        .unwrap_or(false)
}

fn fdset_clear(bits: &mut [u8], fd: u64) {
    let byte = (fd / 8) as usize;
    let bit = (fd % 8) as u8;
    if let Some(b) = bits.get_mut(byte) {
        *b &= !(1u8 << bit);
    }
}

fn select_fd_ready(file: &Cap<OpenFile>, dir: SelectDir) -> (bool, Option<SelectPark>) {
    use crate::adapter::step_engine::{InterestMask, WaitSourceId};
    use tx_subsystems::pipe::PipeSide;
    use tx_subsystems::vfs::structure::{RNodeBacking, StructPayload};

    if let Some((pipe, side)) = file.pipe_endpoint() {
        return match (dir, side) {
            (SelectDir::Read, PipeSide::Reader) => (
                pipe.readable_level(),
                Some(SelectPark::Reactor(
                    WaitSourceId::new(pipe.reader_source_id()),
                    InterestMask::new(0x1),
                )),
            ),
            (SelectDir::Write, PipeSide::Writer) => (
                pipe.writable_level(),
                Some(SelectPark::Reactor(
                    WaitSourceId::new(pipe.writer_source_id()),
                    InterestMask::new(0x2),
                )),
            ),
            // Linux reports the wrong-direction end as not ready for
            // the requested operation; there is no useful wait source.
            _ => (false, None),
        };
    }
    if let Some((rx, tx)) = file.socketpair_endpoint() {
        return match dir {
            SelectDir::Read => (
                rx.readable_level(),
                Some(SelectPark::Reactor(
                    WaitSourceId::new(rx.reader_source_id()),
                    InterestMask::new(0x1),
                )),
            ),
            SelectDir::Write => (
                tx.writable_level(),
                Some(SelectPark::Reactor(
                    WaitSourceId::new(tx.writer_source_id()),
                    InterestMask::new(0x2),
                )),
            ),
        };
    }

    // Real network sockets are not pipe/socketpair/TTY backed; route their
    // readiness through the socket poll seam. Without this, sockets fall into
    // the catch-all `_ => (true, None)` arm below and select/poll reports every
    // socket as permanently ready with no wait source — so a blocking
    // `select`/`poll` on a not-yet-readable socket returns immediately. Socket
    // carriers live in the subsystems `wait_source` registry, so they must park
    // via `wait_on_token` (SelectPark::Socket), not the substrate
    // `await_wait_source` path. The re-home dropped this case entirely.
    {
        use tx_subsystems::net::PollMask;
        let guard = tx_substrate::epoch::guard();
        if let Some(result) = super::socket::socket_poll_mask_from_file(file, &guard) {
            let mask = match result {
                Ok(mask) => mask,
                // Report ready with no wait source: select/poll returns and the
                // subsequent read/write op surfaces the real errno.
                Err(_) => return (true, None),
            };
            let (ready, want) = match dir {
                SelectDir::Read => (
                    mask.intersects(
                        PollMask::IN | PollMask::ERR | PollMask::HUP | PollMask::RDHUP,
                    ),
                    PollMask::IN,
                ),
                SelectDir::Write => {
                    (mask.intersects(PollMask::OUT | PollMask::ERR), PollMask::OUT)
                }
            };
            if ready {
                return (true, None);
            }
            let source = match super::socket::socket_poll_wait_token_from_file(file, want, &guard) {
                Some(Ok(Some(token))) => Some(SelectPark::Socket(token)),
                _ => None,
            };
            return (false, source);
        }
    }

    match file.rnode().backing() {
        RNodeBacking::StructBacked {
            payload: StructPayload::Tty(tty),
        } => match dir {
            SelectDir::Read => (
                tty_readable_level(tty),
                Some(SelectPark::Reactor(
                    WaitSourceId::new(tty.wait_source_id()),
                    InterestMask::new(0x1),
                )),
            ),
            SelectDir::Write => (true, None),
        },
        _ => (true, None),
    }
}

/// Park the current task on the wait source chosen by `select_fd_ready`,
/// routing to the correct registry. Returns when the source fires (or
/// immediately, for an unresolvable socket token, after a yield to avoid a
/// busy-loop). The caller re-polls all fds after this returns.
async fn await_select_park(ctx: &SyscallCtx<'_>, park: SelectPark) {
    match park {
        SelectPark::Reactor(source, interests) => {
            super::await_wait_source(ctx, source, interests).await;
        }
        SelectPark::Socket(token) => {
            match tx_subsystems::wait_source::wait_on_token(token) {
                Some(future) => {
                    let _ = future.await;
                }
                // Token not registered (should not happen for a live socket):
                // yield rather than spin so other tasks make progress.
                None => tx_reactor::yield_now().await,
            }
        }
    }
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
    emit_debug_counter(b"debug.writev.enter", args[0] as i64);
    emit_debug_counter(b"debug.writev.iovcnt", iovcnt as i64);

    if !(0..=1024).contains(&iovcnt) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return SyscallResult::Return(0);
    }

    const IOVEC_BYTES: u64 = 16;
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

            let old_len = combined.len();
            combined.resize(old_len + len as usize, 0);
            if let Err(errno) =
                bootstrap_copy_from_user(&ctx.aspace, &mut combined[old_len..], base)
            {
                return SyscallResult::error_from(errno);
            }
        }

        return sys_write_buffered(&file, &combined, ctx).await;
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

/// PageBacked-only synchronous `writev(2)` lane.
///
/// This is intentionally narrower than [`sys_writev`]: it claims only regular
/// PageBacked fds and only when each PageBacked write completes without a
/// `Yield`. Other fd kinds, and future PageBacked backends that need to park,
/// return `None` so the async `sys_writev` path remains the semantic fallback.
pub(super) fn sys_writev_pagebacked_oneshot<'a>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> Option<SyscallResult> {
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};

    let fd = args[0] as i32;
    let iov_ptr = args[1];
    let iovcnt = args[2] as i32;
    emit_debug_counter(b"debug.writev.pagebacked_oneshot.enter", fd as i64);
    emit_debug_counter(b"debug.writev.pagebacked_oneshot.iovcnt", iovcnt as i64);

    if !(0..=1024).contains(&iovcnt) {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    if iovcnt == 0 {
        return Some(SyscallResult::Return(0));
    }
    if fd < 0 {
        return Some(SyscallResult::Error(EBADF_VALUE));
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return Some(SyscallResult::Error(EBADF_VALUE)),
    };

    if file.posix_mq().is_some() || file.eventfd().is_some() {
        return None;
    }

    let pc = match file.backing() {
        OpenFileBacking::Rnode { rnode } => match rnode.backing() {
            RNodeBacking::PageBacked { pc } => pc.clone(),
            _ => return None,
        },
        _ => return None,
    };
    if !file.flags().write {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }

    const IOVEC_BYTES: u64 = 16;
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return Some(SyscallResult::Return(total));
            }
            return Some(SyscallResult::error_from(errno));
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap()) as usize;
        if len == 0 {
            continue;
        }

        emit_debug_counter(b"debug.write.enter", fd as i64);
        emit_debug_counter(b"debug.write.len", len as i64);
        emit_debug_counter(b"debug.write.pagebacked.len", len as i64);

        if let Some(range) = super::user_copy::covering_user_range(base, len) {
            use crate::adapter::step_engine::StepOutcome as V3;
            use tx_subsystems::vm::UserAccessKind;
            match ctx
                .aspace
                .reserve_user_range_for_access(range, UserAccessKind::Read)
            {
                V3::Done(()) => {}
                V3::Err(e) => {
                    if total > 0 {
                        return Some(SyscallResult::Return(total));
                    }
                    let errno: tx_subsystems::execution::Errno = e.into();
                    return Some(SyscallResult::error_from(errno));
                }
                V3::Yield { .. } | V3::Continue { .. } => {
                    if total > 0 {
                        return Some(SyscallResult::Return(total));
                    }
                    return None;
                }
            }
        }

        let guard = crate::adapter::step_engine::guard();
        match tx_subsystems::page_backed::step_write_from_user(
            &pc,
            &file,
            &ctx.aspace,
            tx_hal::UserPtr::<u8>::new(base as usize),
            len,
            &guard,
        ) {
            tx_substrate::step::StepOutcome::Done(n) => {
                total += n as i64;
                if n < len {
                    return Some(SyscallResult::Return(total));
                }
            }
            tx_substrate::step::StepOutcome::Continue { progress } => {
                total += progress.bytes() as i64;
                return Some(SyscallResult::Return(total));
            }
            tx_substrate::step::StepOutcome::Err(e) => {
                if total > 0 {
                    return Some(SyscallResult::Return(total));
                }
                let errno: tx_subsystems::execution::Errno = e.into();
                if errno == tx_subsystems::execution::Errno::EPIPE {
                    let _ = tx_subsystems::signal::step_kill_process(
                        &ctx.process,
                        tx_subsystems::signal::Signum::SIGPIPE,
                        None,
                    );
                }
                return Some(SyscallResult::error_from(errno));
            }
            tx_substrate::step::StepOutcome::Yield { .. } => {
                if total > 0 {
                    return Some(SyscallResult::Return(total));
                }
                return None;
            }
        }
        drop(guard);
        drop(step_engine::guard());
    }

    Some(SyscallResult::Return(total))
}

pub(super) fn sys_writev_pagebacked_candidate<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> bool {
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};

    let fd = args[0] as i32;
    if fd < 0 {
        return false;
    }
    let Some(file) = resolve_fd(&ctx.process, fd as u32) else {
        return false;
    };
    if file.posix_mq().is_some() || file.eventfd().is_some() {
        return false;
    }
    match file.backing() {
        OpenFileBacking::Rnode { rnode } => {
            matches!(rnode.backing(), RNodeBacking::PageBacked { .. })
        }
        _ => false,
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
/// pattern plus anonymous pipe/socketpair readiness. For each pollfd
/// we route through the same fd readiness helper used by `pselect6`;
/// if no fd is currently ready and the timeout is non-zero, we park on
/// the first fd wait source and re-poll on wake. Returns the number of
/// fds with non-zero `revents`.
///
/// Behaviour gaps (called out so a future caller doesn't trip on
/// them):
/// - Only POLLIN/POLLOUT are honoured; POLLERR / etc. are currently
///   suppressed. TTY write polls still return "ready" eagerly.
/// - Multi-fd waits park on the FIRST unready fd that exposes a wait
///   source. If a different fd becomes ready first, it is noticed on a
///   later wake.
/// - The `timeout_ptr` is read but a non-NULL timeout uses the
///   timeout-elapsed branch only as an upper bound; the actual
///   timer hookup ships with the OnTimer wave (deferred).
/// - The signal mask is ignored.
pub(super) async fn sys_ppoll<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fds_ptr = args[0];
    let nfds = args[1];
    let timeout_ptr = args[2];

    if nfds > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if nfds == 0 {
        return SyscallResult::Return(0);
    }

    const POLLFD_BYTES: u64 = 8;
    const POLLIN: i16 = 0x0001;
    const POLLOUT: i16 = 0x0004;
    const POLLNVAL: i16 = 0x0020;

    let wait_allowed = timeout_ptr == 0; // NULL = infinite wait
    let _ = timeout_ptr;

    let ready = loop {
        // Pump any pending loopback packets so socket readiness reflects the
        // latest delivered data before we sample each fd.
        super::socket::drive_loopback_pending();
        let mut ready: i64 = 0;
        let mut park_source = None;
        for i in 0..nfds {
            let ent_ptr = fds_ptr.wrapping_add(i * POLLFD_BYTES);
            let mut ent_bytes = [0u8; POLLFD_BYTES as usize];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
                return SyscallResult::error_from(errno);
            }
            let fd = i32::from_le_bytes(ent_bytes[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(ent_bytes[4..6].try_into().unwrap());
            let mut revents: i16 = 0;
            if fd >= 0 {
                if let Some(file) = resolve_fd(&ctx.process, fd as u32) {
                    if events & POLLIN != 0 {
                        let (is_ready, source) = select_fd_ready(&file, SelectDir::Read);
                        if is_ready {
                            revents |= POLLIN;
                        } else if park_source.is_none() {
                            park_source = source;
                        }
                    }
                    if events & POLLOUT != 0 {
                        let (is_ready, source) = select_fd_ready(&file, SelectDir::Write);
                        if is_ready {
                            revents |= POLLOUT;
                        } else if park_source.is_none() {
                            park_source = source;
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
                return SyscallResult::error_from(errno);
            }
        }

        if ready > 0 {
            break ready;
        }
        if !wait_allowed {
            break 0;
        }
        let Some(park) = park_source else {
            break 0;
        };
        await_select_park(ctx, park).await;
    };

    SyscallResult::Return(ready)
}

/// `pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask)`.
///
/// This is the Linux generic syscall used by musl's `select(3)`
/// wrapper on RV64. The implementation is intentionally narrow but
/// real: it rewrites fd_set outputs, reports pipe/TTY readiness by
/// level, and parks on the first pipe/TTY wait source when no fd is
/// ready and the timeout is not the zero timeout. Signal-mask handling
/// is deferred, matching the existing `ppoll` surface.
pub(super) async fn sys_pselect6<'a, P: tx_hal::TimeIf + tx_hal::ConsoleIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let nfds = args[0];
    let readfds_ptr = args[1];
    let writefds_ptr = args[2];
    let exceptfds_ptr = args[3];
    let timeout_ptr = args[4];

    let bytes = match fdset_bytes(nfds) {
        Ok(bytes) => bytes,
        Err(result) => return result,
    };

    if nfds == 0 {
        return SyscallResult::Return(0);
    }

    let timeout_ns = if timeout_ptr == 0 {
        None
    } else {
        let mut ts = [0u8; 16];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ts, timeout_ptr) {
            return SyscallResult::error_from(errno);
        }
        let sec = i64::from_le_bytes(ts[0..8].try_into().unwrap());
        let nsec = i64::from_le_bytes(ts[8..16].try_into().unwrap());
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        Some(
            (sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(nsec as u64),
        )
    };
    let wait_allowed = timeout_ns != Some(0);

    let mut readfds = alloc::vec![0u8; bytes];
    let mut writefds = alloc::vec![0u8; bytes];
    let mut exceptfds = alloc::vec![0u8; bytes];
    if readfds_ptr != 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut readfds, readfds_ptr) {
            return SyscallResult::error_from(errno);
        }
    }
    if writefds_ptr != 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut writefds, writefds_ptr) {
            return SyscallResult::error_from(errno);
        }
    }
    if exceptfds_ptr != 0 {
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut exceptfds, exceptfds_ptr) {
            return SyscallResult::error_from(errno);
        }
    }
    // Absolute deadline for a finite, non-zero timeout. Used to bound a socket
    // wait so a select(2) that expects to time out (e.g. negative "no packet"
    // checks) returns 0 instead of parking forever. (zero timeout => wait_allowed
    // is false; None => infinite.)
    let deadline_ns: Option<u64> = match timeout_ns {
        Some(ns) if ns != 0 => Some(<P as tx_hal::TimeIf>::read_ns().saturating_add(ns)),
        _ => None,
    };
    loop {
        // Pump any pending loopback packets so socket readiness reflects the
        // latest delivered data before we sample each fd.
        super::socket::drive_loopback_pending();
        let mut out_read = readfds.clone();
        let mut out_write = writefds.clone();
        let mut out_except = exceptfds.clone();
        let mut ready = 0i64;
        let mut park_source = None;

        for fd in 0..nfds {
            if readfds_ptr != 0 && fdset_get(&readfds, fd) {
                let Some(file) = resolve_fd(&ctx.process, fd as u32) else {
                    return SyscallResult::Error(EBADF_VALUE);
                };
                let (is_ready, source) = select_fd_ready(&file, SelectDir::Read);
                if is_ready {
                    ready += 1;
                } else {
                    fdset_clear(&mut out_read, fd);
                    if park_source.is_none() {
                        park_source = source;
                    }
                }
            }
            if writefds_ptr != 0 && fdset_get(&writefds, fd) {
                let Some(file) = resolve_fd(&ctx.process, fd as u32) else {
                    return SyscallResult::Error(EBADF_VALUE);
                };
                let (is_ready, source) = select_fd_ready(&file, SelectDir::Write);
                if is_ready {
                    ready += 1;
                } else {
                    fdset_clear(&mut out_write, fd);
                    if park_source.is_none() {
                        park_source = source;
                    }
                }
            }
            if exceptfds_ptr != 0 && fdset_get(&exceptfds, fd) {
                // No exceptional conditions are modelled yet.
                fdset_clear(&mut out_except, fd);
            }
        }

        if ready > 0 || !wait_allowed || park_source.is_none() {
            if readfds_ptr != 0 {
                if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, readfds_ptr, &out_read) {
                    return SyscallResult::error_from(errno);
                }
            }
            if writefds_ptr != 0 {
                if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, writefds_ptr, &out_write) {
                    return SyscallResult::error_from(errno);
                }
            }
            if exceptfds_ptr != 0 {
                if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, exceptfds_ptr, &out_except)
                {
                    return SyscallResult::error_from(errno);
                }
            }
            return SyscallResult::Return(ready);
        }

        let Some(park) = park_source else {
            if let Some(ns) = timeout_ns {
                if ns != 0 {
                    sleep_for_select_timeout::<P>(ctx, ns).await;
                }
            }
            return SyscallResult::Return(0);
        };
        // A socket wait with a finite deadline races the source against a timer
        // so the select returns 0 on timeout instead of parking forever.
        if let (SelectPark::Socket(token), Some(dl)) = (&park, deadline_ns) {
            let token = *token;
            let timed_out = match tx_subsystems::wait_source::wait_on_token(token) {
                Some(mut fut) => {
                    if <P as tx_hal::TimeIf>::read_ns() >= dl {
                        true
                    } else if let Some(mut timer) = tx_subsystems::timer_sleep::sleep_until_ns(dl) {
                        core::future::poll_fn(|cx| {
                            if core::future::Future::poll(core::pin::Pin::new(&mut fut), cx)
                                .is_ready()
                            {
                                return core::task::Poll::Ready(false);
                            }
                            if core::future::Future::poll(core::pin::Pin::new(&mut timer), cx)
                                .is_ready()
                            {
                                return core::task::Poll::Ready(true);
                            }
                            core::task::Poll::Pending
                        })
                        .await
                    } else {
                        let _ = fut.await;
                        false
                    }
                }
                None => {
                    tx_reactor::yield_now().await;
                    false
                }
            };
            if timed_out {
                if readfds_ptr != 0 {
                    if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, readfds_ptr, &out_read) {
                        return SyscallResult::error_from(errno);
                    }
                }
                if writefds_ptr != 0 {
                    if let Err(errno) =
                        bootstrap_copy_to_user(&ctx.aspace, writefds_ptr, &out_write)
                    {
                        return SyscallResult::error_from(errno);
                    }
                }
                if exceptfds_ptr != 0 {
                    if let Err(errno) =
                        bootstrap_copy_to_user(&ctx.aspace, exceptfds_ptr, &out_except)
                    {
                        return SyscallResult::error_from(errno);
                    }
                }
                return SyscallResult::Return(0);
            }
            continue;
        }
        await_select_park(ctx, park).await;
    }
}

async fn sleep_for_select_timeout<'a, P: tx_hal::TimeIf>(ctx: &SyscallCtx<'a>, ns: u64) {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = tx_subsystems::vfs::composite::NanosleepOp {
        nanos: ns,
        deadline_ns: <P as tx_hal::TimeIf>::read_ns().saturating_add(ns),
        started: false,
    };
    let _ = drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await;
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
    emit_debug_counter(b"debug.write.pagebacked.len", len as i64);
    if len == 0 {
        return SyscallResult::Return(0);
    }
    emit_debug_counter(b"debug.write.pagebacked.phase", 0);

    // Prefault: eagerly materialise every user page and publish to
    // the pmap so the step loop below finds every page in the cache.
    // If any page is unmapped or has a prot mismatch, fail before
    // transferring any bytes.
    if let Some(range) = super::user_copy::covering_user_range(buf_ptr as u64, len) {
        use crate::adapter::step_engine::StepOutcome as V3;
        use tx_subsystems::vm::UserAccessKind;
        let _guard = crate::adapter::step_engine::guard();
        emit_debug_counter(b"debug.write.pagebacked.phase", 1);
        match ctx
            .aspace
            .reserve_user_range_for_access(range, UserAccessKind::Read)
        {
            V3::Done(()) => {
                emit_debug_counter(b"debug.write.pagebacked.phase", 2);
            }
            V3::Err(e) => {
                emit_debug_counter(b"debug.write.pagebacked.err", 1);
                let errno: tx_subsystems::execution::Errno = e.into();
                return SyscallResult::error_from(errno);
            }
            V3::Yield { .. } | V3::Continue { .. } => {
                emit_debug_counter(b"debug.write.pagebacked.err", 2);
                return SyscallResult::error_from(tx_subsystems::execution::Errno::EIO);
            }
        }
    } else {
        emit_debug_counter(b"debug.write.pagebacked.phase", 2);
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
    emit_debug_counter(b"debug.write.pagebacked.phase", 3);
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
            emit_debug_counter(b"debug.write.pagebacked.done", total as i64);
            emit_debug_counter(b"debug.write.pagebacked.phase", 4);
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            emit_debug_counter(b"debug.write.pagebacked.err", 3);
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

async fn sys_pipe_write_buffered<'a>(
    payload: &Cap<tx_subsystems::pipe::PipePayload>,
    bytes: &[u8],
    nonblocking: bool,
    packet_mode: bool,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::pipe::WriteOp;

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let mode = if nonblocking || mailbox_arc.is_none() {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = WriteOp {
        payload,
        bytes,
        nonblocking,
        packet_mode,
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

async fn sys_pipe_read_buffered<'a>(
    payload: &Cap<tx_subsystems::pipe::PipePayload>,
    buf_ptr: usize,
    len: usize,
    nonblocking: bool,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::pipe::ReadOp;

    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let mode = if nonblocking || mailbox_arc.is_none() {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = ReadOp {
        payload,
        out: &mut staging,
        nonblocking,
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

pub(super) async fn sys_write<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;
    emit_debug_counter(b"debug.write.enter", fd as i64);
    emit_debug_counter(b"debug.write.len", len as i64);

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
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
    // value to the counter. Dispatch before any VFS-shaped rnode()
    // access because eventfd is a non-VFS OpenFile backing.
    if file.eventfd().is_some() {
        return super::eventfd::sys_eventfd_write(&file, args[1], len, ctx).await;
    }
    if let Some((_rx, tx)) = file.socketpair_endpoint() {
        if !file.flags().write {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
        if len == 0 {
            return SyscallResult::Return(0);
        }
        let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, buf_ptr as u64) {
            return SyscallResult::error_from(errno);
        }
        return sys_pipe_write_buffered(&tx, &bytes, file.flags().nonblocking, false, ctx).await;
    }

    let len = if matches!(
        file.rnode().backing(),
        tx_subsystems::vfs::RNodeBacking::PageBacked { .. }
    ) {
        len
    } else {
        core::cmp::min(len, TTY_WRITE_MAX_INLINE)
    };

    if static_char_device(&file) == Some(StaticCharDevice::Null) {
        if !file.flags().write {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        return SyscallResult::Return(len as i64);
    }

    if let Some((pipe, tx_subsystems::pipe::PipeSide::Writer)) = file.pipe_endpoint() {
        if len == 0 {
            return SyscallResult::Return(0);
        }
        let mut bytes: alloc::vec::Vec<u8> = alloc::vec![0u8; len];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut bytes, buf_ptr as u64) {
            return SyscallResult::error_from(errno);
        }
        return sys_pipe_write_buffered(
            &pipe,
            &bytes,
            file.flags().nonblocking,
            file.flags().packet,
            ctx,
        )
        .await;
    }

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

    sys_write_buffered(&file, &bytes, ctx).await
}

async fn sys_write_buffered<'a>(
    file: &Cap<OpenFile>,
    bytes: &[u8],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
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
        file,
        bytes,
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

fn emit_debug_counter(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
        tx_observe::dump_registered_if_requested();
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
    if let Some((rx, _tx)) = file.socketpair_endpoint() {
        if !file.flags().read {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let len = core::cmp::min(len, TTY_WRITE_MAX_INLINE);
        if len == 0 {
            return SyscallResult::Return(0);
        }
        return sys_pipe_read_buffered(&rx, buf_ptr, len, file.flags().nonblocking, ctx).await;
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

    if static_char_device(&file) == Some(StaticCharDevice::Zero) {
        if !file.flags().read {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        if let Err(errno) =
            bootstrap_copy_to_user(&ctx.aspace, buf_ptr as u64, &STATIC_ZERO_READ_BUF[..len])
        {
            return SyscallResult::error_from(errno);
        }
        return SyscallResult::Return(len as i64);
    }

    if let Some((pipe, tx_subsystems::pipe::PipeSide::Reader)) = file.pipe_endpoint() {
        return sys_pipe_read_buffered(&pipe, buf_ptr, len, file.flags().nonblocking, ctx).await;
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
    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_write(args, ctx).await;
    file.set_offset(saved);
    result
}

fn positioned_vector_offset(args: [u64; 6]) -> Result<u64, SyscallResult> {
    let lo = args[3];
    let hi = args[4];
    let offset = lo | (hi << 32);
    if (offset as i64) < 0 {
        Err(SyscallResult::Error(EINVAL_VALUE))
    } else {
        Ok(offset)
    }
}

pub(super) async fn sys_preadv<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let fd = args[0] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let offset = match positioned_vector_offset(args) {
        Ok(offset) => offset,
        Err(err) => return err,
    };
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_readv::<P>(args, ctx).await;
    file.set_offset(saved);
    result
}

pub(super) async fn sys_pwritev<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    let offset = match positioned_vector_offset(args) {
        Ok(offset) => offset,
        Err(err) => return err,
    };
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let saved = file.offset();
    file.set_offset(offset);
    let result = sys_writev(args, ctx).await;
    file.set_offset(saved);
    result
}

pub(super) async fn sys_preadv2<'a, P: tx_hal::TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    if args[5] != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    sys_preadv::<P>(args, ctx).await
}

pub(super) async fn sys_pwritev2<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    if args[5] != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    sys_pwritev(args, ctx).await
}

pub(super) fn sys_fadvise64_64<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let offset = args[1] as i64;
    let len = args[2] as i64;
    let advice = args[3] as i32;
    if fd < 0 || resolve_fd(&ctx.process, fd as u32).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if offset < 0 || len < 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if !(0..=5).contains(&advice) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_readahead<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    if fd < 0 || resolve_fd(&ctx.process, fd as u32).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_sync_file_range<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let offset = args[1] as i64;
    let nbytes = args[2] as i64;
    let flags = args[3] as u32;
    const SYNC_FILE_RANGE_KNOWN: u32 = 0x1 | 0x2 | 0x4;
    if fd < 0 || resolve_fd(&ctx.process, fd as u32).is_none() {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if offset < 0 || nbytes < 0 || flags & !SYNC_FILE_RANGE_KNOWN != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    SyscallResult::Return(0)
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

pub(super) async fn sys_copy_file_range<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    use tx_subsystems::page_backed::step_copy_file_range;
    use tx_subsystems::vfs::structure::RNodeBacking;

    let in_fd = args[0] as i32;
    let off_in_ptr = args[1];
    let out_fd = args[2] as i32;
    let off_out_ptr = args[3];
    let len = args[4] as usize;
    let flags = args[5] as u32;

    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if in_fd < 0 || out_fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len == 0 {
        return SyscallResult::Return(0);
    }

    let in_file = match resolve_fd(&ctx.process, in_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let out_file = match resolve_fd(&ctx.process, out_fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let in_pc = match in_file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let out_pc = match out_file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc.clone(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };

    let in_offset = if off_in_ptr != 0 {
        match bootstrap_read_user::<u64>(&ctx.aspace, off_in_ptr) {
            Ok(offset) => offset,
            Err(errno) => return SyscallResult::error_from(errno),
        }
    } else {
        in_file.offset()
    };
    let out_offset = if off_out_ptr != 0 {
        match bootstrap_read_user::<u64>(&ctx.aspace, off_out_ptr) {
            Ok(offset) => offset,
            Err(errno) => return SyscallResult::error_from(errno),
        }
    } else {
        out_file.offset()
    };

    let guard = tx_substrate::epoch::guard();
    let outcome = step_copy_file_range(&in_pc, in_offset, &out_pc, out_offset, len, &guard);
    drop(guard);

    let transferred = match outcome {
        tx_substrate::step::StepOutcome::Done(n) => n,
        tx_substrate::step::StepOutcome::Err(e) => {
            let errno: tx_subsystems::execution::Errno = e.into();
            return SyscallResult::error_from(errno);
        }
        _ => return SyscallResult::Error(EAGAIN_VALUE),
    };

    if transferred > 0 {
        let new_in = in_offset + transferred as u64;
        let new_out = out_offset + transferred as u64;
        if off_in_ptr != 0 {
            if let Err(_errno) = bootstrap_write_user::<u64>(&ctx.aspace, off_in_ptr, new_in) {
                return SyscallResult::Return(transferred as i64);
            }
        } else {
            in_file.advance_offset(transferred as u64);
        }
        if off_out_ptr != 0 {
            if let Err(_errno) = bootstrap_write_user::<u64>(&ctx.aspace, off_out_ptr, new_out) {
                return SyscallResult::Return(transferred as i64);
            }
        } else {
            out_file.advance_offset(transferred as u64);
        }
    }

    SyscallResult::Return(transferred as i64)
}
