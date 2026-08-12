// Auto-extracted from `tests.rs` (2026-05-21 musl ABI audit).
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{
    NR_EPOLL_CREATE1, NR_EPOLL_CTL, NR_EPOLL_PWAIT, NR_EVENTFD2, NR_STATX, NR_USERFAULTFD, NR_WRITE,
};
use tx_services::time::{
    DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle, TimeError, TimerRole, TimerTarget,
    TimerToken,
};
use tx_substrate::step::DelegateTokenId;
use tx_substrate::wake::MailboxSchedulerHint;
use tx_subsystems::signal::adapter::step_engine::{MailboxEvent, TaskMailbox};
use tx_subsystems::userfaultfd::UffdMsg;

const E_BADF: i32 = 9;
const E_FAULT: i32 = 14;
const E_INVAL: i32 = 22;
const E_EXIST: i32 = 17;
const E_NOENT: i32 = 2;
const EPOLL_CTL_ADD: u32 = 1;
const EPOLL_CTL_DEL: u32 = 2;
const EPOLL_CTL_MOD: u32 = 3;
const EPOLLIN: u32 = 0x001;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;

fn direct_ufd_ref_post_with_hint(
    mailbox: &TaskMailbox,
    event: MailboxEvent,
    hint: MailboxSchedulerHint,
) -> bool {
    mailbox.post_with_scheduler_hint(event, hint)
}

#[derive(Default)]
struct EpollDeadlineDomain {
    registration: std::sync::Mutex<Option<EpollDeadlineRegistration>>,
}

struct EpollDeadlineRegistration {
    role: TimerRole,
    mailbox: alloc::sync::Weak<TaskMailbox>,
    token: TimerToken,
}

impl DeadlineDomain for EpollDeadlineDomain {
    fn register_deadline(
        &self,
        _deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        let TimerTarget::TaskMailbox(mailbox) = target else {
            panic!("epoll timeout must use a task-mailbox deadline");
        };
        let token = TimerToken::new(0xE001);
        *self.registration.lock().unwrap() = Some(EpollDeadlineRegistration {
            role,
            mailbox,
            token,
        });
        Ok(token)
    }

    fn cancel_deadline(&self, _token: TimerToken) -> bool {
        true
    }
}

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
fn dispatch_epoll_pwait_positive_timeout_uses_deadline_abort_task_timer() {
    let (_setup, proc_cap, thread) = epoll_setup();
    let mailbox = alloc::sync::Arc::new(TaskMailbox::new());
    let domain = alloc::sync::Arc::new(EpollDeadlineDomain::default());
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::clone(&mailbox))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let epfd = create_epoll(&ctx);
    let eventfd = create_eventfd(&ctx, 0);
    let mut event = TestEpollEvent {
        events: EPOLLIN,
        _padding: 0,
        data: 0xABCD,
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
        [epfd as u64, out.as_mut_ptr() as u64, 1, 1, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    let first = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(first, Poll::Pending),
        "epoll_pwait with unreadable fd and finite timeout should park on a deadline timer; got {first:?}"
    );
    let registration = domain
        .registration
        .lock()
        .unwrap()
        .take()
        .expect("finite epoll_pwait timeout should register a DeadlineAbort timer");
    assert_eq!(registration.role, TimerRole::DeadlineAbort);
    let target_mailbox = registration
        .mailbox
        .upgrade()
        .expect("deadline timer should retain the syscall task mailbox");
    assert!(alloc::sync::Arc::ptr_eq(&target_mailbox, &mailbox));
    assert!(mailbox.post(MailboxEvent::TimerFired {
        token: registration.token,
    }));

    for _ in 0..16 {
        if let Poll::Ready(result) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(0));
            assert_eq!(out[0], TestEpollEvent::default());
            return;
        }
    }
    panic!("epoll_pwait did not resolve after unified timer expiry");
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
    ufd_cap.push_fault_msg_with_post(
        UffdMsg {
            event: UFFD_EVENT_PAGEFAULT,
            fault_addr: 0x4000,
            ufd_thread_id: 1,
            token_id: DelegateTokenId::new(1),
        },
        direct_ufd_ref_post_with_hint,
    );

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
