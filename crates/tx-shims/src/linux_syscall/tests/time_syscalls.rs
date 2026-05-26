// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    ADJ_OFFSET, ADJ_SETOFFSET, ADJ_TICK, ADJ_TIMECONST, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID,
    CLOCK_REALTIME, CLOCK_THREAD_CPUTIME_ID, ITIMER_PROF, ITIMER_REAL, NR_ADJTIMEX,
    NR_CLOCK_ADJTIME, NR_CLOCK_GETTIME, NR_CLOCK_NANOSLEEP, NR_CLOCK_SETTIME, NR_GETITIMER,
    NR_GETPID, NR_GETTIMEOFDAY, NR_NANOSLEEP, NR_SETITIMER, NR_SETTIMEOFDAY, NR_TIMER_CREATE,
    NR_TIMER_DELETE, NR_TIMER_GETOVERRUN, NR_TIMER_GETTIME, NR_TIMER_SETTIME, NR_TIMES,
    TIMER_ABSTIME, TIMES_NS_PER_TICK,
};
use tx_subsystems::cred::{step_setresuid, Uid};
use tx_subsystems::signal::{step_sigaction, SigDisposition, Signum};

const E_INVAL: i32 = 22;
const E_FAULT: i32 = 14;
const E_INTR: i32 = 4;
const E_OPNOTSUPP: i32 = 95;
const E_PERM: i32 = 1;
const OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC: i64 = 1_779_473_960;
const SIGALRM_RAW: u8 = 14;

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
struct TestItimerspec {
    it_interval: TestTimespec,
    it_value: TestTimespec,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTms {
    tms_utime: i64,
    tms_stime: i64,
    tms_cutime: i64,
    tms_cstime: i64,
}

#[test]
fn dispatch_getitimer_real_initially_disarmed() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut out = TestItimerval::default();

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETITIMER,
            [
                ITIMER_REAL as u64,
                &mut out as *mut TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out, TestItimerval::default());
}

#[test]
fn dispatch_setitimer_real_sets_timer_and_returns_old_value() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let new_timer = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 1,
            tv_usec: 250_000,
        },
        it_value: TestTimeval {
            tv_sec: 3,
            tv_usec: 500_000,
        },
    };
    let mut old_timer = TestItimerval {
        it_interval: TestTimeval {
            tv_sec: 99,
            tv_usec: 99,
        },
        it_value: TestTimeval {
            tv_sec: 99,
            tv_usec: 99,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &new_timer as *const TestItimerval as u64,
                &mut old_timer as *mut TestItimerval as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(old_timer, TestItimerval::default());

    let mut current = TestItimerval::default();
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
    assert_eq!(current.it_interval, new_timer.it_interval);
    let remaining_us = current.it_value.tv_sec * 1_000_000 + current.it_value.tv_usec;
    assert!(
        (1..=3_500_000).contains(&remaining_us),
        "remaining value should be armed and bounded, got {remaining_us}us",
    );
}

#[test]
fn dispatch_setitimer_rejects_cpu_timer_until_cpu_accounting_lands() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let new_timer = TestItimerval::default();

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_PROF as u64,
                &new_timer as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(95));
}

