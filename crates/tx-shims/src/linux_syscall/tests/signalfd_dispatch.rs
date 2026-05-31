use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, NR_READ, NR_SIGNALFD4, NR_TIMER_CREATE, NR_TIMER_SETTIME,
};
use tx_subsystems::signal::{step_sigaction, SigDisposition, SignalMask, Signum};

const E_INTR: i32 = 4;
const SI_TIMER_VALUE: i32 = -2;
const SIGEV_SIGNAL_VALUE: i32 = 0;
const SIGALRM_RAW: u8 = 14;
const SIGUSR1_RAW: u8 = 10;

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

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestSigeventPrefix {
    sigval: u64,
    sigev_signo: i32,
    sigev_notify: i32,
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

fn create_posix_timer_with_event(
    ctx: &SyscallCtx<'_>,
    clock: u32,
    event: &TestSigeventPrefix,
) -> i32 {
    let mut timer_id = -1i32;
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_TIMER_CREATE,
            [
                clock as u64,
                event as *const TestSigeventPrefix as u64,
                &mut timer_id as *mut i32 as u64,
                0,
                0,
                0,
            ],
        ),
        ctx,
    )) {
        SyscallResult::Return(0) => timer_id,
        other => panic!("timer_create with event failed: {other:?}"),
    }
}

fn arm_short_timer(ctx: &SyscallCtx<'_>) {
    let timer_id = create_posix_timer(ctx, CLOCK_MONOTONIC);
    arm_existing_short_timer(ctx, timer_id);
}

fn arm_existing_short_timer(ctx: &SyscallCtx<'_>, timer_id: i32) {
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
            ctx,
        )),
        SyscallResult::Return(0)
    );
}

fn create_signalfd(ctx: &SyscallCtx<'_>, mask: u64) -> i32 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SIGNALFD4,
            [(-1i32) as u64, &mask as *const u64 as u64, 8, 0, 0, 0],
        ),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd as i32,
        other => panic!("signalfd4 failed: {other:?}"),
    }
}

#[test]
fn dispatch_signalfd_read_returns_timer_signal_record_when_mask_matches() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread).with_mailbox(alloc::sync::Arc::new(
        tx_subsystems::signal::adapter::step_engine::TaskMailbox::new(),
    ));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    arm_short_timer(&ctx);
    let sfd = create_signalfd(&ctx, sigalrm.bit());
    let mut siginfo = [0u8; tx_subsystems::signalfd::SIGNALFD_SIGINFO_SIZE];
    let req = SyscallRequest::new(
        NR_READ,
        [
            sfd as u64,
            siginfo.as_mut_ptr() as u64,
            siginfo.len() as u64,
            0,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));

    timer_queue.advance_time_to(u64::MAX);

    let result = pinned.as_mut().poll(&mut cx).map(|result| {
        assert_eq!(
            result,
            SyscallResult::Return(tx_subsystems::signalfd::SIGNALFD_SIGINFO_SIZE as i64)
        );
    });
    assert!(
        result.is_ready(),
        "process timer signal covered by signalfd mask should complete read"
    );
    assert_eq!(
        u32::from_le_bytes([siginfo[0], siginfo[1], siginfo[2], siginfo[3]]),
        SIGALRM_RAW as u32
    );
}

#[test]
fn dispatch_signalfd_read_preserves_posix_timer_sigval() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread).with_mailbox(alloc::sync::Arc::new(
        tx_subsystems::signal::adapter::step_engine::TaskMailbox::new(),
    ));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    let event = TestSigeventPrefix {
        sigval: 0x1122_3344_5566_7788,
        sigev_signo: SIGALRM_RAW as i32,
        sigev_notify: SIGEV_SIGNAL_VALUE,
    };
    let timer_id = create_posix_timer_with_event(&ctx, CLOCK_MONOTONIC, &event);
    arm_existing_short_timer(&ctx, timer_id);
    let sfd = create_signalfd(&ctx, sigalrm.bit());
    let mut siginfo = [0u8; tx_subsystems::signalfd::SIGNALFD_SIGINFO_SIZE];
    let req = SyscallRequest::new(
        NR_READ,
        [
            sfd as u64,
            siginfo.as_mut_ptr() as u64,
            siginfo.len() as u64,
            0,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));
    timer_queue.advance_time_to(u64::MAX);

    let result = pinned.as_mut().poll(&mut cx).map(|result| {
        assert_eq!(
            result,
            SyscallResult::Return(tx_subsystems::signalfd::SIGNALFD_SIGINFO_SIZE as i64)
        );
    });
    assert!(
        result.is_ready(),
        "process timer signal covered by signalfd mask should complete read"
    );
    assert_eq!(
        u32::from_le_bytes(siginfo[0..4].try_into().unwrap()),
        SIGALRM_RAW as u32
    );
    assert_eq!(
        i32::from_le_bytes(siginfo[8..12].try_into().unwrap()),
        SI_TIMER_VALUE
    );
    assert_eq!(
        u32::from_le_bytes(siginfo[44..48].try_into().unwrap()),
        0x5566_7788
    );
    assert_eq!(
        u64::from_le_bytes(siginfo[48..56].try_into().unwrap()),
        0x1122_3344_5566_7788
    );
}

#[test]
fn dispatch_signalfd_read_returns_eintr_when_timer_signal_not_in_mask() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread).with_mailbox(alloc::sync::Arc::new(
        tx_subsystems::signal::adapter::step_engine::TaskMailbox::new(),
    ));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let sigusr1 = Signum::new(SIGUSR1_RAW).expect("SIGUSR1 signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    arm_short_timer(&ctx);
    let sfd = create_signalfd(&ctx, sigusr1.bit());
    let mut siginfo = [0u8; tx_subsystems::signalfd::SIGNALFD_SIGINFO_SIZE];
    let req = SyscallRequest::new(
        NR_READ,
        [
            sfd as u64,
            siginfo.as_mut_ptr() as u64,
            siginfo.len() as u64,
            0,
            0,
            0,
        ],
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
        "deliverable timer signal outside signalfd mask should interrupt read"
    );
}

#[test]
fn dispatch_signalfd_read_keeps_waiting_for_masked_timer_signal_outside_mask() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread.clone()).with_mailbox(alloc::sync::Arc::new(
        tx_subsystems::signal::adapter::step_engine::TaskMailbox::new(),
    ));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let sigusr1 = Signum::new(SIGUSR1_RAW).expect("SIGUSR1 signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));
    thread
        .payload_cap()
        .expect("live thread")
        .store_signal_mask(SignalMask::new(sigalrm.bit()));

    arm_short_timer(&ctx);
    let sfd = create_signalfd(&ctx, sigusr1.bit());
    let mut siginfo = [0u8; tx_subsystems::signalfd::SIGNALFD_SIGINFO_SIZE];
    let req = SyscallRequest::new(
        NR_READ,
        [
            sfd as u64,
            siginfo.as_mut_ptr() as u64,
            siginfo.len() as u64,
            0,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));

    timer_queue.advance_time_to(u64::MAX);

    assert!(
        matches!(pinned.as_mut().poll(&mut cx), Poll::Pending),
        "masked timer signal outside signalfd mask should not interrupt read"
    );
}
