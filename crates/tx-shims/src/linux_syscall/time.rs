//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{Cap, SpinMutex};
use alloc::collections::{BTreeMap, BTreeSet};
use tx_services::time::{
    timekeeper_clock, ClockRead, DeadlineNs, DeadlineRegistrar, DeadlineRegistrarHandle,
    RealtimeControl, RealtimeSetPolicy, TimekeeperClock, TimekeeperIf, TimerGuard, TimerRole,
    TimerTarget,
};
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
pub(super) struct ItimervalLayout {
    pub(super) it_interval: TimevalLayout,
    pub(super) it_value: TimevalLayout,
}

// Cooperative SIGALRM delivery can land at a syscall boundary instead of
// inside the following blocking syscall. Keep one interrupt token so that wait
// paths still observe the signal as `-EINTR`.
static ITIMER_REAL_DELIVERED_INTERRUPTS: SpinMutex<BTreeSet<u32>> = SpinMutex::new(BTreeSet::new());

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

const CLOCK_GETRES_NS: i64 = 2_000_000;

pub(super) fn realtime_ns<P>() -> u64
where
    TimekeeperClock<P>: ClockRead,
{
    timekeeper_clock::<P>().realtime_now_ns()
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
/// BOOTTIME aliases) routes through the timekeeper monotonic clock. Unknown
/// clock ids return `-EINVAL`. Null `tp` returns `-EFAULT`.
pub(super) fn sys_clock_gettime<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
        | CLOCK_BOOTTIME_ALARM => timekeeper_clock::<P>().monotonic_now_ns(),
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
pub(super) fn sys_gettimeofday<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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

fn can_set_realtime(ctx: &SyscallCtx<'_>) -> bool {
    ctx.cred().euid.is_root()
}

fn set_realtime_from_syscall<P>(ns: u64, ctx: &SyscallCtx<'_>) -> SyscallResult
where
    TimekeeperClock<P>: RealtimeControl,
{
    let timer_registrar = ctx
        .timer_registrar
        .as_ref()
        .map(|registrar| registrar as &dyn DeadlineRegistrar);
    match timekeeper_clock::<P>().set_realtime_ns_with_timerfd_post(
        ns,
        RealtimeSetPolicy::BestEffort,
        timer_registrar,
        |mailbox, event| ctx.post_mailbox_ref_event(mailbox, event),
    ) {
        Ok(_) => SyscallResult::Return(0),
        Err(_) => SyscallResult::Error(EINVAL_VALUE),
    }
}

pub(super) fn sys_clock_settime<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: RealtimeControl,
{
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
    set_realtime_from_syscall::<P>(ns, ctx)
}

pub(super) fn sys_settimeofday<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: RealtimeControl,
{
    let tv_uaddr = args[0];
    if tv_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if !can_set_realtime(ctx) {
        return SyscallResult::Error(EPERM_VALUE);
    }
    let tv = match bootstrap_read_user::<TimevalLayout>(&ctx.aspace, tv_uaddr) {
        Ok(tv) => tv,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let Some(ns) = timeval_to_ns(tv) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    set_realtime_from_syscall::<P>(ns, ctx)
}

pub const ITIMER_REAL: u32 = 0;
const ITIMER_VIRTUAL: u32 = 1;
const ITIMER_PROF: u32 = 2;
const SIGALRM: u8 = 14;
const SIGVTALRM: u8 = 26;
const SIGPROF: u8 = 27;

#[derive(Default)]
struct IntervalTimer {
    deadline_ns: u64,
    interval_ns: u64,
    timer_guard: Option<TimerGuard>,
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

fn itimer_to_layout(timer: Option<&IntervalTimer>, now_ns: u64) -> ItimervalLayout {
    let deadline_ns = timer.map_or(0, |timer| timer.deadline_ns);
    let interval_ns = timer.map_or(0, |timer| timer.interval_ns);
    ItimervalLayout {
        it_interval: ns_to_timeval(interval_ns),
        it_value: ns_to_timeval(if deadline_ns == 0 {
            0
        } else {
            deadline_ns.saturating_sub(now_ns)
        }),
    }
}

fn register_syscall_timer_deadline(
    ctx: &SyscallCtx<'_>,
    deadline_ns: u64,
    role: TimerRole,
) -> Option<TimerGuard> {
    let registrar = ctx.timer_registrar.as_ref()?;
    let mailbox = ctx.mailbox.as_ref()?;
    registrar
        .register_deadline(
            DeadlineNs::new(deadline_ns),
            role,
            TimerTarget::SignalTarget {
                mailbox: alloc::sync::Arc::downgrade(mailbox),
            },
        )
        .ok()
}

fn register_task_timer_deadline(
    registrar: Option<&dyn DeadlineRegistrar>,
    mailbox: Option<&alloc::sync::Weak<tx_substrate::wake::TaskMailbox>>,
    deadline_ns: u64,
    role: TimerRole,
) -> Option<TimerGuard> {
    let registrar = registrar?;
    let mailbox = mailbox?;
    registrar
        .register_deadline(
            DeadlineNs::new(deadline_ns),
            role,
            TimerTarget::SignalTarget {
                mailbox: mailbox.clone(),
            },
        )
        .ok()
}

fn parse_itimerval(value: ItimervalLayout) -> Option<(u64, u64)> {
    let interval_ns = timeval_to_ns(value.it_interval)?;
    let value_ns = timeval_to_ns(value.it_value)?;
    Some((interval_ns, value_ns))
}

pub(super) fn sys_getitimer<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let which = args[0] as u32;
    let curr_value_ptr = args[1];
    if !valid_itimer(which) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if curr_value_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
    let value = with_interval_timers(|timers| {
        itimer_to_layout(timers.get(&(ctx.process.pid.0, which)), now_ns)
    });
    match bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, curr_value_ptr, value) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
}

pub(super) fn sys_setitimer<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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

    let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
    let key = (ctx.process.pid.0, which);
    let old_value = with_interval_timers(|timers| itimer_to_layout(timers.get(&key), now_ns));
    if old_value_ptr != 0 {
        if let Err(errno) =
            bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, old_value_ptr, old_value)
        {
            return SyscallResult::error_from(errno);
        }
    }

    let deadline_ns = if value_ns == 0 {
        0
    } else {
        now_ns.saturating_add(value_ns)
    };
    let timer_guard = (deadline_ns != 0)
        .then(|| register_syscall_timer_deadline(ctx, deadline_ns, TimerRole::ItimerReal));
    let timer = IntervalTimer {
        deadline_ns,
        interval_ns,
        timer_guard: timer_guard.flatten(),
    };
    with_interval_timers(|timers| {
        if timer.deadline_ns == 0 && timer.interval_ns == 0 {
            timers.remove(&key);
        } else {
            timers.insert(key, timer);
        }
    });
    SyscallResult::Return(0)
}

