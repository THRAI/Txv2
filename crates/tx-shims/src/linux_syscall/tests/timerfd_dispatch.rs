// Auto-extracted from `tests.rs` (2026-05-21 musl ABI audit).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::numbers::TFD_TIMER_ABSTIME_FLAG;
use crate::linux_syscall::{
    CLOCK_MONOTONIC, CLOCK_REALTIME, NR_READ, NR_TIMERFD_CREATE, NR_TIMERFD_GETTIME,
    NR_TIMERFD_SETTIME, NR_TIMER_CREATE, NR_TIMER_SETTIME, TFD_TIMER_ABSTIME_FLAG,
    TFD_TIMER_CANCEL_ON_SET_FLAG,
};
use tx_subsystems::signal::{
    adapter::step_engine::TaskMailbox, step_sigaction, SigDisposition, Signum,
};

const E_INVAL: i32 = 22;
const E_INTR: i32 = 4;
const E_CANCELED: i32 = 125;
const SIGALRM_RAW: u8 = 14;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestItimerspec {
    it_interval: TestTimespec,
    it_value: TestTimespec,
}

fn timerfd_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

fn create_timerfd(ctx: &SyscallCtx<'_>) -> i64 {
    create_timerfd_with_clock(ctx, CLOCK_MONOTONIC)
}

fn create_timerfd_with_clock(ctx: &SyscallCtx<'_>, clockid: u32) -> i64 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMERFD_CREATE, [clockid as u64, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("timerfd_create: {other:?}"),
    }
}

fn create_posix_timer(ctx: &SyscallCtx<'_>, clock: u32) -> i32 {
    let mut timer_id = -1i32;
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_CREATE,
            [clock as u64, 0, &mut timer_id as *mut i32 as u64, 0, 0, 0],
        ),
        ctx,
    )) {
        SyscallResult::Return(0) => timer_id,
        other => panic!("timer_create: {other:?}"),
    }
}

fn ns_from_timespec(ts: TestTimespec) -> u64 {
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

fn assert_ns_near(actual: u64, expected: u64, context: &str) {
    assert!(
        actual <= expected && actual >= expected.saturating_sub(1_000_000),
        "{context}: expected close to {expected}ns, got {actual}ns",
    );
}

#[test]
fn dispatch_timerfd_settime_and_gettime_use_musl_itimerspec_layout() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd(&ctx);

    let first = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 0,
            tv_nsec: 500_000_000,
        },
        it_value: TestTimespec {
            tv_sec: 2,
            tv_nsec: 0,
        },
    };
    let first_ptr = &first as *const TestItimerspec as u64;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMERFD_SETTIME, [fd as u64, 0, first_ptr, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let second = TestItimerspec {
        it_interval: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
        it_value: TestTimespec {
            tv_sec: 3,
            tv_nsec: 0,
        },
    };
    let mut old = TestItimerspec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0,
                &second as *const TestItimerspec as u64,
                &mut old as *mut TestItimerspec as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(ns_from_timespec(old.it_interval), 500_000_000);
    assert_ns_near(
        ns_from_timespec(old.it_value),
        2_000_000_000,
        "old.it_value",
    );

    let mut current = TestItimerspec::default();
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_GETTIME,
            [
                fd as u64,
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
    assert_eq!(ns_from_timespec(current.it_interval), 1_000_000_000);
    assert_ns_near(
        ns_from_timespec(current.it_value),
        3_000_000_000,
        "current.it_value",
    );
}

#[test]
fn dispatch_timerfd_realtime_abstime_is_converted_to_monotonic_deadline() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TIMERFD_CREATE, [CLOCK_REALTIME as u64, 0, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("timerfd_create realtime: {other:?}"),
    };

    let realtime_deadline = realtime_ns::<ShimsTestPmap>().saturating_add(100_000_000);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: (realtime_deadline / 1_000_000_000) as i64,
            tv_nsec: (realtime_deadline % 1_000_000_000) as i64,
        },
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                TFD_TIMER_ABSTIME_FLAG as u64,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let file = proc_cap.fd(fd as u32).expect("timerfd fd installed");
    let tfd = file.timerfd().expect("timerfd backing");
    let remaining = tfd.remaining_value_ns(<ShimsTestPmap as tx_hal::TimeIf>::read_ns());
    assert!(
        remaining <= 100_000_000,
        "realtime absolute timer must be stored in monotonic deadline domain, got remaining {remaining}ns",
    );
}

#[test]
fn dispatch_timerfd_settime_rejects_unknown_flags() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd(&ctx);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0x8000_0000,
                &new_value as *const TestItimerspec as u64,
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
fn dispatch_timerfd_settime_rejects_invalid_nsec() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd(&ctx);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 0,
            tv_nsec: 1_000_000_000,
        },
    };

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                0,
                &new_value as *const TestItimerspec as u64,
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
fn dispatch_timerfd_cancel_on_set_realtime_abstime_read_returns_ecanceled() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_timerfd_with_clock(&ctx, CLOCK_REALTIME);
    let new_value = TestItimerspec {
        it_interval: TestTimespec::default(),
        it_value: TestTimespec {
            tv_sec: 1_900_000_000,
            tv_nsec: 0,
        },
    };
    let flags = TFD_TIMER_ABSTIME_FLAG | TFD_TIMER_CANCEL_ON_SET_FLAG;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMERFD_SETTIME,
            [
                fd as u64,
                flags as u64,
                &new_value as *const TestItimerspec as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let set = TestTimespec {
        tv_sec: 1_800_000_000,
        tv_nsec: 0,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            crate::linux_syscall::NR_CLOCK_SETTIME,
            [
                CLOCK_REALTIME as u64,
                &set as *const TestTimespec as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut buf = 0u64;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_READ,
            [fd as u64, &mut buf as *mut u64 as u64, 8, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_CANCELED));
}

#[test]
fn dispatch_timerfd_read_wakes_for_process_timer_signal_deadline() {
    let (_setup, proc_cap, thread) = timerfd_setup();
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread.clone())
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    let posix_timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC);
    let posix_timer = TestItimerspec {
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
                    posix_timer_id as u64,
                    0,
                    &posix_timer as *const TestItimerspec as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let fd = create_timerfd(&ctx);
    let mut buf = 0u64;
    let req = SyscallRequest::new(
        NR_READ,
        [fd as u64, &mut buf as *mut u64 as u64, 8, 0, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));

    timer_queue.advance_time_to(u64::MAX);

    let result = pinned.as_mut().poll(&mut cx).map(|result| {
        assert_eq!(result, SyscallResult::Error(E_INTR));
    });
    assert!(
        result.is_ready(),
        "process timer deadline should wake the blocked timerfd read"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "process timer wake should publish SIGALRM through the existing signal path"
    );
}
