use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, NR_EVENTFD2, NR_READ, NR_TIMER_CREATE, NR_TIMER_SETTIME, NR_WRITE,
};
use tx_subsystems::signal::{step_sigaction, SigDisposition, Signum};

const E_INTR: i32 = 4;
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

fn create_eventfd(ctx: &SyscallCtx<'_>, init_val: u64) -> i32 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_EVENTFD2, [init_val, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd as i32,
        other => panic!("eventfd2 failed: {other:?}"),
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
        other => panic!("timer_create failed: {other:?}"),
    }
}

#[test]
fn dispatch_eventfd_read_wakes_for_process_timer_signal_deadline() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread.clone()).with_mailbox(alloc::sync::Arc::new(
        tx_subsystems::signal::adapter::step_engine::TaskMailbox::new(),
    ));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    let timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC);
    let timer = TestItimerspec {
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
                    &timer as *const TestItimerspec as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let eventfd = create_eventfd(&ctx, 0);
    let mut value = 0u64;
    let req = SyscallRequest::new(
        NR_READ,
        [eventfd as u64, &mut value as *mut u64 as u64, 8, 0, 0, 0],
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
        "process timer deadline should wake blocked eventfd read"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "eventfd process-timer wake should publish SIGALRM"
    );
}

#[test]
fn dispatch_eventfd_write_wakes_for_process_timer_signal_deadline() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread.clone()).with_mailbox(alloc::sync::Arc::new(
        tx_subsystems::signal::adapter::step_engine::TaskMailbox::new(),
    ));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    let timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC);
    let timer = TestItimerspec {
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
                    &timer as *const TestItimerspec as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let eventfd = create_eventfd(&ctx, u64::MAX - 1);
    let one = 1u64;
    let req = SyscallRequest::new(
        NR_WRITE,
        [eventfd as u64, &one as *const u64 as u64, 8, 0, 0, 0],
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
        "process timer deadline should wake blocked eventfd write"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "eventfd write process-timer wake should publish SIGALRM"
    );
}