pub fn poll_due_itimers_with_post<P, F>(
    process: &Cap<ProcessIdentity>,
    timer_registrar: Option<&dyn DeadlineRegistrar>,
    timer_mailbox: Option<alloc::sync::Weak<tx_substrate::wake::TaskMailbox>>,
    mut post: F,
) -> Option<u64>
where
    TimekeeperClock<P>: ClockRead,
    F: FnMut(alloc::sync::Weak<tx_substrate::wake::TaskMailbox>, tx_substrate::wake::MailboxEvent),
{
    let pid = process.pid.0;
    let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
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
                    // A blocked signal is still generated and remains pending;
                    // the mask controls delivery, not timer expiry.  Dropping
                    // it here made timeout(1)'s watchdog disappear when expiry
                    // raced with a temporary sigsuspend mask.
                    if deliver_len < to_deliver.len() {
                        to_deliver[deliver_len] = Some(signal);
                        deliver_len += 1;
                    }
                }

                if timer.interval_ns == 0 {
                    timer.timer_guard = None;
                    timer.deadline_ns = 0;
                } else {
                    timer.timer_guard = None;
                    timer.deadline_ns = now_ns.saturating_add(timer.interval_ns);
                    timer.timer_guard = register_task_timer_deadline(
                        timer_registrar,
                        timer_mailbox.as_ref(),
                        timer.deadline_ns,
                        TimerRole::ItimerReal,
                    );
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
        let _ = tx_subsystems::signal::deliver_posix_signal_with_post(
            tx_subsystems::signal::SignalTarget::Process(process.clone()),
            signal,
            &mut post,
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
pub(super) fn sys_times<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let buf_uaddr = args[0];
    let ns = timekeeper_clock::<P>().monotonic_now_ns();
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

pub(super) async fn sleep_until_deadline<'a, P>(
    deadline_ns: u64,
    original_ns: u64,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;

    if timekeeper_clock::<P>().monotonic_now_ns() < deadline_ns {
        let remaining_ns = deadline_ns.saturating_sub(timekeeper_clock::<P>().monotonic_now_ns());
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mailbox_arc = script_ctx.mailbox().cloned();
        let timer_registrar_handle = script_ctx.timer_registrar().cloned();
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
            timer_registrar_handle.as_ref(),
        )
        .await
        {
            Ok(()) => return SyscallResult::Return(0),
            Err(v3errno) => {
                let errno = v3errno;
                if errno == Errno::EINTR {
                    return SyscallResult::Error(EINTR_VALUE);
                }
                return SyscallResult::error_from(errno);
            }
        }
    }
    SyscallResult::Return(0)
}

