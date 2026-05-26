// Auto-extracted from `tests.rs` (2026-05-21 musl ABI audit).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    CLOCK_MONOTONIC, NR_EPOLL_CREATE1, NR_EPOLL_CTL, NR_EPOLL_PWAIT, NR_EVENTFD2, NR_MEMFD_CREATE,
    NR_PIPE2, NR_STATX, NR_TIMER_CREATE, NR_TIMER_SETTIME, NR_USERFAULTFD, NR_WRITE,
};
use tx_substrate::step::DelegateTokenId;
use tx_subsystems::signal::{
    adapter::step_engine::TaskMailbox, step_sigaction, SigDisposition, Signum,
};
use tx_subsystems::userfaultfd::UffdMsg;

const E_BADF: i32 = 9;
const E_FAULT: i32 = 14;
const E_INVAL: i32 = 22;
const E_EXIST: i32 = 17;
const E_NOENT: i32 = 2;
const E_PERM: i32 = 1;
const EPOLL_CTL_ADD: u32 = 1;
const EPOLL_CTL_DEL: u32 = 2;
const EPOLL_CTL_MOD: u32 = 3;
const EPOLLIN: u32 = 0x001;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const SIGALRM_RAW: u8 = 14;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestEpollEvent {
    events: u32,
    _padding: u32,
    data: u64,
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

fn epoll_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

fn create_epoll(ctx: &SyscallCtx<'_>) -> i64 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_EPOLL_CREATE1, [0, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("epoll_create1: {other:?}"),
    }
}

fn create_eventfd(ctx: &SyscallCtx<'_>, init_val: u64) -> i64 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_EVENTFD2, [init_val, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("eventfd2: {other:?}"),
    }
}

fn create_memfd(ctx: &SyscallCtx<'_>) -> i64 {
    let name = b"epoll-regular\0";
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MEMFD_CREATE, [name.as_ptr() as u64, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("memfd_create: {other:?}"),
    }
}

fn create_pipe(ctx: &SyscallCtx<'_>) -> [i32; 2] {
    let mut fds = [-1i32; 2];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIPE2, [fds.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    fds
}

fn epoll_add(ctx: &SyscallCtx<'_>, epfd: i64, fd: i64) -> SyscallResult {
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: fd as u64,
    };
    block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_CTL,
            [
                epfd as u64,
                EPOLL_CTL_ADD as u64,
                fd as u64,
                &mut event as *mut TestEpollEvent as u64,
                0,
                0,
            ],
        ),
        ctx,
    ))
}

fn create_posix_timer(ctx: &SyscallCtx<'_>, clock: i32) -> i32 {
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

#[test]
fn dispatch_epoll_numbers_match_generic_musl_not_statx() {
    assert_eq!(NR_EPOLL_CREATE1, 20);
    assert_eq!(NR_EPOLL_CTL, 21);
    assert_eq!(NR_EPOLL_PWAIT, 22);
    assert_eq!(NR_EVENTFD2, 19);
    assert_eq!(NR_STATX, 291);
}

#[test]
fn dispatch_epoll_create_ctl_pwait_returns_lp64_event_data() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 1);

    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0xfeed_cafe_dead_beef,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_CTL,
            [
                epfd as u64,
                EPOLL_CTL_ADD as u64,
                eventfd as u64,
                &mut event as *mut TestEpollEvent as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut out = [TestEpollEvent::default(); 2];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_PWAIT,
            [
                epfd as u64,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(1));
    assert_eq!(out[0].events, EPOLLIN);
    assert_eq!(out[0].data, event.data);
    assert_eq!(core::mem::size_of::<TestEpollEvent>(), 16);
}

#[test]
fn dispatch_epoll_pwait_zero_timeout_returns_zero_when_no_ready_fd() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 0);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0x1234,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_CTL,
            [
                epfd as u64,
                EPOLL_CTL_ADD as u64,
                eventfd as u64,
                &mut event as *mut TestEpollEvent as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut out = [TestEpollEvent::default(); 1];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_PWAIT,
            [epfd as u64, out.as_mut_ptr() as u64, 1, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out[0], TestEpollEvent::default());
}

#[test]
fn dispatch_epoll_pwait_positive_timeout_no_ready_fd_returns_zero_not_enosys() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 0);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0x5678,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    eventfd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut out = [TestEpollEvent::default(); 1];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_PWAIT,
            [epfd as u64, out.as_mut_ptr() as u64, 1, 1, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out[0], TestEpollEvent::default());
}

#[test]
fn dispatch_epoll_pwait_blocks_until_eventfd_becomes_readable() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread).with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()));
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 0);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0x1111_2222_3333_4444,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    eventfd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut out = [TestEpollEvent::default(); 1];
    let req = SyscallRequest::new(
        NR_EPOLL_PWAIT,
        [epfd as u64, out.as_mut_ptr() as u64, 1, -1i32 as u64, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    let first = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(first, Poll::Pending),
        "epoll_pwait on an unreadable eventfd should park; got {first:?}"
    );

    let value = 1u64;
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_WRITE,
                [
                    eventfd as u64,
                    &value as *const u64 as u64,
                    core::mem::size_of::<u64>() as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(core::mem::size_of::<u64>() as i64)
    );

    for _ in 0..256 {
        if let Poll::Ready(result) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(1));
            assert_eq!(out[0].events, EPOLLIN);
            assert_eq!(out[0].data, event.data);
            return;
        }
    }
    panic!("epoll_pwait did not resolve after eventfd write woke the reader source");
}

