//! `sys_timerfd_create(2)` / `sys_timerfd_settime` / `sys_timerfd_gettime`
//! + timerfd-shaped `read(2)`.
//!
//! A timerfd is an fd that becomes readable when a timer expires.
//! Subsystem dispatch lives in `tx_subsystems::timerfd`.

use tx_subsystems::execution::Errno;
use tx_subsystems::timerfd::{
    ITIMERSPEC_BYTES, ItimerSpec, TimerFd, step_timerfd_read, timerfd_settime,
};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::wait_source;

use super::numbers::{
    CLOCK_MONOTONIC, CLOCK_REALTIME, NR_TIMERFD_CREATE, NR_TIMERFD_GETTIME, NR_TIMERFD_SETTIME,
    TFD_CLOEXEC_FLAG, TFD_NONBLOCK_FLAG, TFD_TIMER_ABSTIME_FLAG,
};
use super::{
    EAGAIN_VALUE, EBADF_VALUE, EINVAL_VALUE, ENOMEM_VALUE, SyscallCtx, SyscallResult,
    bootstrap_copy_to_user, bootstrap_read_user, bootstrap_write_user, errno_to_i32,
};
use crate::adapter::step_engine::{self as step_engine};

// === timerfd_create ===================================================

/// `timerfd_create(clockid, flags)` syscall arm.
///
/// Per `man 2 timerfd_create`:
/// - `clockid`: `CLOCK_REALTIME` (0) or `CLOCK_MONOTONIC` (1).
/// - `flags`: `TFD_CLOEXEC`, `TFD_NONBLOCK`. Other bits → `-EINVAL`.
///
/// Returns:
/// - `Return(fd)` on success.
/// - `Error(EINVAL)` if `clockid` not recognised or bad flags.
/// - `Error(ENOMEM)` if zone allocation fails.
pub(super) fn sys_timerfd_create<'a>(
    clockid: u32,
    flags: u32,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    // Validate clockid.
    if clockid != CLOCK_REALTIME && clockid != CLOCK_MONOTONIC {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let recognised = TFD_CLOEXEC_FLAG | TFD_NONBLOCK_FLAG;
    if flags & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let cloexec = (flags & TFD_CLOEXEC_FLAG) != 0;
    let nonblocking = (flags & TFD_NONBLOCK_FLAG) != 0;

    // Create the timerfd cap via a one-shot step op.
    let tfd_cap = {
        use tx_subsystems::timerfd::TimerfdCreateOp;
        let mut script_ctx = super::build_subject_script_ctx(ctx);
        let mut op = TimerfdCreateOp {
            flags, // pass flags through; subsystem stores them
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(Ok(cap)) => cap,
            Ok(Err(_)) => return SyscallResult::Error(ENOMEM_VALUE),
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };

    let open_flags = OpenFileFlags {
        read: true,
        write: false,
        append: false,
        cloexec,
        nonblocking,
    };
    let open_cap = match OpenFile::new_timerfd_cap(tfd_cap, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    let new_fd = ctx.process.allocate_fd();
    let _ = ctx.process.install_fd(new_fd, open_cap);
    if cloexec {
        ctx.process.set_fd_cloexec(new_fd, true);
    }
    SyscallResult::Return(new_fd as i64)
}

// === timerfd_settime ==================================================

/// `timerfd_settime(fd, flags, new_value, old_value)` syscall arm.
///
/// Per `man 2 timerfd_settime`:
/// - `flags`: 0 or `TFD_TIMER_ABSTIME`.
/// - `new_value`: pointer to `struct itimerspec`.
/// - `old_value`: pointer to `struct itimerspec` (may be null).
///
/// Returns:
/// - `Return(0)` on success.
/// - `Error(EBADF)` if `fd` doesn't name a timerfd.
/// - `Error(EFAULT)` if `new_value` or `old_value` pointer is bad.
pub(super) fn sys_timerfd_settime<'a, P: super::TimeIf>(
    fd: u32,
    flags: u32,
    new_value_ptr: u64,
    old_value_ptr: u64,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let tfd_cap = match file.timerfd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    let abstime = (flags & TFD_TIMER_ABSTIME_FLAG) != 0;

    // Read new_value from userspace.
    let new_bytes = [0u8; ITIMERSPEC_BYTES];
    if let Err(errno) = bootstrap_read_user::<[u8; ITIMERSPEC_BYTES]>(&ctx.aspace, new_value_ptr) {
        return SyscallResult::error_from(errno);
    }
    let new_value = ItimerSpec::from_bytes(&new_bytes);

    let now_ns = P::read_ns();

    // Read old_value if requested (before mutating).
    let old_spec = if old_value_ptr != 0 {
        let mut old = ItimerSpec::default();
        timerfd_settime(tfd_cap, abstime, now_ns, new_value, Some(&mut old));
        Some(old)
    } else {
        timerfd_settime(tfd_cap, abstime, now_ns, new_value, None);
        None
    };

    // Write old_value back to userspace if requested.
    if let Some(old) = old_spec {
        let old_bytes = old.to_bytes();
        if let Err(errno) =
            bootstrap_write_user::<[u8; ITIMERSPEC_BYTES]>(&ctx.aspace, old_value_ptr, old_bytes)
        {
            return SyscallResult::error_from(errno);
        }
    }

    SyscallResult::Return(0)
}

// === timerfd_gettime ==================================================

/// `timerfd_gettime(fd, curr_value)` syscall arm.
///
/// Per `man 2 timerfd_gettime`:
/// - `curr_value`: pointer to `struct itimerspec`.
///
/// Returns:
/// - `Return(0)` on success.
/// - `Error(EBADF)` if `fd` doesn't name a timerfd.
/// - `Error(EFAULT)` if `curr_value` pointer is bad.
pub(super) fn sys_timerfd_gettime<'a>(
    fd: u32,
    curr_value_ptr: u64,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let tfd_cap = match file.timerfd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    let spec = ItimerSpec {
        it_interval_ns: tfd_cap.interval_ns(),
        it_value_ns: tfd_cap.deadline_ns(),
    };
    let bytes = spec.to_bytes();
    if let Err(errno) =
        bootstrap_write_user::<[u8; ITIMERSPEC_BYTES]>(&ctx.aspace, curr_value_ptr, bytes)
    {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

// === timerfd-shaped read ==============================================

/// timerfd-shaped `read(2)` arm.  Returns the number of timer
/// expirations as an 8-byte little-endian unsigned integer.
///
/// Returns:
/// - `Return(8)` on a successful read.
/// - `Error(EINVAL)` if `len < 8`.
/// - `Error(EAGAIN)` if no expirations and fd is non-blocking.
/// - Parks on the timerfd's wait source (via nanosleep-style deadline
///   wait backed by the reactor's timer queue) otherwise.
pub(super) async fn sys_timerfd_read<P: super::TimeIf>(
    file: &OpenFile,
    buf_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let tfd_cap = match file.timerfd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if len == 0 {
        return SyscallResult::Return(0);
    }
    if len < 8 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let nonblocking = file.flags().nonblocking;
    use tx_subsystems::execution::WaitToken;
    loop {
        let now_ns = P::read_ns();
        let mut staging = [0u8; 8];
        let outcome = step_timerfd_read(tfd_cap, now_ns, &mut staging, nonblocking);
        use step_engine::{StepOutcome as V3Out, YieldShape};
        match outcome {
            V3Out::Done(n) => {
                if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, buf_ptr, &staging[..n]) {
                    return SyscallResult::error_from(errno);
                }
                return SyscallResult::Return(n as i64);
            }
            V3Out::Err(v3errno) => {
                let errno: Errno = v3errno.into();
                if errno == Errno::EAGAIN {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                return SyscallResult::error_from(errno);
            }
            V3Out::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
                ..
            } => {
                // Try the deadline-based sleep before falling back to
                // the wait source.  If the timer has an armed deadline,
                // park until that deadline instead of waiting for an
                // arbitrary wake.
                let deadline = tfd_cap.deadline_ns();
                if deadline > 0 && deadline > now_ns {
                    // Use the reactor's timer queue to sleep until
                    // the deadline.
                    if let Some(future) = tx_subsystems::timer_sleep::sleep_until_ns(deadline) {
                        let _ = future.await;
                        continue; // re-poll
                    }
                }
                // Fall back to the classic wait-source path.
                let token = WaitToken::new(carrier.raw(), interests.raw());
                if let Some(future) = wait_source::wait_on_token(token) {
                    let _ = future.await;
                }
            }
            V3Out::Continue { .. } | V3Out::Yield { .. } => {
                return SyscallResult::error_from(Errno::EIO);
            }
        }
    }
}

/// Silence unused-import warnings.
const _: fn() = || {
    let _ = core::mem::size_of::<TimerFd>();
    let _ = NR_TIMERFD_CREATE;
    let _ = NR_TIMERFD_SETTIME;
    let _ = NR_TIMERFD_GETTIME;
};