#[test]
fn dispatch_setitimer_rejects_invalid_timeval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let bad = TestItimerval {
        it_interval: TestTimeval::default(),
        it_value: TestTimeval {
            tv_sec: 0,
            tv_usec: 1_000_000,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &bad as *const TestItimerval as u64,
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

fn test_itimerspec_ns(spec: TestItimerspec) -> (u64, u64) {
    let interval = (spec.it_interval.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(spec.it_interval.tv_nsec as u64);
    let value = (spec.it_value.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(spec.it_value.tv_nsec as u64);
    (interval, value)
}

fn create_posix_timer(ctx: &SyscallCtx<'_>, clockid: u32) -> u32 {
    let mut timer_id: i32 = -1;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_CREATE,
            [clockid as u64, 0, &mut timer_id as *mut i32 as u64, 0, 0, 0],
        ),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(timer_id >= 0, "timer_create should write a nonnegative id");
    timer_id as u32
}

#[test]
fn dispatch_timer_create_default_set_get_delete_round_trips_state() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC);

    let new_timer = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
        it_value: TestTimespec {
            tv_sec: 4,
            tv_nsec: 250_000_000,
        },
    };
    let mut old_timer = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 99,
            tv_nsec: 99,
        },
        it_value: TestTimespec {
            tv_sec: 99,
            tv_nsec: 99,
        },
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_SETTIME,
            [
                timer_id as u64,
                0,
                &new_timer as *const TestItimerspec as u64,
                &mut old_timer as *mut TestItimerspec as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(old_timer, TestItimerspec::default());

    let mut current = TestItimerspec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_GETTIME,
            [
                timer_id as u64,
                &mut current as *mut TestItimerspec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    let (interval_ns, value_ns) = test_itimerspec_ns(current);
    assert_eq!(interval_ns, 1_000_000_000);
    assert!(
        (1..=4_250_000_000).contains(&value_ns),
        "timer_gettime should report bounded remaining time, got {value_ns}ns",
    );

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMER_GETOVERRUN, [timer_id as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMER_DELETE, [timer_id as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_GETTIME,
            [
                timer_id as u64,
                &mut current as *mut TestItimerspec as u64,
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

#[test]
fn dispatch_timer_create_rejects_unsupported_clock_and_bad_pointer() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut timer_id: i32 = -1;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_CREATE,
            [
                CLOCK_PROCESS_CPUTIME_ID as u64,
                0,
                &mut timer_id as *mut i32 as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMER_CREATE, [CLOCK_MONOTONIC as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

#[test]
fn dispatch_timer_settime_rejects_unknown_flags_and_invalid_nsec() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let timer_id = create_posix_timer(&ctx, CLOCK_REALTIME);
    let invalid_nsec = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 1_000_000_000,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_SETTIME,
            [
                timer_id as u64,
                0,
                &invalid_nsec as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));

    let valid = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_SETTIME,
            [
                timer_id as u64,
                0x8000_0000,
                &valid as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_poll_delivers_expired_itimer_real_as_sigalrm() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap.clone(), thread.clone());
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));
    let new_timer = TestItimerval {
        it_interval: TestTimeval::default(),
        it_value: TestTimeval {
            tv_sec: 0,
            tv_usec: 1,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETITIMER,
            [
                ITIMER_REAL as u64,
                &new_timer as *const TestItimerval as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let payload = thread.payload_cap().expect("live thread");
    for _ in 0..2_000 {
        let result = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_GETPID, [0, 0, 0, 0, 0, 0]),
            &ctx,
        ));
        assert!(matches!(result, SyscallResult::Return(_)));
        if payload.pending().is_pending(sigalrm) {
            break;
        }
    }
    assert!(
        payload.pending().is_pending(sigalrm),
        "expired ITIMER_REAL should post SIGALRM through the existing signal path"
    );
}

#[test]
fn dispatch_poll_delivers_expired_posix_timer_signal() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap.clone(), thread.clone());
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));
    let timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC);
    let new_timer = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 1,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_SETTIME,
            [
                timer_id as u64,
                0,
                &new_timer as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETPID, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert!(matches!(result, SyscallResult::Return(_)));

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "expired default POSIX timer should post SIGALRM"
    );
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct TestTimexTimeval {
    tv_sec: i64,
    tv_usec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TestTimex {
    modes: u32,
    _pad0: u32,
    offset: i64,
    freq: i64,
    maxerror: i64,
    esterror: i64,
    status: i32,
    _pad1: u32,
    constant: i64,
    precision: i64,
    tolerance: i64,
    time: TestTimexTimeval,
    tick: i64,
    ppsfreq: i64,
    jitter: i64,
    shift: i32,
    _pad2: u32,
    stabil: i64,
    jitcnt: i64,
    calcnt: i64,
    errcnt: i64,
    stbcnt: i64,
    tai: i32,
    _reserved: [i32; 11],
}

impl Default for TestTimex {
    fn default() -> Self {
        Self {
            modes: 0,
            _pad0: 0,
            offset: 0,
            freq: 0,
            maxerror: 0,
            esterror: 0,
            status: 0,
            _pad1: 0,
            constant: 0,
            precision: 0,
            tolerance: 0,
            time: TestTimexTimeval::default(),
            tick: 0,
            ppsfreq: 0,
            jitter: 0,
            shift: 0,
            _pad2: 0,
            stabil: 0,
            jitcnt: 0,
            calcnt: 0,
            errcnt: 0,
            stbcnt: 0,
            tai: 0,
            _reserved: [0; 11],
        }
    }
}

fn time_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

#[test]
fn dispatch_adjtimex_readonly_fills_timex_and_returns_time_ok() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tx = TestTimex {
        modes: 0,
        ..TestTimex::default()
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_ADJTIMEX,
            [&mut tx as *mut TestTimex as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(tx.modes, 0);
    assert!(tx.time.tv_sec >= OSCOMP_IMAGE_TIMESTAMP_FLOOR_SEC);
    assert_eq!(tx.precision, 1);
    assert_eq!(tx.tolerance, 0);
}

#[test]
fn dispatch_adjtimex_unsupported_slew_mode_returns_eopnotsupp() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tx = TestTimex {
        modes: ADJ_OFFSET,
        offset: 42,
        ..TestTimex::default()
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_ADJTIMEX,
            [&mut tx as *mut TestTimex as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(95));
}

#[test]
fn dispatch_adjtimex_setoffset_steps_realtime() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut before = TestTimespec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [
                CLOCK_REALTIME as u64,
                &mut before as *mut TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut tx = TestTimex {
        modes: ADJ_SETOFFSET,
        time: TestTimexTimeval {
            tv_sec: 2,
            tv_usec: 250_000,
        },
        ..TestTimex::default()
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_ADJTIMEX,
            [&mut tx as *mut TestTimex as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut after = TestTimespec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_GETTIME,
            [
                CLOCK_REALTIME as u64,
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
    let before_ns = before.tv_sec * 1_000_000_000 + before.tv_nsec;
    let after_ns = after.tv_sec * 1_000_000_000 + after.tv_nsec;
    assert!(after_ns >= before_ns + 2_250_000_000);
}

#[test]
fn dispatch_adjtimex_tick_and_timeconst_bookkeeping_round_trip() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tx = TestTimex {
        modes: ADJ_TICK | ADJ_TIMECONST,
        tick: 10_123,
        constant: 17,
        ..TestTimex::default()
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_ADJTIMEX,
            [&mut tx as *mut TestTimex as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(tx.tick, 10_123);
    assert_eq!(tx.constant, 10, "time constant is clamped to Linux MAXTC");

    let mut readback = TestTimex::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_ADJTIMEX,
            [&mut readback as *mut TestTimex as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(readback.tick, 10_123);
    assert_eq!(readback.constant, 10);
}

#[test]
fn dispatch_adjtimex_rejects_out_of_range_tick() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tx = TestTimex {
        modes: ADJ_TICK,
        tick: 11_001,
        ..TestTimex::default()
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_ADJTIMEX,
            [&mut tx as *mut TestTimex as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_clock_adjtime_invalid_clock_returns_einval() {
    let (_setup, proc_cap, thread) = time_setup();
    let ctx = make_ctx(proc_cap, thread);
    let mut tx = TestTimex::default();

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_CLOCK_ADJTIME,
            [
                CLOCK_MONOTONIC as u64,
                &mut tx as *mut TestTimex as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(95));
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
fn dispatch_setitimer_rejects_invalid_timeval_and_defers_cpu_timers() {
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
    assert_eq!(result, SyscallResult::Error(E_OPNOTSUPP));
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

#[test]
fn dispatch_nanosleep_wakes_for_process_timer_signal_deadline() {
    let (_setup, proc_cap, thread) = time_setup();
    let mailbox =
        alloc::sync::Arc::new(tx_subsystems::signal::adapter::step_engine::TaskMailbox::new());
    let wheel = tx_substrate::wake::TimerWheel::new();
    let ctx = make_ctx(proc_cap.clone(), thread.clone())
        .with_mailbox(mailbox)
        .with_timer_wheel(wheel.clone());
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    let timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC);
    let process_timer = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 1_000,
        },
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_TIMER_SETTIME,
                [
                    timer_id as u64,
                    0,
                    &process_timer as *const TestItimerspec as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let req_ts = TestTimespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let req = SyscallRequest::new(
        NR_NANOSLEEP,
        [&req_ts as *const TestTimespec as u64, 0, 0, 0, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));
    assert_eq!(wheel.fire_due(5_001_000_000), 1);

    let result = pinned.as_mut().poll(&mut cx).map(|result| {
        assert_eq!(result, SyscallResult::Error(E_INTR));
    });
    assert!(
        result.is_ready(),
        "process timer deadline should interrupt nanosleep before the sleep deadline"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "nanosleep process-timer wake should publish SIGALRM"
    );
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
