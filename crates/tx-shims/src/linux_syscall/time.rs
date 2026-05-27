//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use alloc::collections::{BTreeMap, BTreeSet};
use core::mem::{offset_of, size_of};

use crate::adapter::step_engine::{Cap, SpinMutex};
use tx_subsystems::process::ProcessIdentity;

use tx_hal::{UserSaFlagsAbi, UserSigInfoAbi, UserSignalMaskAbi, UserTrapContext};
use tx_subsystems::signal::step_kill_process;
use tx_subsystems::thread_runtime::ThreadIdentity;

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
pub(super) struct ItimerspecLayout {
    pub(super) it_interval: TimespecLayout,
    pub(super) it_value: TimespecLayout,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TmsLayout {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct TimexLayout {
    pub(super) modes: u32,
    _pad0: u32,
    pub(super) offset: i64,
    pub(super) freq: i64,
    pub(super) maxerror: i64,
    pub(super) esterror: i64,
    pub(super) status: i32,
    _pad1: u32,
    pub(super) constant: i64,
    pub(super) precision: i64,
    pub(super) tolerance: i64,
    pub(super) time: TimexTimevalLayout,
    pub(super) tick: i64,
    pub(super) ppsfreq: i64,
    pub(super) jitter: i64,
    pub(super) shift: i32,
    _pad2: u32,
    pub(super) stabil: i64,
    pub(super) jitcnt: i64,
    pub(super) calcnt: i64,
    pub(super) errcnt: i64,
    pub(super) stbcnt: i64,
    pub(super) tai: i32,
    _reserved: [i32; 11],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct TimexTimevalLayout {
    pub(super) tv_sec: i64,
    pub(super) tv_usec: i64,
}

pub(super) mod layout_descriptors {
    use core::mem::{align_of, offset_of, size_of};

    use super::{
        ItimerspecLayout, ItimervalLayout, TimespecLayout, TimevalLayout, TimexTimevalLayout,
    };
    pub(super) use super::{TimexLayout, TmsLayout};
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

    impl KernelToUserLayout for ItimerspecLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "ItimerspecLayout",
            musl_header: "time.h",
            musl_type: "struct itimerspec",
            size: size_of::<ItimerspecLayout>(),
            align: align_of::<ItimerspecLayout>(),
            fields: &[
                KernelUserField {
                    rust: "it_interval",
                    musl: "it_interval",
                    offset: offset_of!(ItimerspecLayout, it_interval),
                },
                KernelUserField {
                    rust: "it_value",
                    musl: "it_value",
                    offset: offset_of!(ItimerspecLayout, it_value),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const ITIMERSPEC_LAYOUT: KernelUserLayout =
        <ItimerspecLayout as KernelToUserLayout>::LAYOUT;

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

    impl KernelToUserLayout for TimexLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "TimexLayout",
            musl_header: "sys/timex.h",
            musl_type: "struct timex",
            size: size_of::<TimexLayout>(),
            align: align_of::<TimexLayout>(),
            fields: &[
                KernelUserField {
                    rust: "modes",
                    musl: "modes",
                    offset: offset_of!(TimexLayout, modes),
                },
                KernelUserField {
                    rust: "offset",
                    musl: "offset",
                    offset: offset_of!(TimexLayout, offset),
                },
                KernelUserField {
                    rust: "freq",
                    musl: "freq",
                    offset: offset_of!(TimexLayout, freq),
                },
                KernelUserField {
                    rust: "maxerror",
                    musl: "maxerror",
                    offset: offset_of!(TimexLayout, maxerror),
                },
                KernelUserField {
                    rust: "esterror",
                    musl: "esterror",
                    offset: offset_of!(TimexLayout, esterror),
                },
                KernelUserField {
                    rust: "status",
                    musl: "status",
                    offset: offset_of!(TimexLayout, status),
                },
                KernelUserField {
                    rust: "constant",
                    musl: "constant",
                    offset: offset_of!(TimexLayout, constant),
                },
                KernelUserField {
                    rust: "precision",
                    musl: "precision",
                    offset: offset_of!(TimexLayout, precision),
                },
                KernelUserField {
                    rust: "tolerance",
                    musl: "tolerance",
                    offset: offset_of!(TimexLayout, tolerance),
                },
                KernelUserField {
                    rust: "time",
                    musl: "time",
                    offset: offset_of!(TimexLayout, time),
                },
                KernelUserField {
                    rust: "tick",
                    musl: "tick",
                    offset: offset_of!(TimexLayout, tick),
                },
                KernelUserField {
                    rust: "tai",
                    musl: "tai",
                    offset: offset_of!(TimexLayout, tai),
                },
            ],
        };
    }
    pub(in crate::linux_syscall) const TIMEX_LAYOUT: KernelUserLayout =
        <TimexLayout as KernelToUserLayout>::LAYOUT;

    impl KernelToUserLayout for TimexTimevalLayout {
        const LAYOUT: KernelUserLayout = KernelUserLayout {
            rust_type: "TimexTimevalLayout",
            musl_header: "sys/timex.h",
            musl_type: "struct timeval",
            size: size_of::<TimexTimevalLayout>(),
            align: align_of::<TimexTimevalLayout>(),
            fields: &[
                KernelUserField {
                    rust: "tv_sec",
                    musl: "tv_sec",
                    offset: offset_of!(TimexTimevalLayout, tv_sec),
                },
                KernelUserField {
                    rust: "tv_usec",
                    musl: "tv_usec",
                    offset: offset_of!(TimexTimevalLayout, tv_usec),
                },
            ],
        };
    }
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
    tx_subsystems::timekeeping::clock_now_ns::<P>(tx_subsystems::timekeeping::ClockId::Realtime)
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

fn timespec_to_ns(ts: TimespecLayout) -> Option<u64> {
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return None;
    }
    Some((ts.tv_sec as u64).saturating_mul(1_000_000_000) + ts.tv_nsec as u64)
}

fn ns_to_itimerval(spec: tx_subsystems::timekeeping::IntervalTimerSpec) -> ItimervalLayout {
    ItimervalLayout {
        it_interval: ns_to_timeval(spec.interval_ns),
        it_value: ns_to_timeval(spec.value_ns),
    }
}

fn read_itimerval_at(
    aspace: &AddressSpace,
    uaddr: u64,
) -> Result<tx_subsystems::timekeeping::IntervalTimerSpec, i32> {
    if uaddr == 0 {
        return Ok(tx_subsystems::timekeeping::IntervalTimerSpec::default());
    }
    let layout: ItimervalLayout = match bootstrap_read_user::<ItimervalLayout>(aspace, uaddr) {
        Ok(v) => v,
        Err(errno) => return Err(errno_to_i32(errno)),
    };
    let Some(interval_ns) = timeval_to_ns(layout.it_interval) else {
        return Err(EINVAL_VALUE);
    };
    let Some(value_ns) = timeval_to_ns(layout.it_value) else {
        return Err(EINVAL_VALUE);
    };
    Ok(tx_subsystems::timekeeping::IntervalTimerSpec {
        interval_ns,
        value_ns,
    })
}

fn ns_to_itimerspec(spec: tx_subsystems::timekeeping::PosixTimerSnapshot) -> ItimerspecLayout {
    ItimerspecLayout {
        it_interval: ns_to_timespec(spec.interval_ns),
        it_value: ns_to_timespec(spec.value_ns),
    }
}

fn read_itimerspec_at(
    aspace: &AddressSpace,
    uaddr: u64,
) -> Result<tx_subsystems::timekeeping::PosixTimerSnapshot, i32> {
    if uaddr == 0 {
        return Err(EFAULT_VALUE);
    }
    let layout: ItimerspecLayout = match bootstrap_read_user::<ItimerspecLayout>(aspace, uaddr) {
        Ok(v) => v,
        Err(errno) => return Err(errno_to_i32(errno)),
    };
    let Some(interval_ns) = timespec_to_ns(layout.it_interval) else {
        return Err(EINVAL_VALUE);
    };
    let Some(value_ns) = timespec_to_ns(layout.it_value) else {
        return Err(EINVAL_VALUE);
    };
    Ok(tx_subsystems::timekeeping::PosixTimerSnapshot {
        interval_ns,
        value_ns,
    })
}

fn can_set_realtime(ctx: &SyscallCtx<'_>) -> bool {
    ctx.cred().euid.is_root()
}

fn decode_clock_id(clk_id: u32) -> Option<tx_subsystems::timekeeping::ClockId> {
    match clk_id {
        CLOCK_REALTIME => Some(tx_subsystems::timekeeping::ClockId::Realtime),
        CLOCK_MONOTONIC => Some(tx_subsystems::timekeeping::ClockId::Monotonic),
        CLOCK_PROCESS_CPUTIME_ID => Some(tx_subsystems::timekeeping::ClockId::ProcessCpuTime),
        CLOCK_THREAD_CPUTIME_ID => Some(tx_subsystems::timekeeping::ClockId::ThreadCpuTime),
        CLOCK_MONOTONIC_RAW => Some(tx_subsystems::timekeeping::ClockId::MonotonicRaw),
        CLOCK_REALTIME_COARSE => Some(tx_subsystems::timekeeping::ClockId::RealtimeCoarse),
        CLOCK_MONOTONIC_COARSE => Some(tx_subsystems::timekeeping::ClockId::MonotonicCoarse),
        CLOCK_BOOTTIME => Some(tx_subsystems::timekeeping::ClockId::Boottime),
        CLOCK_TAI => Some(tx_subsystems::timekeeping::ClockId::Tai),
        _ => None,
    }
}

fn timex_to_service(layout: TimexLayout) -> tx_subsystems::timekeeping::TimexState {
    tx_subsystems::timekeeping::TimexState {
        modes: layout.modes,
        offset: layout.offset,
        freq: layout.freq,
        maxerror: layout.maxerror,
        esterror: layout.esterror,
        status: layout.status as u32,
        constant: layout.constant,
        precision: layout.precision,
        tolerance: layout.tolerance,
        time_sec: layout.time.tv_sec,
        time_subsec: layout.time.tv_usec,
        tick: layout.tick,
        ppsfreq: layout.ppsfreq,
        jitter: layout.jitter,
        shift: layout.shift,
        stabil: layout.stabil,
        jitcnt: layout.jitcnt,
        calcnt: layout.calcnt,
        errcnt: layout.errcnt,
        stbcnt: layout.stbcnt,
        tai: layout.tai,
    }
}

fn service_to_timex(state: tx_subsystems::timekeeping::TimexState) -> TimexLayout {
    TimexLayout {
        modes: state.modes,
        _pad0: 0,
        offset: state.offset,
        freq: state.freq,
        maxerror: state.maxerror,
        esterror: state.esterror,
        status: state.status as i32,
        _pad1: 0,
        constant: state.constant,
        precision: state.precision,
        tolerance: state.tolerance,
        time: TimexTimevalLayout {
            tv_sec: state.time_sec,
            tv_usec: state.time_subsec,
        },
        tick: state.tick,
        ppsfreq: state.ppsfreq,
        jitter: state.jitter,
        shift: state.shift,
        _pad2: 0,
        stabil: state.stabil,
        jitcnt: state.jitcnt,
        calcnt: state.calcnt,
        errcnt: state.errcnt,
        stbcnt: state.stbcnt,
        tai: state.tai,
        _reserved: [0; 11],
    }
}

fn timekeeping_error_to_syscall(
    error: tx_subsystems::timekeeping::TimekeepingError,
) -> SyscallResult {
    match error {
        tx_subsystems::timekeeping::TimekeepingError::Invalid
        | tx_subsystems::timekeeping::TimekeepingError::Range => SyscallResult::Error(EINVAL_VALUE),
        tx_subsystems::timekeeping::TimekeepingError::Permission => {
            SyscallResult::Error(EPERM_VALUE)
        }
        tx_subsystems::timekeeping::TimekeepingError::Unsupported => {
            SyscallResult::error_from(Errno::EOPNOTSUPP)
        }
    }
}

const SIGEV_SIGNAL_VALUE: i32 = 0;
const SIGEV_NONE_VALUE: i32 = 1;
const SIGEV_THREAD_VALUE: i32 = 2;
const SIGEV_THREAD_ID_VALUE: i32 = 4;
const SIGALRM_VALUE: u32 = 14;
const SI_TIMER_VALUE: i32 = -2;

fn posix_timer_clock(clockid: u32) -> Option<tx_subsystems::timekeeping::PosixTimerClock> {
    match clockid {
        CLOCK_REALTIME => Some(tx_subsystems::timekeeping::PosixTimerClock::Realtime),
        CLOCK_MONOTONIC => Some(tx_subsystems::timekeeping::PosixTimerClock::Monotonic),
        _ => None,
    }
}

fn posix_timer_deadline_mono_ns<P: TimeIf>(
    clock: tx_subsystems::timekeeping::PosixTimerClock,
    abstime: bool,
    value_ns: u64,
    now_mono_ns: u64,
) -> u64 {
    if value_ns == 0 {
        return 0;
    }
    if !abstime {
        return now_mono_ns.saturating_add(value_ns);
    }
    match clock {
        tx_subsystems::timekeeping::PosixTimerClock::Realtime => {
            tx_subsystems::timekeeping::monotonic_deadline_from_realtime_ns(value_ns)
        }
        tx_subsystems::timekeeping::PosixTimerClock::Monotonic => value_ns,
    }
}

fn read_posix_timer_notify(
    ctx: &SyscallCtx<'_>,
    sevp_uaddr: u64,
) -> Result<tx_subsystems::timekeeping::PosixTimerNotify, i32> {
    if sevp_uaddr == 0 {
        return Ok(tx_subsystems::timekeeping::PosixTimerNotify::Signal {
            signum: SIGALRM_VALUE,
            sigval: 0,
        });
    }
    let sev: crate::linux_syscall::ipc::SigeventPrefixLayout =
        match bootstrap_read_user(&ctx.aspace, sevp_uaddr) {
            Ok(v) => v,
            Err(errno) => return Err(errno_to_i32(errno)),
        };
    match sev.sigev_notify {
        SIGEV_SIGNAL_VALUE => {
            let Some(signum) = u8::try_from(sev.sigev_signo)
                .ok()
                .and_then(tx_subsystems::signal::Signum::new)
            else {
                return Err(EINVAL_VALUE);
            };
            Ok(tx_subsystems::timekeeping::PosixTimerNotify::Signal {
                signum: signum.raw() as u32,
                sigval: sev.sigval,
            })
        }
        SIGEV_NONE_VALUE => Ok(tx_subsystems::timekeeping::PosixTimerNotify::None),
        SIGEV_THREAD_VALUE | SIGEV_THREAD_ID_VALUE => Err(EINVAL_VALUE),
        _ => Err(EINVAL_VALUE),
    }
}

pub(super) fn poll_expired_process_timers<P: TimeIf>(ctx: &SyscallCtx<'_>) {
    let now_mono_ns = tx_subsystems::timekeeping::clock_now_ns::<P>(
        tx_subsystems::timekeeping::ClockId::Monotonic,
    );
    poll_expired_process_timers_at(ctx, now_mono_ns);
}

pub(super) fn poll_expired_process_timers_at(ctx: &SyscallCtx<'_>, now_mono_ns: u64) {
    let Some(expired) = ctx.process.consume_expired_timers(now_mono_ns) else {
        return;
    };
    for signal in expired {
        let Some(signum) = u8::try_from(signal.signum)
            .ok()
            .and_then(tx_subsystems::signal::Signum::new)
        else {
            continue;
        };
        let info = tx_subsystems::signal::SigInfo {
            si_signo: signum.raw() as u32,
            si_code: SI_TIMER_VALUE,
            si_pid: 0,
            si_uid: 0,
            si_value: signal.sigval,
        };
        let _ = tx_subsystems::signal::step_kill_process(&ctx.process, signum, Some(info));
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
    let Some(clock) = decode_clock_id(clk_id) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    let ns = tx_subsystems::timekeeping::clock_now_ns::<P>(clock);
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
    match tx_subsystems::timekeeping::set_realtime_ns::<P>(ns) {
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
        | CLOCK_BOOTTIME
        | CLOCK_TAI => {}
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
    match tx_subsystems::timekeeping::set_realtime_ns::<P>(ns) {
        Ok(_) => SyscallResult::Return(0),
        Err(_) => SyscallResult::Error(EINVAL_VALUE),
    }
}

pub(super) fn sys_getitimer<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let which = args[0] as u32;
    let value_uaddr = args[1];
    if value_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let now_ns = tx_subsystems::timekeeping::clock_now_ns::<P>(
        tx_subsystems::timekeeping::ClockId::Monotonic,
    );
    let spec = match which {
        ITIMER_REAL => ctx.process.itimer_real(now_ns).unwrap_or_default(),
        ITIMER_VIRTUAL | ITIMER_PROF => return SyscallResult::error_from(Errno::EOPNOTSUPP),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    if let Err(errno) =
        bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, value_uaddr, ns_to_itimerval(spec))
    {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_setitimer<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let which = args[0] as u32;
    let value_uaddr = args[1];
    let old_uaddr = args[2];
    let new_value = match read_itimerval_at(&ctx.aspace, value_uaddr) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let now_ns = tx_subsystems::timekeeping::clock_now_ns::<P>(
        tx_subsystems::timekeeping::ClockId::Monotonic,
    );
    let old = match which {
        ITIMER_REAL => ctx
            .process
            .set_itimer_real(now_ns, new_value)
            .unwrap_or_default(),
        ITIMER_VIRTUAL | ITIMER_PROF => return SyscallResult::error_from(Errno::EOPNOTSUPP),
        _ => return SyscallResult::Error(EINVAL_VALUE),
    };
    if old_uaddr != 0 {
        if let Err(errno) =
            bootstrap_write_user::<ItimervalLayout>(&ctx.aspace, old_uaddr, ns_to_itimerval(old))
        {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_timer_create<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let clockid = args[0] as u32;
    let sevp_uaddr = args[1];
    let timerid_uaddr = args[2];
    if timerid_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let Some(clock) = posix_timer_clock(clockid) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    let notify = match read_posix_timer_notify(ctx, sevp_uaddr) {
        Ok(notify) => notify,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let timer_id = match ctx.process.create_posix_timer(clock, notify) {
        Some(Ok(id)) => id,
        Some(Err(error)) => return timekeeping_error_to_syscall(error),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if let Err(errno) = bootstrap_write_user::<i32>(&ctx.aspace, timerid_uaddr, timer_id as i32) {
        let _ = ctx.process.delete_posix_timer(timer_id);
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_timer_settime<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let timer_id = args[0] as u32;
    let flags = args[1] as u32;
    let new_uaddr = args[2];
    let old_uaddr = args[3];
    if flags & !TIMER_ABSTIME != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let new_value = match read_itimerspec_at(&ctx.aspace, new_uaddr) {
        Ok(value) => value,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let now_mono_ns = tx_subsystems::timekeeping::clock_now_ns::<P>(
        tx_subsystems::timekeeping::ClockId::Monotonic,
    );
    let clock = match ctx.process.posix_timer_clock(timer_id) {
        Some(Ok(clock)) => clock,
        Some(Err(error)) => return timekeeping_error_to_syscall(error),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let deadline_mono_ns = posix_timer_deadline_mono_ns::<P>(
        clock,
        flags & TIMER_ABSTIME != 0,
        new_value.value_ns,
        now_mono_ns,
    );
    let old = match ctx.process.set_posix_timer(
        timer_id,
        deadline_mono_ns,
        new_value.interval_ns,
        new_value.value_ns,
        now_mono_ns,
    ) {
        Some(Ok(old)) => old,
        Some(Err(error)) => return timekeeping_error_to_syscall(error),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if old_uaddr != 0 {
        if let Err(errno) =
            bootstrap_write_user::<ItimerspecLayout>(&ctx.aspace, old_uaddr, ns_to_itimerspec(old))
        {
            return SyscallResult::error_from(errno);
        }
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_timer_gettime<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let timer_id = args[0] as u32;
    let curr_uaddr = args[1];
    if curr_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let now_mono_ns = tx_subsystems::timekeeping::clock_now_ns::<P>(
        tx_subsystems::timekeeping::ClockId::Monotonic,
    );
    let current = match ctx.process.get_posix_timer(timer_id, now_mono_ns) {
        Some(Ok(current)) => current,
        Some(Err(error)) => return timekeeping_error_to_syscall(error),
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if let Err(errno) =
        bootstrap_write_user::<ItimerspecLayout>(&ctx.aspace, curr_uaddr, ns_to_itimerspec(current))
    {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(0)
}

pub(super) fn sys_timer_getoverrun<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let timer_id = args[0] as u32;
    match ctx.process.get_posix_timer_overrun(timer_id) {
        Some(Ok(overrun)) => SyscallResult::Return(overrun as i64),
        Some(Err(error)) => timekeeping_error_to_syscall(error),
        None => SyscallResult::Error(EINVAL_VALUE),
    }
}

pub(super) fn sys_timer_delete<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let timer_id = args[0] as u32;
    match ctx.process.delete_posix_timer(timer_id) {
        Some(Ok(())) => SyscallResult::Return(0),
        Some(Err(error)) => timekeeping_error_to_syscall(error),
        None => SyscallResult::Error(EINVAL_VALUE),
    }
}

pub(super) fn sys_adjtimex<'a, P: TimeIf>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    sys_clock_adjtime_inner::<P>(CLOCK_REALTIME, args[0], ctx)
}

pub(super) fn sys_clock_adjtime<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    sys_clock_adjtime_inner::<P>(args[0] as u32, args[1], ctx)
}

fn sys_clock_adjtime_inner<'a, P: TimeIf>(
    clk_id: u32,
    tx_uaddr: u64,
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let Some(clock) = decode_clock_id(clk_id) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    if tx_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let layout = match bootstrap_read_user::<TimexLayout>(&ctx.aspace, tx_uaddr) {
        Ok(layout) => layout,
        Err(errno) => return SyscallResult::error_from(errno),
    };
    let mut state = timex_to_service(layout);
    let result =
        tx_subsystems::timekeeping::adjtimex::<P>(clock, can_set_realtime(ctx), &mut state);
    let ret = match result {
        Ok(ret) => ret,
        Err(error) => return timekeeping_error_to_syscall(error),
    };
    if let Err(errno) =
        bootstrap_write_user::<TimexLayout>(&ctx.aspace, tx_uaddr, service_to_timex(state))
    {
        return SyscallResult::error_from(errno);
    }
    SyscallResult::Return(ret as i64)
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

    if <P as TimeIf>::read_ns() < deadline_ns {
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

fn take_due_itimer_real<P: TimeIf>(pid: u32) -> bool {
    with_interval_timers(|timers| {
        let key = (pid, ITIMER_REAL);
        let Some(timer) = timers.get_mut(&key) else {
            return false;
        };
        let now_ns = P::read_ns();
        if timer.deadline_ns == 0 || timer.deadline_ns > now_ns {
            return false;
        }
        if timer.interval_ns == 0 {
            timers.remove(&key);
        } else {
            timer.deadline_ns = now_ns.saturating_add(timer.interval_ns);
        }
        true
    })
}

pub fn maybe_deliver_itimer_signal<P: TimeIf>(
    mut ctx: UserTrapContext,
    process: &Cap<ProcessIdentity>,
    thread: &Cap<ThreadIdentity>,
    aspace: &AddressSpace,
) -> UserTrapContext {
    let Some(thread_payload) = thread.payload_cap() else {
        return ctx;
    };
    if thread_payload.has_saved_signal_context() {
        return ctx;
    }
    if !take_due_itimer_real::<P>(process.pid.0) {
        return ctx;
    }
    ITIMER_REAL_DELIVERED_INTERRUPTS
        .lock()
        .insert(process.pid.0);

    let Some(sig) = Signum::new(SIGALRM_RAW) else {
        return ctx;
    };
    let Some(SigDisposition::Handler(handler)) = process.sig_disposition(sig) else {
        let _ = step_kill_process(process, sig, None);
        return ctx;
    };

    let Some(frame_addr) = ctx.regs[2]
        .checked_sub(size_of::<CompatSignalFrame>())
        .map(|addr| align_down(addr, RV64_SIGFRAME_ALIGN))
    else {
        return ctx;
    };

    let siginfo_addr = frame_addr + offset_of!(CompatSignalFrame, siginfo);
    let ucontext_addr = frame_addr + offset_of!(CompatSignalFrame, user_context);
    let trampoline_pc = frame_addr + offset_of!(CompatSignalFrame, trampoline);
    let return_pc = sigaction_restorer(process.pid.0, sig).unwrap_or(trampoline_pc);
    let frame = CompatSignalFrame {
        magic: RV64_SIGFRAME_MAGIC,
        version: RV64_SIGFRAME_VERSION,
        frame_size: size_of::<CompatSignalFrame>() as u32,
        sig_no: u32::from(SIGALRM_RAW),
        _reserved0: 0,
        flags: UserSaFlagsAbi::EMPTY.bits,
        siginfo: UserSigInfoAbi::ZERO,
        saved_mask: UserSignalMaskAbi {
            bits: thread_payload.signal_mask().raw_bits(),
        },
        user_context: ctx,
        trampoline: RV64_SIGRETURN_TRAMPOLINE,
    };

    if bootstrap_write_user::<CompatSignalFrame>(aspace, frame_addr as u64, frame).is_err() {
        return ctx;
    }
    if return_pc == trampoline_pc {
        let Ok(trampoline_range) = UserRange::containing_page(UserVirtAddr::new(trampoline_pc))
        else {
            return ctx;
        };
        if aspace
            .pmap()
            .protect_range(trampoline_range, Prot::new(true, true, true))
            .is_err()
        {
            return ctx;
        }
    }

    ctx.pc = handler;
    ctx.regs[1] = return_pc;
    ctx.regs[2] = frame_addr;
    ctx.regs[10] = usize::from(SIGALRM_RAW);
    ctx.regs[11] = siginfo_addr;
    ctx.regs[12] = ucontext_addr;
    thread_payload.store_saved_signal_context(Some(frame.user_context));
    ctx
}

pub(super) fn read_compat_signal_frame(
    aspace: &AddressSpace,
    frame_addr: u64,
) -> Result<CompatSignalFrame, i32> {
    let frame =
        bootstrap_read_user::<CompatSignalFrame>(aspace, frame_addr).map_err(errno_to_i32)?;
    if frame.magic != RV64_SIGFRAME_MAGIC
        || frame.version != RV64_SIGFRAME_VERSION
        || frame.frame_size as usize != size_of::<CompatSignalFrame>()
        || frame.trampoline != RV64_SIGRETURN_TRAMPOLINE
    {
        return Err(EINVAL_VALUE);
    }
    Ok(frame)
}

const fn align_down(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    value & !(align - 1)
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
                tx_subsystems::timekeeping::monotonic_deadline_from_realtime_ns(req_ns);
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
    let (wake_deadline_ns, interrupted_by_process_timer) =
        nanosleep_wake_deadline(ctx, deadline_ns);
    use tx_scripts::drive;
    use tx_substrate::step::DriveMode;
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = NanosleepOp {
        nanos: req_ns,
        deadline_ns: wake_deadline_ns,
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
        Ok(()) if interrupted_by_process_timer => {
            let now_ns = <P as TimeIf>::read_ns().max(wake_deadline_ns);
            poll_expired_process_timers_at(ctx, now_ns);
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
        }
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

fn nanosleep_wake_deadline(ctx: &SyscallCtx<'_>, sleep_deadline_ns: u64) -> (u64, bool) {
    if ctx.mailbox.is_none() || ctx.timer_wheel.is_none() {
        return (sleep_deadline_ns, false);
    }
    match ctx.process.next_process_timer_deadline_ns() {
        Some(timer_deadline_ns) if timer_deadline_ns <= sleep_deadline_ns => {
            (timer_deadline_ns, true)
        }
        _ => (sleep_deadline_ns, false),
    }
}
