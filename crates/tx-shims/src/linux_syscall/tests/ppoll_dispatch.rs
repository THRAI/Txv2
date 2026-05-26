use super::*;

use crate::linux_syscall::{CLOCK_MONOTONIC, NR_PPOLL, NR_TIMER_CREATE, NR_TIMER_SETTIME};
use tx_subsystems::signal::{step_sigaction, SigDisposition, Signum};

const E_INTR: i32 = 4;
const POLLIN: i16 = 0x0001;
const SIGALRM_RAW: u8 = 14;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestPollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

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

#[test]
fn dispatch_ppoll_wakes_for_process_timer_signal_deadline() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    proc_cap.set_fd(0, Some(tx_fs::devfs::open_console_for_init()));

    let wheel = tx_substrate::wake::TimerWheel::new();
    let ctx = make_ctx(proc_cap.clone(), thread.clone())
        .with_mailbox(alloc::sync::Arc::new(
            tx_subsystems::signal::adapter::step_engine::TaskMailbox::new(),
        ))
        .with_timer_wheel(wheel.clone());
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

    let mut pollfd = TestPollFd {
        fd: 0,
        events: POLLIN,
        revents: 0,
    };
    let req = SyscallRequest::new(
        NR_PPOLL,
        [&mut pollfd as *mut TestPollFd as u64, 1, 0, 0, 0, 0],
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
        "process timer deadline should wake blocked ppoll"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "ppoll process-timer wake should publish SIGALRM"
    );
}
