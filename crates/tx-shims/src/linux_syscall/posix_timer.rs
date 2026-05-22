//! Minimal POSIX timer syscall surface.
//!
//! This is intentionally smaller than Linux's full signal-delivery
//! timer engine: it tracks timer ids, validates user ABI, and reports
//! timer state. Signal delivery can be layered on top once the basic
//! syscall contract is stable.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU32, Ordering};

use tx_subsystems::timerfd::{ItimerSpec, ITIMERSPEC_BYTES};

use tx_subsystems::process::ProcessIdentity;

use super::numbers::{
    CLOCK_BOOTTIME, CLOCK_BOOTTIME_ALARM, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID,
    CLOCK_REALTIME, CLOCK_REALTIME_ALARM, CLOCK_TAI, CLOCK_THREAD_CPUTIME_ID, NR_TIMER_CREATE,
    NR_TIMER_DELETE, NR_TIMER_GETOVERRUN, NR_TIMER_GETTIME, NR_TIMER_SETTIME, TIMER_ABSTIME,
};
use super::time::realtime_ns;
use super::{
    bootstrap_read_user, bootstrap_write_user, SyscallCtx, SyscallResult, EFAULT_VALUE,
    EINVAL_VALUE,
};
use crate::adapter::step_engine::{Cap, SpinMutex};

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD_ID: i32 = 4;
const SIGALRM: u8 = 14;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SigeventPrefixLayout {
    sigval: u64,
    sigev_signo: i32,
    sigev_notify: i32,
}

#[derive(Clone, Copy, Debug, Default)]
struct PosixTimer {
    clockid: u32,
    deadline_ns: u64,
    interval_ns: u64,
    overrun: i32,
    signal: Option<tx_subsystems::signal::Signum>,
}

static NEXT_TIMER_ID: AtomicU32 = AtomicU32::new(1);
static POSIX_TIMERS: SpinMutex<Option<BTreeMap<(u32, u32), PosixTimer>>> = SpinMutex::new(None);

fn with_timer_map<R>(f: impl FnOnce(&mut BTreeMap<(u32, u32), PosixTimer>) -> R) -> R {
    let mut guard = POSIX_TIMERS.lock();
    let map = guard.get_or_insert_with(BTreeMap::new);
    f(map)
}

fn valid_clock(clockid: u32) -> bool {
    matches!(
        clockid,
        CLOCK_REALTIME
            | CLOCK_MONOTONIC
            | CLOCK_PROCESS_CPUTIME_ID
            | CLOCK_THREAD_CPUTIME_ID
            | CLOCK_BOOTTIME
            | CLOCK_REALTIME_ALARM
            | CLOCK_BOOTTIME_ALARM
            | CLOCK_TAI
    )
}

fn remaining_ns(timer: PosixTimer, now_ns: u64) -> u64 {
    if timer.deadline_ns == 0 {
        0
    } else {
        timer.deadline_ns.saturating_sub(now_ns)
    }
}

fn current_clock_ns<P: super::TimeIf>(clockid: u32, now_ns: u64) -> u64 {
    if matches!(clockid, CLOCK_REALTIME | CLOCK_REALTIME_ALARM | CLOCK_TAI) {
        realtime_ns::<P>()
    } else {
        now_ns
    }
}

fn timer_to_spec(timer: PosixTimer, now_ns: u64) -> ItimerSpec {
    ItimerSpec {
        it_interval_ns: timer.interval_ns,
        it_value_ns: remaining_ns(timer, now_ns),
    }
}

fn timer_key(ctx: &SyscallCtx<'_>, timerid: u32) -> (u32, u32) {
    (ctx.process.pid.0, timerid)
}

