use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, NR_IO_GETEVENTS, NR_IO_SETUP, NR_TIMER_CREATE, NR_TIMER_SETTIME,
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

fn create_aio_context(ctx: &SyscallCtx<'_>) -> i32 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_IO_SETUP, [1, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd as i32,
        other => panic!("io_setup failed: {other:?}"),
    }
}

#[test]
fn dispatch_io_getevents_wakes_for_process_timer_signal_deadline() {
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

    let aio_fd = create_aio_context(&ctx);
    let mut event = [0u8; tx_subsystems::aio::IO_EVENT_BYTES];
    let req = SyscallRequest::new(
        NR_IO_GETEVENTS,
        [aio_fd as u64, 1, 1, event.as_mut_ptr() as u64, 0, 0],
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
        "process timer deadline should wake blocked io_getevents"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "io_getevents process-timer wake should publish SIGALRM"
    );
}