pub(super) fn itimer_real_deadline_ns(pid: u32) -> Option<u64> {
    with_interval_timers(|timers| {
        timers
            .get(&(pid, ITIMER_REAL))
            .and_then(|timer| (timer.deadline_ns != 0).then_some(timer.deadline_ns))
    })
}

pub(super) fn consume_itimer_real_delivered_interrupt(pid: u32) -> bool {
    ITIMER_REAL_DELIVERED_INTERRUPTS.lock().remove(&pid)
}

#[cfg(test)]
pub(super) fn reset_itimer_registry_for_test() {
    with_interval_timers(|timers| timers.clear());
    ITIMER_REAL_DELIVERED_INTERRUPTS.lock().clear();
}
/// `nanosleep(req, rem)`. Linux RV64 generic ABI
/// `__NR_nanosleep = 101`.
///
/// Validates `*req` (returns `-EINVAL` on negative fields or
/// `tv_nsec >= 1_000_000_000`); zero-duration requests short-circuit
/// immediately to `Return(0)`. Non-zero durations park the task on the
/// shared timer registry until the absolute deadline passes, then
/// return `0`. Null `req` returns `-EFAULT`.
///
/// `rem` (args[1]) is ignored — no EINTR path wired yet.
#[cfg_attr(not(test), allow(clippy::extra_unused_type_parameters))]
#[cfg_attr(test, allow(clippy::extra_unused_type_parameters))]
pub(super) async fn sys_nanosleep<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let req_uaddr = args[0];
    let rem_uaddr = args[1];
    let req_ns = match read_timespec_ns_checked(&ctx.aspace, req_uaddr) {
        Ok(ns) => ns,
        Err(result) => return result,
    };
    if req_ns == 0 {
        return SyscallResult::Return(0);
    }
    let deadline_ns = timekeeper_clock::<P>()
        .monotonic_now_ns()
        .saturating_add(req_ns);
    match sleep_until_deadline::<P>(deadline_ns, req_ns, ctx).await {
        SyscallResult::Error(errno) if errno == EINTR_VALUE => {
            let remaining_ns =
                deadline_ns.saturating_sub(timekeeper_clock::<P>().monotonic_now_ns());
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
pub(super) async fn sys_clock_nanosleep<'a, P>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
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
    let platform_now = timekeeper_clock::<P>().monotonic_now_ns();
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
    // No upper bound on the relative sleep: `nanosleep` (the syscall musl
    // routes `usleep`/`sleep` through) parks for arbitrarily long deadlines
    // via the reactor, and `clock_nanosleep` must match it. glibc's
    // `usleep`/`sleep` lower onto `clock_nanosleep`, so an artificial cap
    // here made e.g. the LTP shell watchdog's `usleep(300s)` return
    // EOPNOTSUPP immediately — firing a bogus "Test timed out" and
    // live-locking the test's `/proc/<pid>/stat` setup spin.
    let deadline_ns = platform_now.saturating_add(sleep_ns);
    match sleep_until_deadline::<P>(deadline_ns, sleep_ns, ctx).await {
        SyscallResult::Error(errno) if errno == EINTR_VALUE => {
            let remaining_ns =
                deadline_ns.saturating_sub(timekeeper_clock::<P>().monotonic_now_ns());
            match write_remaining_timespec(&ctx.aspace, rem_uaddr, remaining_ns) {
                SyscallResult::Return(_) => SyscallResult::Error(EINTR_VALUE),
                other => other,
            }
        }
        other => other,
    }
}

/// Fire `pid`'s ITIMER_REAL at expiry: re-arm the deadline and deliver SIGALRM
/// to the process when it has a handler installed.
///
/// The socket recv wait calls this the moment the deadline is reached. Two
/// effects matter:
///
///  1. **Re-arm.** A periodic timer (`it_interval > 0`) advances to `now +
///     interval` (not `deadline + interval`, which under slow TCG could stay in
///     the past and make a blocked recv spin returning EINTR); a one-shot timer
///     is disarmed. Without this the passed deadline made every recv wake
///     immediately — the busy loop seen on `ping01`.
///  2. **SIGALRM.** Delivered only when a handler is installed through the
///     injected-post signal helper. busybox `ping` sends each subsequent probe
///     from its SIGALRM handler, so without delivery it only ever sent one
///     packet; handler-less alarm users keep EINTR-only semantics and are not
///     terminated.
pub(super) fn fire_itimer_real_with_post<P>(ctx: &SyscallCtx<'_>)
where
    TimekeeperClock<P>: ClockRead,
{
    fire_itimer_real_inner::<P, _, _>(
        ctx.process.pid.0,
        |process, sig| {
            tx_subsystems::signal::deliver_signal_if_handler_with_post(
                process,
                sig,
                |mailbox, event| {
                    ctx.post_mailbox_event(mailbox, event);
                },
            )
        },
        |deadline_ns| register_syscall_timer_deadline(ctx, deadline_ns, TimerRole::ItimerReal),
    );
}

fn fire_itimer_real_inner<P, F, R>(pid: u32, deliver: F, mut register_rearm: R)
where
    TimekeeperClock<P>: ClockRead,
    F: FnOnce(&Cap<ProcessIdentity>, tx_subsystems::signal::Signum) -> bool,
    R: FnMut(u64) -> Option<TimerGuard>,
{
    let Some(process) = tx_subsystems::process::execution::process_by_pid(
        tx_subsystems::process::structure::Pid(pid),
    ) else {
        return;
    };
    let Some(sigalrm) = tx_subsystems::signal::Signum::new(SIGALRM_SIGNUM) else {
        return;
    };
    // No handler → preserve the existing EINTR-only contract: do not deliver and
    // do not re-arm (the caller still returns EINTR for this expiry).
    if !deliver(&process, sigalrm) {
        return;
    }
    let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
    let key = (pid, ITIMER_REAL);
    let interval_ns =
        with_interval_timers(|timers| timers.get(&key).map(|timer| timer.interval_ns));
    match interval_ns {
        Some(interval) if interval > 0 => with_interval_timers(|timers| {
            if let Some(timer) = timers.get_mut(&key) {
                timer.timer_guard = None;
                timer.deadline_ns = now_ns.saturating_add(interval);
                timer.timer_guard = register_rearm(timer.deadline_ns);
            }
        }),
        Some(_) => with_interval_timers(|timers| {
            timers.remove(&key);
        }),
        None => {}
    }
}

const SIGALRM_SIGNUM: u8 = 14;

/// Generic syscall-boundary check for an expired ITIMER_REAL.
///
/// The socket recv path ([`super::socket`]) fires the timer at its own wait
/// deadline, which covers alarm-bounded *blocking* recvs (e.g. busybox `ping`).
/// But a process spinning in a tight non-blocking loop never reaches a socket
/// wait: netperf's `UDP_STREAM`/`TCP_STREAM` send burst arms `alarm(N)` (→
/// `setitimer(ITIMER_REAL)`) and then loops on `send`/`sendto` until its
/// `SIGALRM` handler sets `times_up`. With delivery gated to the socket-wait
/// path that handler never runs and the test sends forever (observed as a hang
/// right after the test banner).
///
/// Linux delivers a fired ITIMER_REAL on the next return-to-userspace from ANY
/// syscall. The dispatcher calls this on every syscall boundary to reproduce
/// that: when the calling process's ITIMER_REAL deadline has passed, post
/// SIGALRM through the injected-post helper so the AST checkpoint delivers the
/// handler on this syscall's return. The common case — no armed ITIMER_REAL —
/// is a single `BTreeMap` lookup that returns `None`.
pub(super) fn poll_itimer_real_on_syscall_boundary<P>(ctx: &SyscallCtx<'_>)
where
    TimekeeperClock<P>: ClockRead,
{
    let pid = ctx.process.pid.0;
    let Some(deadline_ns) = itimer_real_deadline_ns(pid) else {
        return;
    };
    if timekeeper_clock::<P>().monotonic_now_ns() >= deadline_ns {
        fire_itimer_real_with_post::<P>(ctx);
    }
}
