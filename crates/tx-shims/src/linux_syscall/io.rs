//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::reactor_entry;
use crate::adapter::step_engine::Cap;
use crate::adapter::step_engine::{self as step_engine};

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
    let mut total: i64 = 0;
    for i in 0..iovcnt as u64 {
        let ent_ptr = iov_ptr.wrapping_add(i * IOVEC_BYTES);
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(errno_to_i32(errno));
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
    }
    SyscallResult::Return(total)
}

/// `readv(fd, iov, iovcnt)` — scatter-read counterpart of `sys_writev`.
pub(super) async fn sys_readv<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
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
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        if len == 0 {
            continue;
        }

        let read_args = [args[0], base, len, 0, 0, 0];
        match sys_read(read_args, ctx).await {
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
pub(super) async fn sys_ppoll<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    use tx_subsystems::vfs::structure::{RNodeBacking, StructPayload};

    let fds_ptr = args[0];
    let nfds = args[1];
    let timeout_ptr = args[2];

    if nfds > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if nfds == 0 {
        return SyscallResult::Return(0);
    }

    // Pollfd layout: { i32 fd; i16 events; i16 revents } — 8 bytes
    // packed on Linux RV64 generic ABI.
    const POLLFD_BYTES: u64 = 8;
    const POLLIN: i16 = 0x0001;

    // Snapshot the timeout treatment: NULL → infinite wait, else
    // treat any non-NULL pointer as "wait but bounded" — the timer
    // wire isn't actually consulted today (see header comment), so
    // for non-NULL we still wait on the carrier (the re-poll loop
    // returns whatever's ready on wake) and trust the caller to
    // retry. Empty timespec ({0,0}) would be the "poll-without-wait"
    // shape, but distinguishing it from "wait forever" requires
    // reading two u64s; the busybox flow uses NULL = forever, so we
    // only implement that branch precisely.
    let wait_allowed = true;
    let _ = timeout_ptr; // see header note

    // First pass: read each pollfd, check readability against the
    // backing, write back `revents`, and remember the first TTY fd
    // that requested POLLIN but isn't currently readable. That fd's
    // wait source is what we park on if nothing is ready.
    let mut park_on_tty: Option<Cap<tx_subsystems::tty::structure::TtyIdentity>> = None;
    let ready = loop {
        let mut ready: i64 = 0;
        for i in 0..nfds {
            let ent_ptr = fds_ptr.wrapping_add(i * POLLFD_BYTES);
            let mut ent_bytes = [0u8; POLLFD_BYTES as usize];
            if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
            let fd = i32::from_le_bytes(ent_bytes[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(ent_bytes[4..6].try_into().unwrap());
            let mut revents: i16 = 0;
            if fd >= 0 {
                if let Some(file) = resolve_fd(&ctx.process, fd as u32) {
                    if events & POLLIN != 0 {
                        // TTY backing: peek the readable level.
                        if let RNodeBacking::StructBacked {
                            payload: StructPayload::Tty(tty),
                        } = file.rnode().backing()
                        {
                            if tty_readable_level(tty) {
                                revents |= POLLIN;
                            } else if park_on_tty.is_none() {
                                park_on_tty = Some(tty.clone());
                            }
                        } else {
                            // Non-TTY backings: punt to the legacy
                            // "always ready" shape so files / pipes
                            // / chardevs don't regress to a hang.
                            revents |= POLLIN;
                        }
                    }
                    // POLLOUT-only polls: legacy semantics — TTYs
                    // and most other fds are always writable in v1.
                    let pollout: i16 = 0x0004;
                    if events & pollout != 0 {
                        revents |= pollout;
                    }
                } else {
                    let pollnval: i16 = 0x0020;
                    revents = pollnval;
                }
            }
            if revents != 0 {
                ready += 1;
            }
            ent_bytes[6..8].copy_from_slice(&revents.to_le_bytes());
            if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, ent_ptr, &ent_bytes) {
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }

        if ready > 0 {
            break ready;
        }
        if !wait_allowed {
            break 0;
        }
        let Some(tty) = park_on_tty.take() else {
            // No carrier to park on (all fds non-TTY, none ready) —
            // give up rather than spin.
            break 0;
        };
        wait_for_tty_readable(tty).await;
    };

    SyscallResult::Return(ready)
}

pub(super) async fn sys_write<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len > TTY_WRITE_MAX_INLINE {
        return SyscallResult::Error(E2BIG_VALUE);
    }

    // Resolve fd → Cap<OpenFile> against the process payload's stub
    // fd table. Holding the payload guard across the lookup is fine —
    // the resulting Cap is independent and the lock is released before
    // any `.await`.
    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

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
            return SyscallResult::Error(errno_to_i32(errno));
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
    use step_engine::StepOp;
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileWriteOp;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let guard = step_engine::guard();
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mut op = OpenFileWriteOp {
        file: &file,
        bytes: &bytes,
        guard: &guard,
        cursor: 0,
    };
    match drive(op, &mut script_ctx, mode, None, None, None).await {
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
                );
            }
            SyscallResult::Error(errno_to_i32(errno))
        }
    }
}

/// `read(fd, buf, count)`.
///
/// Mirrors `sys_write`'s structure: resolve fd → `Cap<OpenFile>`,
/// route the user buffer through `bootstrap_copy_to_user`, and loop
/// on the wait-carrier discipline.
///
/// **Blocking semantic.** Pre-ELF Phase 5 (item 9) wires the UART RX
/// path so a blocked `read(0, ...)` actually parks until bytes arrive:
/// `tty::execution::step_read` returns `Blocked(token)` on an empty
/// input queue, the dispatcher awaits `wait_source::wait_on_token`,
/// and `tx_kernel::irq::uart_rx_irq_handler` drives
/// `tty::execution::step_ingest` from the IRQ side, which fires the
/// TTY's wait `Channel`. On any partial progress (`total > 0`)
/// the dispatcher returns what it has rather than block again,
/// matching `sys_write`'s partial-success policy.
pub(super) async fn sys_read<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fd = args[0] as i32;
    let buf_ptr = args[1] as usize;
    let len = args[2] as usize;

    if fd < 0 {
        return SyscallResult::Error(EBADF_VALUE);
    }
    if len > TTY_WRITE_MAX_INLINE {
        return SyscallResult::Error(E2BIG_VALUE);
    }

    let file = match resolve_fd(&ctx.process, fd as u32) {
        Some(file) => file,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

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
    use step_engine::StepOp;
    use tx_substrate::step::DriveMode;
    use tx_subsystems::vfs::execution::OpenFileReadOp;
    use tx_scripts::drive;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let guard = step_engine::guard();
    let mode = if file.flags().nonblocking {
        DriveMode::Nonblocking
    } else {
        DriveMode::Waiting
    };
    let mut op = OpenFileReadOp {
        file: &file,
        out: &mut staging,
        guard: &guard,
        cursor: 0,
    };
    match drive(op, &mut script_ctx, mode, None, None, None).await {
        Ok(total) => {
            if total > 0 {
                if let Err(errno) = bootstrap_copy_to_user(
                    &ctx.aspace,
                    buf_ptr as u64,
                    &staging[..total],
                ) {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
            }
            SyscallResult::Return(total as i64)
        }
        Err(v3errno) => {
            let errno: tx_subsystems::execution::Errno = v3errno.into();
            SyscallResult::Error(errno_to_i32(errno))
        },
    }
}
