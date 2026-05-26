//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct TimespecLayout {
    pub(super) tv_sec: i64,
    pub(super) tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct TimevalLayout {
    pub(super) tv_sec: i64,
    pub(super) tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct ItimervalLayout {
    pub(super) it_interval: TimevalLayout,
    pub(super) it_value: TimevalLayout,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TmsLayout {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

pub(super) mod layout_descriptors {
    use core::mem::{align_of, offset_of, size_of};

    pub(super) use super::TmsLayout;
    use super::{ItimervalLayout, TimespecLayout, TimevalLayout};
    use crate::linux_syscall::{KernelToUserLayout, KernelUserField, KernelUserLayout};

    impl KernelToUserLayout for TimespecLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "TimespecLayout",
            musl_header: "time.h",
            musl_type: "struct timespec",
            size: size_of::<TimespecLayout>(),
            align: align_of::<TimespecLayout>(),
            fields: &[
                KernelUserField {
                    rust: "tv_sec",
                    musl: "tv_sec",
                    offset: offset_of!(TimespecLayout, tv_sec),
                },
                KernelUserField {
                    rust: "tv_nsec",
                    musl: "tv_nsec",
                    offset: offset_of!(TimespecLayout, tv_nsec),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const TIMESPEC_LAYOUT: KernelUserLayout =
        <TimespecLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for TimevalLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "TimevalLayout",
            musl_header: "sys/time.h",
            musl_type: "struct timeval",
            size: size_of::<TimevalLayout>(),
            align: align_of::<TimevalLayout>(),
            fields: &[
                KernelUserField {
                    rust: "tv_sec",
                    musl: "tv_sec",
                    offset: offset_of!(TimevalLayout, tv_sec),
                },
                KernelUserField {
                    rust: "tv_usec",
                    musl: "tv_usec",
                    offset: offset_of!(TimevalLayout, tv_usec),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const TIMEVAL_LAYOUT: KernelUserLayout =
        <TimevalLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for ItimervalLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "ItimervalLayout",
            musl_header: "sys/time.h",
            musl_type: "struct itimerval",
            size: size_of::<ItimervalLayout>(),
            align: align_of::<ItimervalLayout>(),
            fields: &[
                KernelUserField {
                    rust: "it_interval.tv_sec",
                    musl: "it_interval.tv_sec",
                    offset: offset_of!(ItimervalLayout, it_interval)
                        + offset_of!(TimevalLayout, tv_sec),
                },
                KernelUserField {
                    rust: "it_interval.tv_usec",
                    musl: "it_interval.tv_usec",
                    offset: offset_of!(ItimervalLayout, it_interval)
                        + offset_of!(TimevalLayout, tv_usec),
                },
                KernelUserField {
                    rust: "it_value.tv_sec",
                    musl: "it_value.tv_sec",
                    offset: offset_of!(ItimervalLayout, it_value)
                        + offset_of!(TimevalLayout, tv_sec),
                },
                KernelUserField {
                    rust: "it_value.tv_usec",
                    musl: "it_value.tv_usec",
                    offset: offset_of!(ItimervalLayout, it_value)
                        + offset_of!(TimevalLayout, tv_usec),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const ITIMERVAL_LAYOUT: KernelUserLayout =
        <ItimervalLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for TmsLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "TmsLayout",
            musl_header: "sys/times.h",
            musl_type: "struct tms",
            size: size_of::<TmsLayout>(),
            align: align_of::<TmsLayout>(),
            fields: &[
                KernelUserField {
                    rust: "tms_utime",
                    musl: "tms_utime",
                    offset: offset_of!(TmsLayout, tms_utime),
                },
                KernelUserField {
                    rust: "tms_stime",
                    musl: "tms_stime",
                    offset: offset_of!(TmsLayout, tms_stime),
                },
                KernelUserField {
                    rust: "tms_cutime",
                    musl: "tms_cutime",
                    offset: offset_of!(TmsLayout, tms_cutime),
                },
                KernelUserField {
                    rust: "tms_cstime",
                    musl: "tms_cstime",
                    offset: offset_of!(TmsLayout, tms_cstime),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const TMS_LAYOUT: KernelUserLayout =
        <TmsLayout as KernelToUserLayout>::LAYOUT;
}

/// Convert a nanosecond count to a Linux-shaped `(tv_sec, tv_nsec)`
/// pair. Both fields are signed 64-bit per the uapi.
pub(super) fn ns_to_timespec(ns: u64) -> TimespecLayout {
    TimespecLayout {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_nsec: (ns % 1_000_000_000) as i64,
    }
}

/// Convert a nanosecond count to a Linux-shaped `(tv_sec, tv_usec)`
/// pair (microsecond resolution — `gettimeofday` truncates the
/// sub-microsecond residue).
pub(super) fn ns_to_timeval(ns: u64) -> TimevalLayout {
    TimevalLayout {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_usec: ((ns % 1_000_000_000) / 1_000) as i64,
    }
}

pub(super) fn realtime_ns<P: TimeIf>() -> u64 {
    tx_subsystems::wall_clock::realtime_now_ns::<P>()
}

/// Read a Linux-shaped `(tv_sec, tv_nsec)` pair from user memory and
/// fold it back into a nanosecond count. Returns `None` if either
/// field is negative or `tv_nsec` overflows the canonical
/// `[0, 1_000_000_000)` range — those are the two `-EINVAL` cases
/// `nanosleep(2)` documents (`req->tv_nsec >= 1_000_000_000` or
/// either field negative).
///
/// Bridges through `bootstrap_read_user::<TimespecLayout>` for the
/// user-VA copy.
pub(super) fn read_timespec_at(aspace: &AddressSpace, uaddr: u64) -> Option<u64> {
    if uaddr == 0 {
        return None;
    }
    let ts: TimespecLayout = match bootstrap_read_user::<TimespecLayout>(aspace, uaddr) {
        Ok(v) => v,
        Err(_) => return None,
    };
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return None;
    }
    Some((ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64))
}

fn read_timeval_at(aspace: &AddressSpace, uaddr: u64) -> Option<u64> {
    if uaddr == 0 {
        return None;
    }
    let tv: TimevalLayout = match bootstrap_read_user::<TimevalLayout>(aspace, uaddr) {
        Ok(v) => v,
        Err(_) => return None,
    };
    if tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1_000_000 {
        return None;
    }
    Some((tv.tv_sec as u64).saturating_mul(1_000_000_000) + (tv.tv_usec as u64) * 1_000)
}

fn timeval_to_ns(tv: TimevalLayout) -> Option<u64> {
    if tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1_000_000 {
        return None;
    }
    Some((tv.tv_sec as u64).saturating_mul(1_000_000_000) + (tv.tv_usec as u64) * 1_000)
}

fn read_itimerval_at(aspace: &AddressSpace, uaddr: u64) -> Result<(u64, u64), Errno> {
    if uaddr == 0 {
        return Err(Errno::EFAULT);
    }
    let it: ItimervalLayout = bootstrap_read_user::<ItimervalLayout>(aspace, uaddr)?;
    let interval_ns = timeval_to_ns(it.it_interval).ok_or(Errno::EINVAL)?;
    let value_ns = timeval_to_ns(it.it_value).ok_or(Errno::EINVAL)?;
    Ok((value_ns, interval_ns))
}

fn itimerval_from_ns(value_ns: u64, interval_ns: u64) -> ItimervalLayout {
    ItimervalLayout {
        it_interval: ns_to_timeval(interval_ns),
        it_value: ns_to_timeval(value_ns),
    }
}

fn can_set_realtime(ctx: &SyscallCtx<'_>) -> bool {
    ctx.cred().euid.is_root()
}

/// `clock_gettime(clk_id, tp)`. Linux RV64 generic ABI
/// `__NR_clock_gettime = 113`.
///
/// Day-1 surface: every recognised clock id (REALTIME / MONOTONIC /
/// PROCESS_CPUTIME / THREAD_CPUTIME plus the *_RAW / *_COARSE /
/// BOOTTIME aliases) routes to `<P as TimeIf>::read_ns()`. Unknown
/// clock ids return `-EINVAL`. Null `tp` returns `-EFAULT`.
pub(super) fn sys_clock_gettime<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let clk_id = args[0] as u32;
    let ts_uaddr = args[1];
    if ts_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let ns = match clk_id {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => realtime_ns::<P>(),
        CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME => <P as TimeIf>::read_ns(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let ts = ns_to_timespec(ns);
    if let Err(errno) = bootstrap_write_user::<TimespecLayout>(&ctx.aspace, ts_uaddr, ts) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

/// `clock_settime(clk_id, tp)`. Linux RV64 generic ABI
/// `__NR_clock_settime = 112`.
///
/// v1 supports setting `CLOCK_REALTIME` by replacing the wallclock
/// offset above HAL monotonic time. Other clock ids are not settable
/// and return `-EINVAL`; unprivileged callers return `-EPERM`.
pub(super) fn sys_clock_settime<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let clk_id = args[0] as u32;
    let ts_uaddr = args[1];
    if clk_id != CLOCK_REALTIME {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if ts_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if !can_set_realtime(ctx) {
        return SyscallResult::Error(EPERM_VALUE);
    }
    let Some(ns) = read_timespec_at(&ctx.aspace, ts_uaddr) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    match tx_subsystems::wall_clock::set_realtime_ns::<P>(ns) {
        Ok(_) => SyscallResult::Return(0),
        Err(_) => SyscallResult::Error(EINVAL_VALUE),
    }
}

/// `clock_getres(clk_id, res)`. Linux RV64 generic ABI
/// `__NR_clock_getres = 114`.
///
/// Mirrors `clock_gettime`'s v1 clock-id surface. The platform time
/// source is nanosecond-shaped, so the fixed reported resolution is
/// one nanosecond. Linux permits a null `res` pointer; it still
/// validates the clock id first.
pub(super) fn sys_clock_getres<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let clk_id = args[0] as u32;
    match clk_id {
        CLOCK_REALTIME
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME => {}
        _ => return SyscallResult::Error(EINVAL_VALUE),
    }

    let res_uaddr = args[1];
    if res_uaddr != 0 {
        let res = TimespecLayout {
            tv_sec: 0,
            tv_nsec: 1,
        };
        if let Err(errno) = bootstrap_write_user::<TimespecLayout>(&ctx.aspace, res_uaddr, res) {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(0)
}

/// `gettimeofday(tv, tz)`. Linux RV64 generic ABI
/// `__NR_gettimeofday = 169`.
///
/// The `tz` argument (args[1]) is deprecated on Linux and ignored.
/// Null `tv` returns `-EFAULT`.
pub(super) fn sys_gettimeofday<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let tv_uaddr = args[0];
    // args[1] = tz (ignored — deprecated on Linux).
    if tv_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let tv = ns_to_timeval(realtime_ns::<P>());
    if let Err(errno) = bootstrap_write_user::<TimevalLayout>(&ctx.aspace, tv_uaddr, tv) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

/// `settimeofday(tv, tz)`. Linux RV64 generic ABI
/// `__NR_settimeofday = 170`.
///
/// The deprecated timezone argument is ignored for v1; setting time
/// goes through the same wallclock offset as `clock_settime`.
pub(super) fn sys_settimeofday<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let tv_uaddr = args[0];
    if tv_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if !can_set_realtime(ctx) {
        return SyscallResult::Error(EPERM_VALUE);
    }
    let Some(ns) = read_timeval_at(&ctx.aspace, tv_uaddr) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    match tx_subsystems::wall_clock::set_realtime_ns::<P>(ns) {
        Ok(_) => SyscallResult::Return(0),
        Err(_) => SyscallResult::Error(EINVAL_VALUE),
    }
}

/// `times(buf)`. Linux RV64 generic ABI `__NR_times = 153`.
///
/// Returns the monotonic tick count at `_SC_CLK_TCK = 100Hz`. Writes
/// `tms_utime = ticks` and zeros the other three fields when `buf` is
/// non-null. Null `buf` is permitted per Linux semantics — only the
/// return value matters in that case (LTP `times02` covers this).
pub(super) fn sys_times<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let buf_uaddr = args[0];
    let ns = <P as TimeIf>::read_ns();
    let ticks = (ns / TIMES_NS_PER_TICK) as i64;
    if buf_uaddr != 0 {
        let tms = TmsLayout {
            tms_utime: ticks,
            tms_stime: 0,
            tms_cutime: 0,
            tms_cstime: 0,
        };
        if let Err(errno) = bootstrap_write_user::<TmsLayout>(&ctx.aspace, buf_uaddr, tms) {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(ticks)
}

/// `getitimer(which, value)`. Linux RV64 generic ABI
/// `__NR_getitimer = 102`.
///
/// v1 implements the `ITIMER_REAL` shape used by musl's `alarm(2)`
/// wrapper. CPU-time timers (`ITIMER_VIRTUAL` / `ITIMER_PROF`) remain
/// deferred until process CPU accounting exists.
pub(super) fn sys_getitimer<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let which = args[0] as u32;
    let value_uaddr = args[1];
    if value_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if which != ITIMER_REAL {
        return match which {
            ITIMER_VIRTUAL | ITIMER_PROF => SyscallResult::Error(EINVAL_VALUE),
            _ => SyscallResult::Error(EINVAL_VALUE),
        };
    }
    let now_ns = <P as TimeIf>::read_ns();
    let timer = ctx.process.real_timer_snapshot(now_ns);
    let it = itimerval_from_ns(timer.deadline_ns, timer.interval_ns);
    if let Err(errno) = bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, value_uaddr, it) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

/// `setitimer(which, value, ovalue)`. Linux RV64 generic ABI
/// `__NR_setitimer = 103`.
pub(super) fn sys_setitimer<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let which = args[0] as u32;
    let value_uaddr = args[1];
    let old_uaddr = args[2];
    if which != ITIMER_REAL {
        return match which {
            ITIMER_VIRTUAL | ITIMER_PROF => SyscallResult::Error(EINVAL_VALUE),
            _ => SyscallResult::Error(EINVAL_VALUE),
        };
    }
    let (value_ns, interval_ns) = match read_itimerval_at(&ctx.aspace, value_uaddr) {
        Ok(v) => v,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let now_ns = <P as TimeIf>::read_ns();
    let old = ctx.process.set_real_timer(now_ns, value_ns, interval_ns);
    if old_uaddr != 0 {
        let old_it = itimerval_from_ns(old.deadline_ns, old.interval_ns);
        if let Err(errno) = bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, old_uaddr, old_it)
        {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(0)
}

/// `nanosleep(req, rem)`. Linux RV64 generic ABI
/// `__NR_nanosleep = 101`.
///
/// Validates `*req` (returns `-EINVAL` on negative fields or
/// `tv_nsec >= 1_000_000_000`); zero-duration requests short-circuit
/// immediately to `Return(0)`. Non-zero durations park the task on the
/// reactor's timer queue until the absolute deadline passes, then
/// return `0`. Null `req` returns `-EFAULT`.
///
/// `rem` (args[1]) is ignored — no EINTR path wired yet.
#[cfg_attr(not(test), allow(clippy::extra_unused_type_parameters))]
#[cfg_attr(test, allow(clippy::extra_unused_type_parameters))]
pub(super) async fn sys_nanosleep<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let req_uaddr = args[0];
    let req_ns = match read_timespec_at(&ctx.aspace, req_uaddr) {
        Some(ns) => ns,
        None if req_uaddr == 0 => return SyscallResult::Error(EFAULT_VALUE),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if req_ns == 0 {
        return SyscallResult::Return(0);
    }
    let deadline_ns = <P as TimeIf>::read_ns().saturating_add(req_ns);
    drive_nanosleep_until::<P>(ctx, req_ns, deadline_ns, args[1]).await
}

/// `clock_nanosleep(clk_id, flags, req, rem)`. Linux RV64 generic ABI
/// `__NR_clock_nanosleep = 115`.
///
/// Same semantics as `nanosleep` plus `TIMER_ABSTIME`: when set `req`
/// is an absolute deadline; past deadlines return `0` immediately.
/// Recognised clock ids match `clock_gettime`. Unknown clock ids or
/// flag bits return `-EINVAL`. Null `req` returns `-EFAULT`.
pub(super) async fn sys_clock_nanosleep<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let clk_id = args[0] as u32;
    let flags = args[1] as u32;
    let req_uaddr = args[2];

    match clk_id {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME => {}
        _ => return SyscallResult::Error(EINVAL_VALUE),
    }
    if (flags & !TIMER_ABSTIME) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let req_ns = match read_timespec_at(&ctx.aspace, req_uaddr) {
        Some(ns) => ns,
        None if req_uaddr == 0 => return SyscallResult::Error(EFAULT_VALUE),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if (flags & TIMER_ABSTIME) != 0 && (clk_id == CLOCK_REALTIME || clk_id == CLOCK_REALTIME_COARSE)
    {
        loop {
            if realtime_ns::<P>() >= req_ns {
                return SyscallResult::Return(0);
            }
            if ctx.timer_wheel.is_none() {
                return SyscallResult::Return(0);
            }
            let deadline_ns =
                tx_subsystems::wall_clock::monotonic_deadline_from_realtime_ns(req_ns);
            if <P as TimeIf>::read_ns() >= deadline_ns {
                continue;
            }
            match drive_nanosleep_until::<P>(ctx, req_ns, deadline_ns, 0).await {
                SyscallResult::Return(0) => continue,
                other => return other,
            }
        }
    }

    let deadline_ns = if (flags & TIMER_ABSTIME) != 0 {
        req_ns
    } else {
        <P as TimeIf>::read_ns().saturating_add(req_ns)
    };
    if <P as TimeIf>::read_ns() >= deadline_ns {
        return SyscallResult::Return(0);
    }
    let rem_uaddr = if (flags & TIMER_ABSTIME) == 0 {
        args[3]
    } else {
        0
    };
    drive_nanosleep_until::<P>(ctx, req_ns, deadline_ns, rem_uaddr).await
}

async fn drive_nanosleep_until<'a, P: TimeIf>(
    ctx: &SyscallCtx<'a>,
    req_ns: u64,
    deadline_ns: u64,
    rem_uaddr: u64,
) -> SyscallResult {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    let wait_deadline_ns = ctx
        .process
        .real_timer_deadline_ns()
        .map(|alarm_deadline| alarm_deadline.min(deadline_ns))
        .unwrap_or(deadline_ns);
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = NanosleepOp {
        nanos: req_ns,
        deadline_ns: wait_deadline_ns,
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
        Ok(()) => {
            let now_ns = <P as TimeIf>::read_ns();
            if ctx.process.fire_real_timer_if_due(now_ns) {
                let _ =
                    tx_subsystems::signal::step_kill_process(&ctx.process, Signum::SIGALRM, None);
                if rem_uaddr != 0 {
                    let remaining = deadline_ns.saturating_sub(now_ns);
                    let rem = ns_to_timespec(remaining);
                    if let Err(errno) =
                        bootstrap_write_user::<TimespecLayout>(&ctx.aspace, rem_uaddr, rem)
                    {
                        return SyscallResult::error_from(errno);
                    }
                }
                SyscallResult::Error(EINTR_VALUE)
            } else {
                SyscallResult::Return(0)
            }
        }
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}