pub fn poll_due_posix_timers<P: super::TimeIf>(process: &Cap<ProcessIdentity>) -> Option<u64> {
    let pid = process.pid.0;
    let now_ns = P::read_ns();
    let mut to_deliver = [None; 8];
    let mut deliver_len = 0usize;
    let next_deadline = with_timer_map(|timers| {
        let mut next_deadline: Option<u64> = None;
        for ((timer_pid, _), timer) in timers.iter_mut() {
            if *timer_pid != pid || timer.deadline_ns == 0 {
                continue;
            }

            if timer.deadline_ns <= now_ns {
                if let Some(signal) = timer.signal {
                    if deliver_len < to_deliver.len() {
                        to_deliver[deliver_len] = Some(signal);
                        deliver_len += 1;
                    }
                }

                if timer.interval_ns == 0 || timer.overrun == i32::MAX {
                    timer.deadline_ns = 0;
                } else {
                    let elapsed = now_ns.saturating_sub(timer.deadline_ns);
                    let periods = elapsed / timer.interval_ns + 1;
                    timer.overrun = periods.saturating_sub(1).min(i32::MAX as u64) as i32;
                    timer.deadline_ns = timer
                        .deadline_ns
                        .saturating_add(periods.saturating_mul(timer.interval_ns));
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

fn timer_signal_from_sigevent(
    sevp_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> Result<Option<tx_subsystems::signal::Signum>, SyscallResult> {
    if sevp_ptr == 0 {
        return Ok(tx_subsystems::signal::Signum::new(SIGALRM));
    }
    let sev: SigeventPrefixLayout = match bootstrap_read_user(&ctx.aspace, sevp_ptr) {
        Ok(sev) => sev,
        Err(errno) => return Err(SyscallResult::error_from(errno)),
    };
    match sev.sigev_notify {
        SIGEV_SIGNAL | SIGEV_THREAD_ID => {
            let Some(signum) = u8::try_from(sev.sigev_signo)
                .ok()
                .and_then(tx_subsystems::signal::Signum::new)
            else {
                return Err(SyscallResult::Error(EINVAL_VALUE));
            };
            Ok(Some(signum))
        }
        SIGEV_NONE => Ok(None),
        _ => Err(SyscallResult::Error(EINVAL_VALUE)),
    }
}

pub(super) fn sys_timer_create(
    clockid: u32,
    sevp_ptr: u64,
    timerid_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if !valid_clock(clockid) {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if timerid_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let signal = match timer_signal_from_sigevent(sevp_ptr, ctx) {
        Ok(signal) => signal,
        Err(result) => return result,
    };

    let timerid = NEXT_TIMER_ID.fetch_add(1, Ordering::Relaxed);
    if timerid == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    with_timer_map(|timers| {
        timers.insert(
            timer_key(ctx, timerid),
            PosixTimer {
                clockid,
                signal,
                ..PosixTimer::default()
            },
        );
    });

    match bootstrap_write_user::<i32>(&ctx.aspace, timerid_ptr, timerid as i32) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => {
            with_timer_map(|timers| {
                timers.remove(&timer_key(ctx, timerid));
            });
            SyscallResult::error_from(errno)
        }
    }
}

pub(super) fn sys_timer_delete(timerid: u32, ctx: &SyscallCtx<'_>) -> SyscallResult {
    let removed = with_timer_map(|timers| timers.remove(&timer_key(ctx, timerid)).is_some());
    if removed {
        SyscallResult::Return(0)
    } else {
        SyscallResult::Error(EINVAL_VALUE)
    }
}

pub(super) fn sys_timer_getoverrun(timerid: u32, ctx: &SyscallCtx<'_>) -> SyscallResult {
    let overrun = with_timer_map(|timers| timers.get(&timer_key(ctx, timerid)).map(|t| t.overrun));
    match overrun {
        Some(v) => SyscallResult::Return(v as i64),
        None => SyscallResult::Error(EINVAL_VALUE),
    }
}

pub(super) fn sys_timer_gettime<P: super::TimeIf>(
    timerid: u32,
    curr_value_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if curr_value_ptr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let now_ns = P::read_ns();
    let spec = with_timer_map(|timers| {
        timers
            .get(&timer_key(ctx, timerid))
            .copied()
            .map(|timer| timer_to_spec(timer, now_ns))
    });
    let Some(spec) = spec else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    match bootstrap_write_user::<[u8; ITIMERSPEC_BYTES]>(
        &ctx.aspace,
        curr_value_ptr,
        spec.to_bytes(),
    ) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
}

pub(super) fn sys_timer_settime<P: super::TimeIf>(
    timerid: u32,
    flags: u32,
    new_value_ptr: u64,
    old_value_ptr: u64,
    ctx: &SyscallCtx<'_>,
) -> SyscallResult {
    if flags & !TIMER_ABSTIME != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if new_value_ptr == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let new_bytes = match bootstrap_read_user::<[u8; ITIMERSPEC_BYTES]>(&ctx.aspace, new_value_ptr)
    {
        Ok(bytes) => bytes,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let Some(new_value) = ItimerSpec::try_from_bytes(&new_bytes) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let now_ns = P::read_ns();
    let abstime = (flags & TIMER_ABSTIME) != 0;
    let update = with_timer_map(|timers| {
        let timer = timers.get_mut(&timer_key(ctx, timerid))?;
        let old_spec = timer_to_spec(*timer, now_ns);
        let clock_now_ns = current_clock_ns::<P>(timer.clockid, now_ns);
        timer.interval_ns = new_value.it_interval_ns;
        let expired_periodic_abstime = abstime
            && new_value.it_value_ns != 0
            && new_value.it_interval_ns != 0
            && new_value.it_value_ns <= clock_now_ns;
        timer.deadline_ns = if new_value.it_value_ns == 0 {
            0
        } else if expired_periodic_abstime {
            now_ns
        } else if abstime {
            now_ns.saturating_add(new_value.it_value_ns.saturating_sub(clock_now_ns))
        } else {
            now_ns.saturating_add(new_value.it_value_ns)
        };
        timer.overrun = if expired_periodic_abstime {
            i32::MAX
        } else {
            0
        };
        Some((old_spec, *timer))
    });
    let Some((old_spec, timer_after)) = update else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    if old_value_ptr != 0 {
        match bootstrap_write_user::<[u8; ITIMERSPEC_BYTES]>(
            &ctx.aspace,
            old_value_ptr,
            old_spec.to_bytes(),
        ) {
            Ok(()) => {}
            Err(errno) => return SyscallResult::error_from(errno),
        }
    }
    if timer_after.deadline_ns != 0 {
        P::set_deadline_ns(timer_after.deadline_ns);
    }
    SyscallResult::Return(0)
}

/// Silence unused-import warnings in narrow build slices.
const _: fn() = || {
    let _ = NR_TIMER_CREATE;
    let _ = NR_TIMER_GETTIME;
    let _ = NR_TIMER_GETOVERRUN;
    let _ = NR_TIMER_SETTIME;
    let _ = NR_TIMER_DELETE;
};
