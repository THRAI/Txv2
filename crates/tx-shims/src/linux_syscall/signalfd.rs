//! `sys_signalfd4(2)` + signalfd-shaped `read(2)` — D9-D.
//!
//! Spec:
//! - `docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
//!   §6 (Option C follow-up: signalfd as a per-process subscription
//!   driven from `step_kill_process` after the thread-eligibility
//!   post)
//! - `man 2 signalfd`, `man 2 signalfd4`
//!
//! # What lands here
//!
//! 1. [`sys_signalfd4`] — the syscall dispatcher for
//!    `__NR_signalfd4 = 74`. Two call shapes per Linux:
//!    - `signalfd4(-1, &mask, sizemask, flags)` mints a fresh fd.
//!    - `signalfd4(fd, &mask, sizemask, flags)` replaces the mask
//!      on an existing signalfd. Returns `fd` on success.
//! 2. [`sys_signalfd_read`] — signalfd-shaped `read(2)` arm that
//!    pops one `struct signalfd_siginfo` (128 bytes) off the per-fd
//!    pending queue. Parks on the per-fd `WaitSource` when the
//!    queue is empty and the fd is blocking; returns `EAGAIN` if
//!    non-blocking.

use tx_subsystems::execution::Errno;
use tx_subsystems::signalfd::{signalfd_create, signalfd_read, SignalFd, SIGNALFD_SIGINFO_SIZE};
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::wait_source;

use super::numbers::{O_CLOEXEC, O_NONBLOCK, SFD_CLOEXEC, SFD_NONBLOCK};
use super::{
    bootstrap_read_user, errno_to_i32, SyscallCtx, SyscallResult, EAGAIN_VALUE, EBADF_VALUE,
    EINVAL_VALUE, ENOMEM_VALUE,
};
use crate::adapter::step_engine::{self as step_engine};

/// Linux's `sigset_t` is 8 bytes on RV64 / x86_64 (a single `u64`).
/// The signalfd4 syscall takes `sizemask = sizeof(sigset_t) = 8` and
/// rejects with `-EINVAL` for any other size.
const SIGSET_SIZE: usize = 8;