#[test]
fn dispatch_epoll_pwait_wakes_for_process_timer_signal_deadline() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread.clone())
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()));
    let sigalrm = Signum::new(SIGALRM_RAW).expect("SIGALRM signum");
    let _ = step_sigaction(&proc_cap, sigalrm, SigDisposition::Handler(0xCAFE));

    let timer_id = create_posix_timer(&ctx, CLOCK_MONOTONIC as i32);
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

    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 0);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0x5151,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    eventfd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut out = [TestEpollEvent::default(); 1];
    let req = SyscallRequest::new(
        NR_EPOLL_PWAIT,
        [epfd as u64, out.as_mut_ptr() as u64, 1, -1i32 as u64, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    assert!(matches!(pinned.as_mut().poll(&mut cx), Poll::Pending));

    timer_queue.advance_time_to(u64::MAX);

    let result = pinned.as_mut().poll(&mut cx).map(|result| {
        assert_eq!(result, SyscallResult::Return(0));
    });
    assert!(
        result.is_ready(),
        "process timer deadline should wake the blocked epoll wait"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "process timer wake should publish SIGALRM through the existing signal path"
    );
}

#[test]
fn dispatch_epoll_pwait_reports_userfaultfd_pending_fault_readable() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let epfd = create_epoll(&ctx);
    let ufd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_USERFAULTFD, [0, 0, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("userfaultfd: {other:?}"),
    };
    let ufd_file = proc_cap.fd(ufd as u32).expect("ufd installed");
    let ufd_cap = ufd_file.ufd().expect("ufd backing").clone();
    ufd_cap.push_fault_msg(UffdMsg {
        event: UFFD_EVENT_PAGEFAULT,
        fault_addr: 0x4000,
        ufd_thread_id: 1,
        token_id: DelegateTokenId::new(1),
    });

    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0xaaaa_bbbb_cccc_dddd,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    ufd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut out = [TestEpollEvent::default(); 1];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_PWAIT,
            [epfd as u64, out.as_mut_ptr() as u64, 1, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(1));
    assert_eq!(out[0].events, EPOLLIN);
    assert_eq!(out[0].data, event.data);
}

#[test]
fn dispatch_epoll_ctl_validates_fd_and_event_pointer() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_CTL,
            [epfd as u64, EPOLL_CTL_ADD as u64, 99, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_FAULT));

    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0,
    };
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_CTL,
            [
                epfd as u64,
                EPOLL_CTL_ADD as u64,
                99,
                &mut event as *mut TestEpollEvent as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_BADF));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_CTL,
            [
                epfd as u64,
                EPOLL_CTL_ADD as u64,
                epfd as u64,
                &mut event as *mut TestEpollEvent as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_epoll_ctl_add_regular_file_returns_neg_eperm() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let regular_fd = create_memfd(&ctx);

    assert_eq!(
        epoll_add(&ctx, epfd, regular_fd),
        SyscallResult::Error(E_PERM)
    );
}

#[test]
fn dispatch_epoll_ctl_add_pipe_read_end_is_pollable() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let pipe_fds = create_pipe(&ctx);

    assert_eq!(
        epoll_add(&ctx, epfd, pipe_fds[0] as i64),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_epoll_ctl_add_rejects_too_deep_epoll_nesting() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let ep0 = create_epoll(&ctx);
    let ep1 = create_epoll(&ctx);
    let ep2 = create_epoll(&ctx);
    let ep3 = create_epoll(&ctx);
    let ep4 = create_epoll(&ctx);
    let ep5 = create_epoll(&ctx);

    assert_eq!(epoll_add(&ctx, ep4, ep5), SyscallResult::Return(0));
    assert_eq!(epoll_add(&ctx, ep3, ep4), SyscallResult::Return(0));
    assert_eq!(epoll_add(&ctx, ep2, ep3), SyscallResult::Return(0));
    assert_eq!(epoll_add(&ctx, ep1, ep2), SyscallResult::Return(0));
    assert_eq!(epoll_add(&ctx, ep0, ep1), SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_epoll_ctl_del_allows_null_event_pointer() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 1);

    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    eventfd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [epfd as u64, EPOLL_CTL_DEL as u64, eventfd as u64, 0, 0, 0,],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_epoll_ctl_add_mod_del_enforce_registration_state() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 1);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 1,
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_MOD as u64,
                    eventfd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_NOENT)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    eventfd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    eventfd as u64,
                    &mut event as *mut TestEpollEvent as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_EXIST)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [epfd as u64, EPOLL_CTL_DEL as u64, eventfd as u64, 0, 0, 0,],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [epfd as u64, EPOLL_CTL_DEL as u64, eventfd as u64, 0, 0, 0,],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_NOENT)
    );
}
