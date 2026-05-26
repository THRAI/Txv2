// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID,
    ITIMER_PROF, ITIMER_REAL, NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP, NR_CLOCK_SETTIME, NR_GETITIMER,
    NR_GETTIMEOFDAY, NR_NANOSLEEP, NR_SETITIMER, NR_SETTIMEOFDAY, NR_TIMES, TIMER_ABSTIME,
    TIMES_NS_PER_TICK,
};
use tx_subsystems::cred::{step_setresuid, Uid};

const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_PERM: i32 = 1;
const OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC: i64 = 1_779_473_960;

/// Mirror of `TimespecLayout` for test-side decoding. The
/// production layout is private to `mod.rs`, so the tests
/// reconstruct the same shape via `read_volatile` against a
/// stack-allocated buffer.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestItimerval {
    it_interval: TestTimeval,
    it_value: TestTimeval,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTms {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

fn time_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

/// `clock_gettime(CLOCK_MONOTONIC, ts)` succeeds and writes a
/// `(tv_sec, tv_nsec)` pair derived from the platform clock.
/// `ShimsTestPmap::read_ns()` starts at 5_000_000_000 ns
/// (= 5 seconds) and increments per call, so the observed
/// timespec must satisfy `tv_sec >= 5` and `tv_nsec` is in
/// `[0, 1_000_000_000)`.
#[test]
fn dispatch_clock_gettime_monotonic_writes_timespec_to_user() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut ts = TestTimespec::default();
    let ts_uaddr = &mut ts as *mut TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_CLOCK_GETTIME,
        [CLOCK_MONOTONIC as u64, ts_uaddr, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(ts.tv_sec >= 5, "tv_sec should reflect the test clock base");
    assert!(
        (0..1_000_000_000).contains(&ts.tv_nsec),
        "tv_nsec must be in [0, 1e9): got {}",
        ts.tv_nsec,
    );
}

/// CPU-time clock ids alias to the platform monotonic in v1 and
/// must succeed.
#[test]
fn dispatch_clock_gettime_cputime_aliases_to_monotonic() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut ts = TestTimespec::default();
    let ts_uaddr = &mut ts as *mut TestTimespec as u64;
    for clk in [
        CLOCK_REALTIME,
        CLOCK_PROCESS_CPUTIME_ID,
        CLOCK_THREAD_CPUTIME_ID,
    ] {
        let req = SyscallRequest::new(NR_CLOCK_GETTIME, [clk as u64, ts_uaddr, 0, 0, 0, 0]);
        let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
        assert_eq!(result, SyscallResult::Return(0), "clk_id {clk}");
    }
}

