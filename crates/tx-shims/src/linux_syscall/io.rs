//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;


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

    if iovcnt < 0 || iovcnt > 1024 {
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

    if iovcnt < 0 || iovcnt > 1024 {
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
///   TTY wait carrier until UART RX delivers bytes. End-to-end
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
pub(super) async fn sys_ppoll<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let fds_ptr = args[0];
    let nfds = args[1];

    if nfds > 1024 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if nfds == 0 {
        return SyscallResult::Return(0);
    }

    const POLLFD_BYTES: u64 = 8;
    let mut ready: i64 = 0;
    for i in 0..nfds {
        let ent_ptr = fds_ptr.wrapping_add(i * POLLFD_BYTES);
        let mut ent_bytes = [0u8; POLLFD_BYTES as usize];
        if let Err(errno) = bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
        let fd = i32::from_le_bytes(ent_bytes[0..4].try_into().unwrap());
        let events = i16::from_le_bytes(ent_bytes[4..6].try_into().unwrap());
        // revents = events for fd >= 0; revents = 0 for fd < 0.
        let revents: i16 = if fd >= 0 { events } else { 0 };
        if fd >= 0 {
            ready += 1;
        }
        ent_bytes[6..8].copy_from_slice(&revents.to_le_bytes());
        if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, ent_ptr, &ent_bytes) {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }
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

    // Loop on the canonical async wait discipline pattern from
    // `vm::execution::fault_script`. Each iteration takes a fresh
    // `tx_substrate::epoch::guard()` inside the step's call site so
    // the guard never crosses an `.await`.
    let mut total: usize = 0;
    let mut remaining = bytes.as_slice();
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            file.step_write(remaining, &guard)
        };
        match outcome {
            StepOutcome::Done(written) | StepOutcome::Advanced(written) => {
                total += written;
                let stop = written == 0 || written >= remaining.len();
                if stop {
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[written..];
            }
            StepOutcome::AdvancedThenBlocked(written, token) => {
                total += written;
                if written >= remaining.len() {
                    return SyscallResult::Return(total as i64);
                }
                remaining = &remaining[written..];
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
                // Otherwise the carrier has been retired or is a test
                // placeholder; fall through and retry immediately.
            }
            StepOutcome::Blocked(token) => {
                // No progress made yet; await the carrier and retry.
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
            }
            StepOutcome::Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total as i64);
                }
                // fd-ops Wave 3 — Q2 DECIDED 2026-05-07. SIGPIPE is
                // delivered to the calling process before returning
                // `-EPIPE` to userspace. The pipe `step_write` cannot
                // do this itself (no process Cap); the syscall arm
                // is the right boundary because it has `ctx.process`.
                if errno == Errno::EPIPE {
                    let _ = tx_subsystems::signal::step_kill_process(
                        &ctx.process,
                        tx_subsystems::signal::Signum::SIGPIPE,
                    );
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
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
/// input queue, the dispatcher awaits `wait_carrier::wait_on_token`,
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

    // Read into a kernel-side staging buffer, then copy out through
    // the canonical user-VA lane (`bootstrap_copy_to_user` bridges
    // via `aspace.copy_to_user`, falling back to the kernel-pointer
    // dance the trio's earlier exemption used).
    let mut staging: alloc::vec::Vec<u8> = alloc::vec![0u8; len];

    let mut total: usize = 0;
    let mut cursor: usize = 0;
    loop {
        let outcome = {
            let guard = tx_substrate::epoch::guard();
            file.step_read(&mut staging[cursor..], &guard)
        };
        match outcome {
            StepOutcome::Done(read) | StepOutcome::Advanced(read) => {
                if read > 0 {
                    if let Err(errno) = bootstrap_copy_to_user(
                        &ctx.aspace,
                        buf_ptr as u64 + cursor as u64,
                        &staging[cursor..cursor + read],
                    ) {
                        if total > 0 {
                            return SyscallResult::Return(total as i64);
                        }
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                total += read;
                let stop = read == 0 || cursor + read >= len;
                if stop {
                    return SyscallResult::Return(total as i64);
                }
                cursor += read;
            }
            StepOutcome::AdvancedThenBlocked(read, _token) => {
                if read > 0 {
                    if let Err(errno) = bootstrap_copy_to_user(
                        &ctx.aspace,
                        buf_ptr as u64 + cursor as u64,
                        &staging[cursor..cursor + read],
                    ) {
                        if total > 0 {
                            return SyscallResult::Return(total as i64);
                        }
                        return SyscallResult::Error(errno_to_i32(errno));
                    }
                }
                total += read;
                if total > 0 {
                    // Partial-success policy: same as `write`. Return
                    // what we got rather than blocking; userspace
                    // re-issues the syscall to drain more.
                    return SyscallResult::Return(total as i64);
                }
                // total == 0 here is unreachable in practice (Advanced
                // implies progress) but fall through defensively to
                // the `Blocked` arm below.
                return SyscallResult::Return(0);
            }
            StepOutcome::Blocked(token) => {
                // Pre-ELF Phase 5 (item 9): no input buffered yet.
                // Park on the registered TTY wait carrier (fired
                // from `tty::execution::step_ingest` after UART RX
                // bytes land via `irq::uart_rx_irq_handler`), then
                // re-poll. Mirrors the canonical async wait
                // discipline pattern from
                // `vm::execution::fault_script` /
                // `RangeLock::WouldBlock`.
                //
                // `wait_on_token` returns `None` for test
                // placeholder tokens (carrier id not registered);
                // in that case fall through and re-poll
                // immediately. Production carriers are always
                // registered (see `TtyIdentity::new`). If a partial
                // read already happened on a prior iteration
                // (`total > 0`) we return what we have rather than
                // block, matching `sys_write`'s partial-success
                // policy.
                if total > 0 {
                    return SyscallResult::Return(total as i64);
                }
                if let Some(future) = wait_carrier::wait_on_token(token) {
                    let _ = future.await;
                }
            }
            StepOutcome::Err(errno) => {
                if total > 0 {
                    return SyscallResult::Return(total as i64);
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
        }
    }
}

