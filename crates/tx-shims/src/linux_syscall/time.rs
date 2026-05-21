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
struct TmsLayout {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

pub(super) mod layout_descriptors {
    use core::mem::{align_of, offset_of, size_of};

    pub(super) use super::TmsLayout;
    use super::{TimespecLayout, TimevalLayout};
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
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
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
    let tv = ns_to_timeval(<P as TimeIf>::read_ns());
    if let Err(errno) = bootstrap_write_user::<TimevalLayout>(&ctx.aspace, tv_uaddr, tv) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
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
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = NanosleepOp {
        nanos: req_ns,
        deadline_ns,
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
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
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
    let now = <P as TimeIf>::read_ns();
    let deadline_ns = if (flags & TIMER_ABSTIME) != 0 {
        req_ns
    } else {
        now.saturating_add(req_ns)
    };
    if now >= deadline_ns {
        return SyscallResult::Return(0);
    }
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = NanosleepOp {
        nanos: req_ns,
        deadline_ns,
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
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}