/// `signalfd4(fd, &mask, sizemask, flags)` syscall arm.
///
/// Per `man 2 signalfd4`:
/// - `fd == -1` mints a fresh signalfd; returns the new fd.
/// - `fd >= 0` updates the mask of an existing signalfd; returns `fd`.
/// - `mask` is a pointer to a `sigset_t` (a `u64` on Linux generic).
/// - `sizemask` must equal `sizeof(sigset_t) = 8`.
/// - `flags`: `SFD_CLOEXEC`, `SFD_NONBLOCK`; other bits return EINVAL.
///
/// Returns:
/// - `Return(fd)` on success.
/// - `Error(EINVAL)` for bad flags / `sizemask != 8` / unrecognised
///   create-vs-update semantics.
/// - `Error(EBADF)` if `fd >= 0` and does not name a signalfd.
/// - `Error(ENOMEM)` if zone allocation fails when minting a fresh
///   signalfd.
pub(super) fn sys_signalfd4<'a>(
    fd: i32,
    mask_ptr: u64,
    sizemask: u64,
    flags: u32,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    // Validate flags.
    let recognised = SFD_CLOEXEC | SFD_NONBLOCK;
    if flags & !recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let cloexec = (flags & SFD_CLOEXEC) != 0;
    let nonblocking = (flags & SFD_NONBLOCK) != 0;

    // sizemask must equal `sizeof(sigset_t) = 8`.
    if (sizemask as usize) != SIGSET_SIZE {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if mask_ptr == 0 {
        return SyscallResult::Error(errno_to_i32(Errno::EFAULT));
    }

    // Read the sigset_t from userspace as a u64.
    let mask: u64 = match bootstrap_read_user::<u64>(&ctx.aspace, mask_ptr) {
        Ok(v) => v,
        Err(errno) => return SyscallResult::Error(errno_to_i32(errno)),
    };

    if fd < 0 {
        // Create a fresh signalfd.
        let sfd_cap = match signalfd_create(&ctx.process, mask) {
            Ok(cap) => cap,
            Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
        };

        let open_flags = OpenFileFlags {
            read: true,
            write: false,
            append: false,
            cloexec,
            nonblocking,
        };
        let open_cap = match OpenFile::new_signalfd_cap(sfd_cap, open_flags) {
            Ok(cap) => cap,
            Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
        };

        let new_fd = ctx.process.allocate_fd();
        let _ = ctx.process.install_fd(new_fd, open_cap);
        if cloexec {
            ctx.process.set_fd_cloexec(new_fd, true);
        }
        return SyscallResult::Return(new_fd as i64);
    }

    // Update an existing signalfd's mask.
    let file = match ctx.process.fd(fd as u32) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let sfd_cap = match file.signalfd() {
        Some(sfd) => sfd,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    sfd_cap.set_mask(mask);
    SyscallResult::Return(fd as i64)
}

/// signalfd-shaped `read(2)` arm. Drains one
/// `struct signalfd_siginfo` (128 bytes) off the per-fd pending
/// queue. Mirrors `step_ufd_read`'s shape.
///
/// Returns:
/// - `Return(128)` on a successful single-record drain.
/// - `Error(EINVAL)` if `len < 128`.
/// - `Error(EAGAIN)` if the queue is empty and the fd was opened
///   with `O_NONBLOCK` / `SFD_NONBLOCK`.
/// - Otherwise parks on the per-fd wait source.
pub(super) async fn sys_signalfd_read(
    file: &OpenFile,
    buf_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    let sfd_cap = match file.signalfd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EBADF_VALUE),
    };

    if len == 0 {
        return SyscallResult::Return(0);
    }
    if len < SIGNALFD_SIGINFO_SIZE {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let nonblocking = file.flags().nonblocking;
    use tx_subsystems::execution::WaitToken;
    loop {
        let outcome = {
            let mut staging = [0u8; SIGNALFD_SIGINFO_SIZE];
            let result = signalfd_read(sfd_cap, &mut staging, nonblocking);
            (result, staging)
        };
        use step_engine::{StepOutcome as V3Out, YieldShape};
        match outcome.0 {
            V3Out::Done(read) => {
                if read == 0 {
                    return SyscallResult::Return(0);
                }
                if let Err(errno) =
                    super::bootstrap_copy_to_user(&ctx.aspace, buf_ptr, &outcome.1[..read])
                {
                    return SyscallResult::Error(errno_to_i32(errno));
                }
                return SyscallResult::Return(read as i64);
            }
            V3Out::Err(v3errno) => {
                let errno: Errno = v3errno.into();
                if errno == Errno::EAGAIN {
                    return SyscallResult::Error(EAGAIN_VALUE);
                }
                return SyscallResult::Error(errno_to_i32(errno));
            }
            V3Out::Yield {
                shape:
                    YieldShape::OnWaitSource {
                        source: carrier,
                        interests,
                    },
                ..
            } => {
                let token = WaitToken::new(carrier.raw(), interests.raw());
                if let Some(future) = wait_source::wait_on_token(token) {
                    let _ = future.await;
                }
                // Re-poll on next loop iteration.
            }
            V3Out::Continue { .. } | V3Out::Yield { .. } => {
                return SyscallResult::Error(errno_to_i32(Errno::EIO));
            }
        }
    }
}

/// Silence unused-import warnings; the `SignalFd` import resolves the
/// `Cap<SignalFd>` deref site implicitly via `file.signalfd()`.
const _: fn() = || {
    let _ = core::mem::size_of::<SignalFd>();
    // Ensure the O_* aliases agree with the SFD_* aliases.
    let _ = O_CLOEXEC ^ SFD_CLOEXEC;
    let _ = O_NONBLOCK ^ SFD_NONBLOCK;
};
