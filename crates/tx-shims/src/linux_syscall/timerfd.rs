//! `sys_timerfd_create(2)` / `sys_timerfd_settime` / `sys_timerfd_gettime`
//! + timerfd-shaped `read(2)`.
//!
//! A timerfd is an fd that becomes readable when a timer expires.
//! Subsystem dispatch lives in `tx_subsystems::timerfd`.

use core::marker::PhantomData;
use tx_services::time::{
    timekeeper, timekeeper_clock, ClockRead, DeadlineRegistrar, DeadlineRegistrarHandle,
    TimekeeperClock, TimekeeperIf,
};
use tx_subsystems::execution::Errno;
use tx_subsystems::timerfd::{
    step_timerfd_read, timerfd_deadline_fired_with_post, timerfd_settime_with_flags_and_post,
    ItimerSpec, TimerFd, ITIMERSPEC_BYTES,
};
use tx_subsystems::vfs::structure::OpenFileFlags;
use tx_subsystems::vfs::OpenFile;

use super::numbers::{
    CLOCK_MONOTONIC, CLOCK_REALTIME, NR_TIMERFD_CREATE, NR_TIMERFD_GETTIME, NR_TIMERFD_SETTIME,
    TFD_CLOEXEC_FLAG, TFD_NONBLOCK_FLAG, TFD_TIMER_ABSTIME_FLAG, TFD_TIMER_CANCEL_ON_SET_FLAG,
};
use super::{
    bootstrap_copy_to_user, bootstrap_read_user, bootstrap_write_user, errno_to_i32, SyscallCtx,
    SyscallResult, EAGAIN_VALUE, EBADF_VALUE, EINVAL_VALUE, ENOMEM_VALUE,
};
use crate::adapter::step_engine::{
    self as step_engine, ByteProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
};

struct TimerfdReadOp<'a, P, F>
where
    TimekeeperClock<P>: ClockRead,
    F: FnMut(&tx_substrate::wake::TaskMailbox, tx_substrate::wake::MailboxEvent) -> bool,
{
    tfd: &'a TimerFd,
    out: &'a mut [u8; 8],
    nonblocking: bool,
    timer_registrar: Option<DeadlineRegistrarHandle>,
    post: F,
    _platform: PhantomData<fn() -> P>,
}

impl<P, F, I> StepOp<I> for TimerfdReadOp<'_, P, F>
where
    TimekeeperClock<P>: ClockRead,
    F: FnMut(&tx_substrate::wake::TaskMailbox, tx_substrate::wake::MailboxEvent) -> bool,
    I: SubjectIdentity,
{
    type Output = usize;
    type Progress = ByteProgress;

    fn step(&mut self, ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        let _ = ctx.subject();
        let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
        // A deadline delivery is only a wait-source hint. Reconcile it in the
        // timerfd owner before draining its expiration count.
        let _ = timerfd_deadline_fired_with_post(
            self.tfd,
            now_ns,
            self.timer_registrar
                .as_ref()
                .map(|registrar| registrar as &dyn DeadlineRegistrar),
            &mut self.post,
        );
        step_timerfd_read(self.tfd, self.out, self.nonblocking)
    }
}

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
            clockid,
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
        packet: false,
    };
    let open_cap = match OpenFile::new_timerfd_cap(tfd_cap, open_flags) {
        Ok(cap) => cap,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    let Some(new_fd) = ctx.process.install_new_fd(open_cap, cloexec) else {
        return SyscallResult::Error(super::EMFILE_VALUE);
    };
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
pub(super) fn sys_timerfd_settime<'a, P>(
    fd: u32,
    flags: u32,
    new_value_ptr: u64,
    old_value_ptr: u64,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let file = match ctx.process.fd(fd) {
        Some(f) => f,
        None => return SyscallResult::Error(EBADF_VALUE),
    };
    let tfd_cap = match file.timerfd() {
        Some(cap) => cap,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    let recognised_flags = TFD_TIMER_ABSTIME_FLAG | TFD_TIMER_CANCEL_ON_SET_FLAG;
    if flags & !recognised_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let abstime = (flags & TFD_TIMER_ABSTIME_FLAG) != 0;

    // Read new_value from userspace.
    let new_bytes = match bootstrap_read_user::<[u8; ITIMERSPEC_BYTES]>(&ctx.aspace, new_value_ptr)
    {
        Ok(bytes) => bytes,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let Some(new_value) = ItimerSpec::try_from_bytes(&new_bytes) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
    let generation = timekeeper().realtime_generation();
    let timer_registrar = ctx
        .timer_registrar
        .as_ref()
        .map(|registrar| registrar as &dyn DeadlineRegistrar);

    // Read old_value if requested (before mutating).
    let old_spec = if old_value_ptr != 0 {
        let mut old = ItimerSpec::default();
        timerfd_settime_with_flags_and_post(
            tfd_cap,
            abstime,
            now_ns,
            new_value,
            Some(&mut old),
            flags,
            generation,
            timer_registrar,
            |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        );
        Some(old)
    } else {
        timerfd_settime_with_flags_and_post(
            tfd_cap,
            abstime,
            now_ns,
            new_value,
            None,
            flags,
            generation,
            timer_registrar,
            |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        );
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
pub(super) fn sys_timerfd_gettime<'a, P>(
    fd: u32,
    curr_value_ptr: u64,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
        it_value_ns: tfd_cap.remaining_value_ns(timekeeper_clock::<P>().monotonic_now_ns()),
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
///   wait backed by the shared timer registry) otherwise.
pub(super) async fn sys_timerfd_read<P>(
    file: &OpenFile,
    buf_ptr: u64,
    len: usize,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
    let mut staging = [0u8; 8];
    let mut script_ctx = super::build_subject_script_ctx(ctx);
    let mailbox = script_ctx
        .mailbox()
        .cloned()
        .unwrap_or_else(|| alloc::sync::Arc::new(tx_substrate::wake::TaskMailbox::new()));
    let timer_registrar = script_ctx.timer_registrar().cloned();
    let delegate_registry = script_ctx.delegate_registry().cloned();
    let op = TimerfdReadOp::<P, _> {
        tfd: &tfd_cap,
        out: &mut staging,
        nonblocking,
        timer_registrar: timer_registrar.clone(),
        post: |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
        _platform: PhantomData,
    };
    match tx_scripts::drive(
        op,
        &mut script_ctx,
        if nonblocking {
            step_engine::DriveMode::Nonblocking
        } else {
            step_engine::DriveMode::Waiting
        },
        Some(&mailbox),
        delegate_registry.as_deref(),
        timer_registrar.as_ref(),
    )
    .await
    {
        Ok(read) => {
            if let Err(errno) = bootstrap_copy_to_user(&ctx.aspace, buf_ptr, &staging[..read]) {
                return SyscallResult::error_from(errno);
            }
            SyscallResult::Return(read as i64)
        }
        Err(errno) => SyscallResult::error_from(errno.into()),
    }
}

/// Silence unused-import warnings.
const _: fn() = || {
    let _ = core::mem::size_of::<TimerFd>();
    let _ = NR_TIMERFD_CREATE;
    let _ = NR_TIMERFD_SETTIME;
    let _ = NR_TIMERFD_GETTIME;
};
