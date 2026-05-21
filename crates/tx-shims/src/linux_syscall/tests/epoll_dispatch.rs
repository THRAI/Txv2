// Auto-extracted from `tests.rs` (2026-05-21 musl ABI audit).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{NR_EPOLL_CREATE1, NR_EPOLL_CTL, NR_EPOLL_PWAIT, NR_EVENTFD2, NR_STATX};

const E_BADF: i32 = 9;
const E_FAULT: i32 = 14;
const E_INVAL: i32 = 22;
const E_EXIST: i32 = 17;
const E_NOENT: i32 = 2;
const EPOLL_CTL_ADD: u32 = 1;
const EPOLL_CTL_DEL: u32 = 2;
const EPOLL_CTL_MOD: u32 = 3;
const EPOLLIN: u32 = 0x001;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestEpollEvent {
    events: u32,
    _padding: u32,
    data: u64,
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

#[test]
fn dispatch_epoll_numbers_match_generic_musl_not_statx() {
    assert_eq!(NR_EPOLL_CREATE1, 20);
    assert_eq!(NR_EPOLL_CTL, 21);
    assert_eq!(NR_EPOLL_PWAIT, 22);
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
                [epfd as u64, EPOLL_CTL_DEL as u64, eventfd as u64, 0, 0, 0],
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
                [epfd as u64, EPOLL_CTL_DEL as u64, eventfd as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [epfd as u64, EPOLL_CTL_DEL as u64, eventfd as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_NOENT)
    );
}
