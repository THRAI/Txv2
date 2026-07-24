use super::*;

use crate::linux_syscall::{
    NR_EPOLL_CREATE1, NR_EPOLL_CTL, NR_EPOLL_PWAIT2, NR_EVENTFD2, NR_FANOTIFY_INIT,
    NR_FANOTIFY_MARK, NR_INOTIFY_ADD_WATCH, NR_INOTIFY_INIT1, NR_INOTIFY_RM_WATCH, NR_PIPE2,
    NR_READ, NR_WRITE,
};
use tx_substrate::wake::{MailboxEvent, MailboxSchedulerHint, TaskMailbox};

const E_INVAL: i32 = 22;
const E_NOSYS: i32 = 38;
const EPOLL_CTL_ADD: u32 = 1;
const EPOLLIN: u32 = 0x001;
const IN_CLOEXEC: u32 = 0o2000000;
const IN_NONBLOCK: u32 = 0o4000;
const FAN_CLOEXEC: u32 = 0x0000_0001;
const FAN_NONBLOCK: u32 = 0x0000_0002;

static EVENTFD_REF_POST_COUNT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static PIPE_REF_POST_COUNT: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

fn counting_eventfd_ref_post(mailbox: &TaskMailbox, event: MailboxEvent) -> bool {
    EVENTFD_REF_POST_COUNT.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    mailbox.post(event)
}

fn counting_pipe_ref_post_with_hint(
    mailbox: &TaskMailbox,
    event: MailboxEvent,
    _hint: MailboxSchedulerHint,
) -> bool {
    PIPE_REF_POST_COUNT.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
    mailbox.post(event)
}

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

fn create_pipe(ctx: &SyscallCtx<'_>) -> [i32; 2] {
    let mut pipefd = [-1i32; 2];
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(0) => pipefd,
        other => panic!("pipe2: {other:?}"),
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
fn dispatch_eventfd_write_uses_syscall_ctx_mailbox_ref_post_for_reader_wake() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    EVENTFD_REF_POST_COUNT.store(0, core::sync::atomic::Ordering::Release);
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_mailbox_ref_post(counting_eventfd_ref_post);
    let eventfd = create_eventfd(&ctx, 0);
    let mut out = 0u64;

    let read_req = SyscallRequest::new(
        NR_READ,
        [
            eventfd as u64,
            &mut out as *mut u64 as u64,
            core::mem::size_of::<u64>() as u64,
            0,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut read = Box::pin(dispatch::<ShimsTestPmap>(read_req, &ctx));
    assert!(
        matches!(read.as_mut().poll(&mut cx), Poll::Pending),
        "read on an empty blocking eventfd should park"
    );

    let value = 7u64;
    let write_result = block_on(dispatch::<ShimsTestPmap>(
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
    ));

    assert_eq!(write_result, SyscallResult::Return(8));
    assert_eq!(
        EVENTFD_REF_POST_COUNT.load(core::sync::atomic::Ordering::Acquire),
        1,
        "eventfd write should wake the parked reader through SyscallCtx"
    );
    assert_eq!(block_on(read), SyscallResult::Return(8));
    assert_eq!(out, value);
}

#[test]
fn dispatch_pipe_write_uses_syscall_ctx_mailbox_ref_post_for_reader_wake() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    PIPE_REF_POST_COUNT.store(0, core::sync::atomic::Ordering::Release);
    let ctx = make_ctx(proc_cap, thread)
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()))
        .with_mailbox_ref_post_with_hint(counting_pipe_ref_post_with_hint);
    let [reader_fd, writer_fd] = create_pipe(&ctx);
    let mut out = [0u8; 1];

    let read_req = SyscallRequest::new(
        NR_READ,
        [reader_fd as u64, out.as_mut_ptr() as u64, 1, 0, 0, 0],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut read = Box::pin(dispatch::<ShimsTestPmap>(read_req, &ctx));
    assert!(
        matches!(read.as_mut().poll(&mut cx), Poll::Pending),
        "read on an empty blocking pipe should park"
    );

    let byte = [b'x'];
    let write_result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_WRITE,
            [writer_fd as u64, byte.as_ptr() as u64, 1, 0, 0, 0],
        ),
        &ctx,
    ));

    assert_eq!(write_result, SyscallResult::Return(1));
    assert_eq!(
        PIPE_REF_POST_COUNT.load(core::sync::atomic::Ordering::Acquire),
        1,
        "pipe write should wake the parked reader through SyscallCtx"
    );
    assert_eq!(block_on(read), SyscallResult::Return(1));
    assert_eq!(out[0], byte[0]);
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
fn dispatch_inotify_init1_creates_fd_but_watch_ops_remain_deferred() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_INOTIFY_INIT1,
            [IN_CLOEXEC as u64 | IN_NONBLOCK as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("inotify_init1 should return fd, got {other:?}"),
    };
    let file = ctx.process.fd(fd).expect("inotify fd installed");
    let flags = file.flags();
    assert!(flags.read);
    assert!(flags.cloexec);
    assert!(flags.nonblocking);
    assert!(ctx.process.fd_cloexec(fd));
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
fn dispatch_fanotify_init_creates_fd_but_mark_remains_deferred() {
    let (_setup, proc_cap, thread) = event_notify_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_FANOTIFY_INIT,
            [FAN_CLOEXEC as u64 | FAN_NONBLOCK as u64, 0, 0, 0, 0, 0],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("fanotify_init should return fd, got {other:?}"),
    };
    let file = ctx.process.fd(fd).expect("fanotify fd installed");
    let flags = file.flags();
    assert!(flags.read);
    assert!(flags.cloexec);
    assert!(flags.nonblocking);
    assert!(ctx.process.fd_cloexec(fd));
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
