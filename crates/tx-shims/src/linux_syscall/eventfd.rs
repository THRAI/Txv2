//! `sys_eventfd2(2)` + eventfd-shaped `read(2)` / `write(2)`.
//!
//! An eventfd is a 64-bit counter that can be read (drains to zero)
//! or written (adds to the counter).  Subsystem dispatch lives in
//! `tx_subsystems::eventfd`.

use tx_subsystems::eventfd::{step_eventfd_read, step_eventfd_write, EventFd, EFD_SEMAPHORE};
use tx_subsystems::execution::Errno;
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::vfs::OpenFile;

use super::numbers::{EFD_CLOEXEC_FLAG, EFD_NONBLOCK_FLAG, NR_EVENTFD2};
use super::{
    bootstrap_copy_to_user, bootstrap_read_user, errno_to_i32, SyscallCtx, SyscallResult,
    EAGAIN_VALUE, EBADF_VALUE, EINVAL_VALUE, ENOMEM_VALUE,
};
use crate::adapter::step_engine::{self as step_engine};

/// `eventfd2(init_val, flags)` syscall arm.
///
/// Per `man 2 eventfd`:
/// - `init_val` is the initial counter value (u32 in the man page,
///   but Linux treats it as u64 — the `u32` is a historical limit).
/// - `flags`: `EFD_CLOEXEC`, `EFD_NONBLOCK`, `EFD_SEMAPHORE`.
///   Other bits return `-EINVAL`.
///
/// Returns:
/// - `Return(fd)` on success.
/// - `Error(EINVAL)` for bad flags.
/// - `Error(ENOMEM)` if zone allocation fails.
pub(super) fn sys_eventfd2<'a>(init_val: u64, flags: u32, ctx: &SyscallCtx<'a>) -> SyscallResult {
    use super::numbers::EFD_SEMAPHORE_FLAG;

    let recognised = EFD_SEMAPHORE_FLAG | EFD_CLOEXEC_FLAG | EFD_NONBLOCK_FLAG;
    if flags & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let cloexec = (flags & EFD_CLOEXEC_FLAG) != 0;
    let nonblocking = (flags & EFD_NONBLOCK_FLAG) != 0;
    let semaphore = (flags & EFD_SEMAPHORE_FLAG) != 0;

    // Build the subsystem flags — strip CLOEXEC/NONBLOCK (handled at
    // OpenFile level) and keep SEMAPHORE.
    let efd_flags = if semaphore { EFD_SEMAPHORE } else { 0 };

    // Create the eventfd cap via a one-shot step op.
    let efd_cap = {
        use tx_subsystems::eventfd::EventfdCreateOp;
        let mut script_ctx = super::build_subject_script_ctx(ctx);
        let mut op = EventfdCreateOp {
            init_val,
            flags: efd_flags,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(Ok(cap)) => cap,
            Ok(Err(_)) => return SyscallResult::Error(ENOMEM_VALUE),
            Err(v3errno) => return SyscallResult::error_from(Errno::from(v3errno)),
        }
    };

    let open_flags = OpenFileFlags {
        read: true,
        write: true,
        append: false,
        cloexec,
        nonblocking,
        packet: false,
    };
    let open_cap = match OpenFile::new_eventfd_cap(efd_cap, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    let Some(new_fd) = ctx.process.install_new_fd(open_cap, cloexec) else {
        return SyscallResult::Error(super::EMFILE_VALUE);
    };
    SyscallResult::Return(new_fd as i64)
}

/// eventfd-shaped `read(2)` arm.  Drains the 64-bit counter and
/// copies 8 bytes to userspace.
///
/// Returns:
/// - `Return(8)` on a successful read.
/// - `Error(EINVAL)` if `len < 8`.
/// - `Error(EAGAIN)` if counter == 0 and fd is non-blocking.
/// - Parks on the eventfd's read wait source otherwise.
pub(super) async fn sys_eventfd_read(
    file: &OpenFile,
    buf_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let efd_cap = match file.eventfd() {
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
    loop {
        let mut staging = [0u8; 8];
        let outcome = step_eventfd_read(efd_cap, &mut staging, nonblocking);
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
                super::await_wait_source(ctx, carrier, interests).await;
            }
            V3Out::Continue { .. } | V3Out::Yield { .. } => {
                return SyscallResult::error_from(Errno::EIO);
            }
        }
    }
}

/// eventfd-shaped `write(2)` arm.  Adds a 64-bit value to the
/// counter.
///
/// Returns:
/// - `Return(8)` on a successful write.
/// - `Error(EINVAL)` if `len < 8` or `val == u64::MAX`.
/// - `Error(EAGAIN)` if the addition would overflow and the fd is
///   non-blocking.
/// - Parks on the eventfd's write wait source otherwise.
pub(super) async fn sys_eventfd_write(
    file: &OpenFile,
    buf_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let efd_cap = match file.eventfd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if len < 8 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    // Read the 8-byte value from userspace.
    let val: u64 = match bootstrap_read_user::<u64>(&ctx.aspace, buf_ptr) {
        Ok(v) => v,
        Err(errno) => return SyscallResult::error_from(errno),
    };

    let nonblocking = file.flags().nonblocking;
    loop {
        let outcome = step_eventfd_write(efd_cap, val, nonblocking);
        use step_engine::{StepOutcome as V3Out, YieldShape};
        match outcome {
            V3Out::Done(()) => {
                // Linux convention: write to eventfd always returns 8.
                return SyscallResult::Return(8);
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
                super::await_wait_source(ctx, carrier, interests).await;
            }
            V3Out::Continue { .. } | V3Out::Yield { .. } => {
                return SyscallResult::error_from(Errno::EIO);
            }
        }
    }
}

/// Silence unused-import warnings.
const _: fn() = || {
    let _ = core::mem::size_of::<EventFd>();
    let _ = NR_EVENTFD2;
};
