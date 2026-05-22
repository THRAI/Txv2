//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use alloc::collections::BTreeMap;

use crate::adapter::step_engine::{Cap, SpinMutex};
use tx_subsystems::process::ProcessIdentity;

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
struct ItimervalLayout {
    it_interval: TimevalLayout,
    it_value: TimevalLayout,
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

fn timeval_to_ns(tv: TimevalLayout) -> Option<u64> {
    if tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1_000_000 {
        return None;
    }
    (tv.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|sec_ns| sec_ns.checked_add((tv.tv_usec as u64).saturating_mul(1_000)))
}

const REALTIME_EPOCH_BASE_NS: u64 = 1_749_920_000_000_000_000;
const MAX_CLOCK_NANOSLEEP_NS: u64 = 30_000_000_000;
const CLOCK_GETRES_NS: i64 = 2_000_000;

pub(super) fn realtime_ns<P: TimeIf>() -> u64 {
    REALTIME_EPOCH_BASE_NS.saturating_add(<P as TimeIf>::read_ns())
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

fn read_timespec_ns_checked(aspace: &AddressSpace, uaddr: u64) -> Result<u64, SyscallResult> {
    if uaddr == 0 {
        return Err(SyscallResult::Error(EFAULT_VALUE));
    }
    let ts: TimespecLayout = match bootstrap_read_user::<TimespecLayout>(aspace, uaddr) {
        Ok(v) => v,
        Err(errno) => return Err(SyscallResult::error_from(errno)),
    };
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(SyscallResult::Error(EINVAL_VALUE));
    }
    Ok((ts.tv_sec as u64).saturating_mul(1_000_000_000) + (ts.tv_nsec as u64))
}

fn write_remaining_timespec(
    aspace: &AddressSpace,
    rem_uaddr: u64,
    remaining_ns: u64,
) -> SyscallResult {
    if rem_uaddr == 0 {
        return SyscallResult::Return(0);
    }
    let rem = ns_to_timespec(remaining_ns);
    match bootstrap_write_user::<TimespecLayout>(aspace, rem_uaddr, rem) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
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
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_REALTIME_ALARM | CLOCK_TAI => {
            realtime_ns::<P>()
        }
        CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME
        | CLOCK_BOOTTIME_ALARM => <P as TimeIf>::read_ns(),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    let ts = ns_to_timespec(ns);
    if let Err(errno) = bootstrap_write_user::<TimespecLayout>(&ctx.aspace, ts_uaddr, ts) {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

/// `clock_getres(clk_id, res)`. Linux RV64 generic ABI
/// `__NR_clock_getres = 114`.
///
/// The hardware time source can be read at nanosecond scale, but the
/// userspace-visible sleep wakeup path is scheduler/timer-wheel driven and is
/// currently millisecond-ish under QEMU. Report that effective resolution so
/// timer conformance tests do not assume a high-resolution wakeup guarantee we
/// do not actually provide yet.
pub(super) fn sys_clock_getres<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let clk_id = args[0] as u32;
    let res_uaddr = args[1];
    match clk_id {
        CLOCK_REALTIME
        | CLOCK_REALTIME_COARSE
        | CLOCK_REALTIME_ALARM
        | CLOCK_TAI
        | CLOCK_MONOTONIC
        | CLOCK_PROCESS_CPUTIME_ID
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_MONOTONIC_RAW
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME
        | CLOCK_BOOTTIME_ALARM => {}
        _ => return SyscallResult::Error(EINVAL_VALUE),
    }
    if res_uaddr != 0 {
        let ts = TimespecLayout {
            tv_sec: 0,
            tv_nsec: CLOCK_GETRES_NS,
        };
        if let Err(errno) = bootstrap_write_user::<TimespecLayout>(&ctx.aspace, res_uaddr, ts) {
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

const ITIMER_REAL: u32 = 0;
const ITIMER_VIRTUAL: u32 = 1;
const ITIMER_PROF: u32 = 2;
const SIGALRM: u8 = 14;
const SIGVTALRM: u8 = 26;
const SIGPROF: u8 = 27;

#[derive(Clone, Copy, Debug, Default)]
struct IntervalTimer {
    deadline_ns: u64,
    interval_ns: u64,
}

static INTERVAL_TIMERS: SpinMutex<Option<BTreeMap<(u32, u32), IntervalTimer>>> =
    SpinMutex::new(None);

fn with_interval_timers<R>(f: impl FnOnce(&mut BTreeMap<(u32, u32), IntervalTimer>) -> R) -> R {
    let mut guard = INTERVAL_TIMERS.lock();
    let timers = guard.get_or_insert_with(BTreeMap::new);
    f(timers)
}

fn valid_itimer(which: u32) -> bool {
    matches!(which, ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF)
}

fn signal_for_itimer(which: u32) -> Option<tx_subsystems::signal::Signum> {
    let signo = match which {
        ITIMER_REAL => SIGALRM,
        ITIMER_VIRTUAL => SIGVTALRM,
        ITIMER_PROF => SIGPROF,
        _ => return None,
    };
    tx_subsystems::signal::Signum::new(signo)
}

fn itimer_to_layout(timer: Option<IntervalTimer>, now_ns: u64) -> ItimervalLayout {
    let timer = timer.unwrap_or_default();
    ItimervalLayout {
        it_interval: ns_to_timeval(timer.interval_ns),
        it_value: ns_to_timeval(if timer.deadline_ns == 0 {
            0
        } else {
            timer.deadline_ns.saturating_sub(now_ns)
        }),
    }
}

fn parse_itimerval(value: ItimervalLayout) -> Option<(u64, u64)> {
    let interval_ns = timeval_to_ns(value.it_interval)?;
    let value_ns = timeval_to_ns(value.it_value)?;
    Some((interval_ns, value_ns))
}

fn signal_unblocked_on_any_thread(
    process: &Cap<ProcessIdentity>,
    signal: tx_subsystems::signal::Signum,
) -> bool {
    let Some(threads) = process.threads_snapshot() else {
        return false;
    };
    threads.iter().any(|thread| match thread.payload_cap() {
        Some(payload) => !payload.signal_mask().is_blocked(signal),
        None => false,
    })
}

pub(super) fn sys_getitimer<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let which = args[0] as u32;
    let curr_value_ptr = args[1];
    if !valid_itimer(which) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if curr_value_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let now_ns = P::read_ns();
    let timer = with_interval_timers(|timers| timers.get(&(ctx.process.pid.0, which)).copied());
    let value = itimer_to_layout(timer, now_ns);
    match bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, curr_value_ptr, value) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
}

pub(super) fn sys_setitimer<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let which = args[0] as u32;
    let new_value_ptr = args[1];
    let old_value_ptr = args[2];
    if !valid_itimer(which) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if new_value_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let new_value = match bootstrap_read_user::<ItimervalLayout>(&ctx.aspace, new_value_ptr) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let Some((interval_ns, value_ns)) = parse_itimerval(new_value) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let now_ns = P::read_ns();
    let key = (ctx.process.pid.0, which);
    let old_timer = with_interval_timers(|timers| timers.get(&key).copied());
    if old_value_ptr != 0 {
        let old_value = itimer_to_layout(old_timer, now_ns);
        if let Err(errno) =
            bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, old_value_ptr, old_value)
        {
            return SyscallResult::error_from(errno);
        }
    }

    let timer = IntervalTimer {
        deadline_ns: if value_ns == 0 {
            0
        } else {
            now_ns.saturating_add(value_ns)
        },
        interval_ns,
    };
    with_interval_timers(|timers| {
        if timer.deadline_ns == 0 && timer.interval_ns == 0 {
            timers.remove(&key);
        } else {
            timers.insert(key, timer);
        }
    });
    if timer.deadline_ns != 0 {
        P::set_deadline_ns(timer.deadline_ns);
    }
    SyscallResult::Return(0)
}

pub fn poll_due_itimers<P: TimeIf>(process: &Cap<ProcessIdentity>) -> Option<u64> {
    let pid = process.pid.0;
    let now_ns = P::read_ns();
    let mut to_deliver = [None; 3];
    let mut deliver_len = 0usize;

    let next_deadline = with_interval_timers(|timers| {
        let mut next_deadline: Option<u64> = None;
        for ((timer_pid, which), timer) in timers.iter_mut() {
            if *timer_pid != pid || timer.deadline_ns == 0 {
                continue;
            }

            if timer.deadline_ns <= now_ns {
                if let Some(signal) = signal_for_itimer(*which) {
                    if signal_unblocked_on_any_thread(process, signal)
                        && deliver_len < to_deliver.len()
                    {
                        to_deliver[deliver_len] = Some(signal);
                        deliver_len += 1;
                    }
                }

                if timer.interval_ns == 0 {
                    timer.deadline_ns = 0;
                } else {
                    timer.deadline_ns = now_ns.saturating_add(timer.interval_ns);
                }
            }

            if timer.deadline_ns != 0 {
                next_deadline = Some(match next_deadline {
                    Some(existing) => existing.min(timer.deadline_ns),
                    None => timer.deadline_ns,
                });
            }
        }
        next_deadline
    });

    for signal in to_deliver.into_iter().flatten().take(deliver_len) {
        let _ = tx_subsystems::signal::deliver_posix_signal(
            tx_subsystems::signal::SignalTarget::Process(process.clone()),
            signal,
        );
    }

    next_deadline
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

pub(super) async fn sleep_until_deadline<'a, P: TimeIf>(
    deadline_ns: u64,
    original_ns: u64,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;

    while <P as TimeIf>::read_ns() < deadline_ns {
        let remaining_ns = deadline_ns.saturating_sub(<P as TimeIf>::read_ns());
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mailbox_arc = script_ctx.mailbox().cloned();
        let timer_wheel_arc = script_ctx.timer_wheel().cloned();
        let delegate_registry_arc = script_ctx.delegate_registry().cloned();
        let op = NanosleepOp {
            nanos: original_ns.min(remaining_ns),
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
            Ok(()) => return SyscallResult::Return(0),
            Err(v3errno) => {
                let errno = Errno::from(v3errno);
                if errno == Errno::EINTR {
                    return SyscallResult::Error(EINTR_VALUE);
                }
                return SyscallResult::error_from(errno);
            }
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
    let rem_uaddr = args[1];
    let req_ns = match read_timespec_ns_checked(&ctx.aspace, req_uaddr) {
        Ok(ns) => ns,
        Err(result) => return result,
    };
    if req_ns == 0 {
        return SyscallResult::Return(0);
    }
    let deadline_ns = <P as TimeIf>::read_ns().saturating_add(req_ns);
    match sleep_until_deadline::<P>(deadline_ns, req_ns, ctx).await {
        SyscallResult::Error(errno) if errno == EINTR_VALUE => {
            let remaining_ns = deadline_ns.saturating_sub(<P as TimeIf>::read_ns());
            match write_remaining_timespec(&ctx.aspace, rem_uaddr, remaining_ns) {
                SyscallResult::Return(_) => SyscallResult::Error(EINTR_VALUE),
                other => other,
            }
        }
        other => other,
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
    let rem_uaddr = args[3];

    match clk_id {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_REALTIME_ALARM
        | CLOCK_TAI
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_BOOTTIME
        | CLOCK_BOOTTIME_ALARM => {}
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
            return SyscallResult::Error(EOPNOTSUPP_VALUE);
        }
        _ => return SyscallResult::Error(EINVAL_VALUE),
    }
    if (flags & !TIMER_ABSTIME) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let req_ns = match read_timespec_ns_checked(&ctx.aspace, req_uaddr) {
        Ok(ns) => ns,
        Err(result) => return result,
    };
    let platform_now = <P as TimeIf>::read_ns();
    let clock_now = match clk_id {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE | CLOCK_REALTIME_ALARM | CLOCK_TAI => {
            realtime_ns::<P>()
        }
        _ => platform_now,
    };
    let sleep_ns = if (flags & TIMER_ABSTIME) != 0 {
        if clock_now >= req_ns {
            return SyscallResult::Return(0);
        }
        req_ns.saturating_sub(clock_now)
    } else {
        req_ns
    };
    if sleep_ns > MAX_CLOCK_NANOSLEEP_NS {
        return SyscallResult::Error(EOPNOTSUPP_VALUE);
    }
    let deadline_ns = platform_now.saturating_add(sleep_ns);
    match sleep_until_deadline::<P>(deadline_ns, sleep_ns, ctx).await {
        SyscallResult::Error(errno) if errno == EINTR_VALUE => {
            let remaining_ns = deadline_ns.saturating_sub(<P as TimeIf>::read_ns());
            match write_remaining_timespec(&ctx.aspace, rem_uaddr, remaining_ns) {
                SyscallResult::Return(_) => SyscallResult::Error(EINTR_VALUE),
                other => other,
            }
        }
        other => other,
    }
}
