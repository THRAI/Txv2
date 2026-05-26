use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, NR_READ, NR_TIMER_CREATE, NR_TIMER_SETTIME, NR_USERFAULTFD,
};
use tx_substrate::step::DelegateTokenId;
use tx_subsystems::signal::{step_sigaction, SigDisposition, SignalMask, Signum};
use tx_subsystems::userfaultfd::{UffdMsg, UFFD_MSG_WIRE_SIZE};

const E_INTR: i32 = 4;
const SIGALRM_RAW: u8 = 14;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;

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

fn arm_short_timer(ctx: &SyscallCtx<'_>) {
    let timer_id = create_posix_timer(ctx, CLOCK_MONOTONIC);
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

fn create_userfaultfd(ctx: &SyscallCtx<'_>) -> i32 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_USERFAULTFD, [0, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd as i32,
        other => panic!("userfaultfd failed: {other:?}"),
    }
}

#[test]
fn dispatch_userfaultfd_read_returns_eintr_when_process_timer_expires() {
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
    let ufd = create_userfaultfd(&ctx);
    let mut msg = [0u8; UFFD_MSG_WIRE_SIZE];
    let req = SyscallRequest::new(
        NR_READ,
        [
            ufd as u64,
            msg.as_mut_ptr() as u64,
            msg.len() as u64,
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
        "empty userfaultfd read should be interrupted by deliverable timer signal"
    );
}

#[test]
fn dispatch_userfaultfd_read_keeps_waiting_for_masked_process_timer() {
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
    thread
        .payload_cap()
        .expect("live thread")
        .store_signal_mask(SignalMask::new(sigalrm.bit()));

    arm_short_timer(&ctx);
    let ufd = create_userfaultfd(&ctx);
    let mut msg = [0u8; UFFD_MSG_WIRE_SIZE];
    let req = SyscallRequest::new(
        NR_READ,
        [
            ufd as u64,
            msg.as_mut_ptr() as u64,
            msg.len() as u64,
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
        "masked timer signal should not abort an empty userfaultfd read"
    );
}

#[test]
fn dispatch_userfaultfd_read_returns_fault_message_when_timer_and_fault_are_ready() {
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
    let ufd = create_userfaultfd(&ctx);
    let ufd_file = proc_cap.fd(ufd as u32).expect("ufd installed");
    let ufd_cap = ufd_file.ufd().expect("ufd backing").clone();
    let mut msg = [0u8; UFFD_MSG_WIRE_SIZE];
    let req = SyscallRequest::new(
        NR_READ,
        [
            ufd as u64,
            msg.as_mut_ptr() as u64,
            msg.len() as u64,
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
    ufd_cap.push_fault_msg(UffdMsg {
        event: UFFD_EVENT_PAGEFAULT,
        fault_addr: 0x4000,
        ufd_thread_id: 7,
        token_id: DelegateTokenId::new(1),
    });

    let result = pinned.as_mut().poll(&mut cx).map(|result| {
        assert_eq!(result, SyscallResult::Return(UFFD_MSG_WIRE_SIZE as i64));
    });
    assert!(
        result.is_ready(),
        "queued userfaultfd message should win over timer interruption"
    );
    assert_eq!(msg[0], UFFD_EVENT_PAGEFAULT);
    assert_eq!(
        u64::from_le_bytes(msg[16..24].try_into().expect("fault address bytes")),
        0x4000
    );
    assert_eq!(
        u32::from_le_bytes(msg[24..28].try_into().expect("tid bytes")),
        7
    );
}
