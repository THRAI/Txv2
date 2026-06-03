use super::*;

use crate::linux_syscall::{
    NR_EPOLL_CREATE1, NR_EPOLL_CTL, NR_EPOLL_PWAIT2, NR_EVENTFD2, NR_FANOTIFY_INIT,
    NR_FANOTIFY_MARK, NR_INOTIFY_ADD_WATCH, NR_INOTIFY_INIT1, NR_INOTIFY_RM_WATCH, NR_WRITE,
};
use tx_subsystems::signal::adapter::step_engine::TaskMailbox;

const E_INVAL: i32 = 22;
const E_NOSYS: i32 = 38;
const EPOLL_CTL_ADD: u32 = 1;
const EPOLLIN: u32 = 0x001;
const IN_CLOEXEC: u32 = 0o2000000;
const IN_NONBLOCK: u32 = 0o4000;
const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestEpollEvent {
    events: u32,
    _padding: u32,
    data: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

fn event_notify_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
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

#[test]
fn event_notification_numbers_match_linux_rv64_6_17() {
    assert_eq!(NR_INOTIFY_INIT1, 26);
    assert_eq!(NR_INOTIFY_ADD_WATCH, 27);
    assert_eq!(NR_INOTIFY_RM_WATCH, 28);
    assert_eq!(NR_FANOTIFY_INIT, 262);
    assert_eq!(NR_FANOTIFY_MARK, 263);
    assert_eq!(NR_EPOLL_PWAIT2, 441);
}

#[test]
fn dispatch_epoll_pwait2_null_timeout_reuses_epoll_ready_path() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    let ctx = make_ctx(proc_cap, thread).with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()));
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 0);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0x5152_5354_5556_5758,
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

    let mut out = [TestEpollEvent::default(); 1];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_PWAIT2,
            [epfd as u64, out.as_mut_ptr() as u64, 1, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(1));
    assert_eq!(out[0].events, EPOLLIN);
    assert_eq!(out[0].data, event.data);
}

#[test]
fn dispatch_epoll_pwait2_zero_timeout_returns_zero_when_no_ready_fd() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 0);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0x9090,
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

    let timeout = TestTimespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let mut out = [TestEpollEvent::default(); 1];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_PWAIT2,
            [
                epfd as u64,
                out.as_mut_ptr() as u64,
                1,
                &timeout as *const TestTimespec as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(out[0], TestEpollEvent::default());
}

#[test]
fn dispatch_epoll_pwait2_rejects_invalid_timespec() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let timeout = TestTimespec {
        tv_sec: 0,
        tv_nsec: 1_000_000_000,
    };
    let mut out = [TestEpollEvent::default(); 1];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_EPOLL_PWAIT2,
            [
                epfd as u64,
                out.as_mut_ptr() as u64,
                1,
                &timeout as *const TestTimespec as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_inotify_scaffold_validates_flags_then_reports_deferred_storage() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    let ctx = make_ctx(proc_cap, thread);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_INOTIFY_INIT1,
                [IN_CLOEXEC as u64 | IN_NONBLOCK as u64, 0, 0, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Error(E_NOSYS)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_INOTIFY_INIT1, [0x8000_0000, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_INOTIFY_ADD_WATCH, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_NOSYS)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_INOTIFY_RM_WATCH, [0, 1, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_NOSYS)
    );
}

#[test]
fn dispatch_fanotify_scaffold_validates_init_flags_then_reports_deferred_storage() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    let ctx = make_ctx(proc_cap, thread);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FANOTIFY_INIT,
                [FAN_CLOEXEC as u64 | FAN_NONBLOCK as u64, 0, 0, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Error(E_NOSYS)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FANOTIFY_INIT, [0x8000_0000, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FANOTIFY_MARK, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_NOSYS)
    );
}