/// Unrecognised clock ids return `-EINVAL`.
#[test]
fn dispatch_clock_gettime_invalid_clock_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut ts = TestTimespec::default();
    let ts_uaddr = &mut ts as *mut TestTimespec as u64;

    let req = SyscallRequest::new(NR_CLOCK_GETTIME, [99, ts_uaddr, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// Null `tp` returns `-EFAULT` (without dereferencing the null
/// pointer).
#[test]
fn dispatch_clock_gettime_null_buffer_returns_neg_efault() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_CLOCK_GETTIME, [CLOCK_MONOTONIC as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

/// `gettimeofday(tv, _)` writes a `(tv_sec, tv_usec)` pair from
/// the platform clock.
#[test]
fn dispatch_gettimeofday_writes_timeval_to_user() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tv = TestTimeval::default();
    let tv_uaddr = &mut tv as *mut TestTimeval as u64;

    let req = SyscallRequest::new(NR_GETTIMEOFDAY, [tv_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(tv.tv_sec >= 5);
    assert!(
        (0..1_000_000).contains(&tv.tv_usec),
        "tv_usec must be in [0, 1e6): got {}",
        tv.tv_usec,
    );
}

#[test]
fn dispatch_gettimeofday_realtime_is_not_before_oscomp_image_timestamps() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tv = TestTimeval::default();
    let tv_uaddr = &mut tv as *mut TestTimeval as u64;

    let req = SyscallRequest::new(NR_GETTIMEOFDAY, [tv_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));

    assert_eq!(result, SyscallResult::Return(0));
    assert!(
        tv.tv_sec >= OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC,
        "CLOCK_REALTIME seconds {} must not predate OSComp image mtimes {}",
        tv.tv_sec,
        OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC,
    );
}

/// Null `tv` returns `-EFAULT`.
#[test]
fn dispatch_gettimeofday_null_buffer_returns_neg_efault() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETTIMEOFDAY, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

#[test]
fn dispatch_clock_settime_updates_realtime_without_moving_monotonic() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let new_rt = TestTimespec {
        tv_sec: 1_800_000_000,
        tv_nsec: 123_456_789,
    };

    let before_mono = {
        let mut ts = TestTimespec::default();
        let result = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_CLOCK_GETTIME,
                [
                    CLOCK_MONOTONIC as u64,
                    &mut ts as *mut TestTimespec as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(result, SyscallResult::Return(0));
        ts.tv_sec
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec)
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &new_rt as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut rt = TestTimespec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [
                CLOCK_REALTIME as u64,
                &mut rt as *mut TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(rt.tv_sec, new_rt.tv_sec);
    assert!(rt.tv_nsec >= new_rt.tv_nsec);

    let mut after = TestTimespec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [
                CLOCK_MONOTONIC as u64,
                &mut after as *mut TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    let after_mono = after
        .tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(after.tv_nsec);
    assert!(after_mono >= before_mono);
    assert!(
        after_mono - before_mono < 1_000_000,
        "clock_settime must not jump CLOCK_MONOTONIC: before={before_mono} after={after_mono}"
    );
}

#[test]
fn dispatch_settimeofday_updates_gettimeofday_realtime() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let tv = TestTimeval {
        tv_sec: 1_800_000_010,
        tv_usec: 654_321,
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETTIMEOFDAY,
            [&tv as *const TestTimeval as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut out = TestTimeval::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETTIMEOFDAY,
            [&mut out as *mut TestTimeval as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out.tv_sec, tv.tv_sec);
    assert!(out.tv_usec >= tv.tv_usec);
}

#[test]
fn dispatch_clock_settime_rejects_non_realtime_and_unprivileged_callers() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap.clone(), thread.clone());
    let ts = TestTimespec {
        tv_sec: 1_800_000_000,
        tv_nsec: 0,
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_SETTIME,
            [
                CLOCK_MONOTONIC as u64,
                &ts as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));

    let target = Uid(1000);
    assert!(matches!(
        step_setresuid(&proc_cap, Some(target), Some(target), Some(target)),
        tx_subsystems::cred::CredChange::Replaced { .. }
    ));
    let unpriv_ctx = make_ctx(proc_cap, thread);
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &ts as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &unpriv_ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_PERM));
}

/// `times(buf)` returns the monotonic tick count and writes
/// `tms_utime = ticks`. The other three fields stay at their
/// pre-existing values (the syscall arm zeros them, which is
/// observable since the buffer is initialised to a non-zero
/// sentinel below).
#[test]
fn dispatch_times_returns_tick_count_and_writes_buffer() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    // Initialise the buffer to a sentinel so we can assert the
    // syscall arm overwrites all four fields.
    let mut tms = TestTms {
        tms_utime: 0xdead_beef,
        tms_stime: 0xdead_beef,
        tms_cutime: 0xdead_beef,
        tms_cstime: 0xdead_beef,
    };
    let buf_uaddr = &mut tms as *mut TestTms as u64;

    let req = SyscallRequest::new(NR_TIMES, [buf_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let returned_ticks = match result {
        SyscallResult::Return(t) => t,
        other => panic!("expected Return, got {other:?}"),
    };
    assert!(returned_ticks > 0);
    assert_eq!(
        tms.tms_utime, returned_ticks,
        "tms_utime should match the returned tick count",
    );
    assert_eq!(tms.tms_stime, 0);
    assert_eq!(tms.tms_cutime, 0);
    assert_eq!(tms.tms_cstime, 0);

    // Sanity: returned_ticks * NS_PER_TICK should be in the same
    // ballpark as the test clock base (5 seconds = 500 ticks at
    // 100Hz) — bounded loosely so other tests advancing the
    // counter do not break this one.
    let approx_ns = (returned_ticks as u64) * TIMES_NS_PER_TICK;
    assert!(
        approx_ns >= 5_000_000_000,
        "ticks {returned_ticks} * {TIMES_NS_PER_TICK}ns = {approx_ns}ns < 5e9ns",
    );
}

/// `times(NULL)` returns the tick count without writing anywhere.
/// Linux semantics: a null `buf` is permitted; only the return
/// value matters in that case (LTP `times02`).
#[test]
fn dispatch_times_with_null_buffer_returns_tick_count() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_TIMES, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let ticks = match result {
        SyscallResult::Return(t) => t,
        other => panic!("expected Return, got {other:?}"),
    };
    assert!(ticks > 0);
}

/// `setitimer(ITIMER_REAL, new, old)` arms the process real timer and
/// reports the previous remaining value. This is the ABI path musl's
/// `alarm(2)` uses on Linux RV64.
#[test]
fn dispatch_setitimer_real_reports_previous_remaining_timer() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let first = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: TestTimeval {
            tv_sec: 10,
            tv_usec: 0,
        },
    };
    let second = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: TestTimeval {
            tv_sec: 1,
            tv_usec: 0,
        },
    };
    let mut old = TestItimerval::default();

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &first as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &second as *const TestItimerval as u64,
                &mut old as *mut TestItimerval as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(old.it_interval.tv_sec, 0);
    assert_eq!(old.it_interval.tv_usec, 0);
    assert_eq!(old.it_value.tv_sec, 9);
    assert!(old.it_value.tv_usec <= 999_999);
}

