// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID, CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID,
    NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP, NR_GETTIMEOFDAY, NR_NANOSLEEP, NR_TIMES, TIMER_ABSTIME,
    TIMES_NS_PER_TICK,
};

const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_NOSYS: i32 = 38;

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

/// Null `tv` returns `-EFAULT`.
#[test]
fn dispatch_gettimeofday_null_buffer_returns_neg_efault() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_GETTIMEOFDAY, [0, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
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

/// `nanosleep((1, 0), _)` returns `-ENOSYS` in Slice 4 — real
/// non-zero durations are deferred to the timer-channel slice.
/// Pinned here so a future slice that lands real-duration
/// nanosleep updates this test alongside the implementation.
#[test]
fn dispatch_nanosleep_nonzero_duration_returns_neg_enosys() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let req_ts = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let req_uaddr = &req_ts as *const TestTimespec as u64;

    let req = SyscallRequest::new(NR_NANOSLEEP, [req_uaddr, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
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