/// `getitimer(ITIMER_REAL, value)` returns the currently armed process
/// real timer in Linux's `struct itimerval` layout.
#[test]
fn dispatch_getitimer_real_returns_current_timer() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let new = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 2,
            tv_usec: 500_000,
        },
        it_value: TestTimeval {
            tv_sec: 3,
            tv_usec: 250_000,
        },
    };
    let mut current = TestItimerval::default();

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &new as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETITIMER,
            [
                ITIMER_REAL as u64,
                &mut current as *mut TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(current.it_interval, new.it_interval);
    assert_eq!(current.it_value.tv_sec, 3);
    assert!(current.it_value.tv_usec <= 250_000);
}

#[test]
fn dispatch_setitimer_rejects_cpu_timers_and_invalid_timeval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let invalid = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 0,
            tv_usec: 1_000_000,
        },
        it_value: TestTimeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &invalid as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));

    let valid = TestItimerval::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_PROF as u64,
                &valid as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `nanosleep((0, 0), _)` short-circuits to `Return(0)` per the
/// Linux semantics — a zero-duration sleep is a no-op.
#[test]
fn dispatch_nanosleep_zero_duration_returns_immediately() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `nanosleep((1, 0), _)` returns `0` — real-duration sleeps now
/// complete immediately in the test context (no reactor installed,
/// so the timer future is skipped and we return success).
#[test]
fn dispatch_nanosleep_nonzero_duration_returns_zero_without_reactor() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `nanosleep((-1, 0), _)` returns `-EINVAL` — negative tv_sec is
/// rejected by Linux.
#[test]
fn dispatch_nanosleep_negative_tv_sec_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: -1,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `nanosleep(NULL, _)` returns `-EFAULT`.
#[test]
fn dispatch_nanosleep_null_buffer_returns_neg_efault() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_NANOSLEEP, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

/// `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, past_ts, _)`
/// short-circuits to `Return(0)` because the absolute deadline
/// is already in the past.
#[test]
fn dispatch_clock_nanosleep_abstime_past_deadline_returns_immediately() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    // tv_sec = 1 (== 1e9 ns), well below the test clock base of
    // 5_000_000_000 ns — the deadline is already past.
    let req_ts = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_CLOCK_NANOSLEEP,
        [
            CLOCK_MONOTONIC as u64,
            TIMER_ABSTIME as u64,
            req_uaddr,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `clock_nanosleep` with an unknown clock id returns `-EINVAL`.
#[test]
fn dispatch_clock_nanosleep_invalid_clock_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_CLOCK_NANOSLEEP, [99, 0, req_uaddr, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `clock_nanosleep` with an unknown flag bit returns `-EINVAL`.
#[test]
fn dispatch_clock_nanosleep_unknown_flag_returns_neg_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(
        NR_CLOCK_NANOSLEEP,
        [
            CLOCK_MONOTONIC as u64,
            0x2, // unknown flag bit
            req_uaddr,
            0,
            0,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}
