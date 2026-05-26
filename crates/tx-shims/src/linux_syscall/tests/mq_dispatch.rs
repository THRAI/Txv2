use super::*;

use crate::linux_syscall::{
    MqAttrLayout, CLOCK_MONOTONIC, NR_CLOSE, NR_EPOLL_CREATE1, NR_EPOLL_CTL, NR_EPOLL_PWAIT,
    NR_FCNTL, NR_MQ_GETSETATTR, NR_MQ_NOTIFY, NR_MQ_OPEN, NR_MQ_TIMEDRECEIVE, NR_MQ_TIMEDSEND,
    NR_MQ_UNLINK, NR_READ, NR_RT_SIGACTION, NR_TIMER_CREATE, NR_TIMER_SETTIME, NR_WRITE, O_CLOEXEC,
    O_CREAT, O_EXCL, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY,
};
use tx_subsystems::signal::adapter::step_engine::{MailboxEvent, SignalRouting, TaskMailbox};
use tx_subsystems::signal::{step_sigaction, SigDisposition, Signum};

const E_AGAIN: i32 = 11;
const E_BADF: i32 = 9;
const E_EXIST: i32 = 17;
const E_INVAL: i32 = 22;
const E_INTR: i32 = 4;
const E_MSGSIZE: i32 = 90;
const E_NOENT: i32 = 2;
const EPOLL_CTL_ADD: u32 = 1;
const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const F_GETFL_CMD: i32 = 3;
const MQ_PRIO_MAX: u64 = 32768;
const SIGALRM_RAW: u8 = 14;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TestSigeventPrefix {
    sigval: u64,
    sigev_signo: i32,
    sigev_notify: i32,
}

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

fn mq_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
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
        other => panic!("epoll_create1 failed: {other:?}"),
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

fn mq_open(
    ctx: &SyscallCtx<'_>,
    name: &[u8],
    flags: u32,
    attr: Option<&MqAttrLayout>,
) -> SyscallResult {
    let attr_ptr = attr.map(|a| a as *const MqAttrLayout as u64).unwrap_or(0);
    block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MQ_OPEN,
            [name.as_ptr() as u64, flags as u64, 0o660, attr_ptr, 0, 0],
        ),
        ctx,
    ))
}

#[test]
fn dispatch_mq_layout_and_generic_numbers_match_musl_headers() {
    assert_eq!(NR_MQ_OPEN, 180);
    assert_eq!(NR_MQ_UNLINK, 181);
    assert_eq!(NR_MQ_TIMEDSEND, 182);
    assert_eq!(NR_MQ_TIMEDRECEIVE, 183);
    assert_eq!(NR_MQ_NOTIFY, 184);
    assert_eq!(NR_MQ_GETSETATTR, 185);
    assert_eq!(core::mem::size_of::<MqAttrLayout>(), 64);
    assert_eq!(O_NONBLOCK, 0o4000);
}

#[test]
fn dispatch_mq_open_returns_fd_and_close_removes_it() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let name = b"tx-mq-close\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 4,
        mq_msgsize: 32,
        ..MqAttrLayout::default()
    };

    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR | O_CLOEXEC, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    let file = proc_cap.fd(fd).expect("fd installed");
    assert!(file.posix_mq().is_some());
    assert!(proc_cap.fd_cloexec(fd));

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_CLOSE, [fd as u64, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(proc_cap.fd(fd).is_none());
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_MQ_TIMEDSEND, [fd as u64, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_BADF)
    );
}

#[test]
fn dispatch_mq_send_receive_and_getsetattr_use_fd_descriptor() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread);
    let name = b"tx-mq-roundtrip\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 4,
        mq_msgsize: 32,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };

    let msg = *b"hello-mq";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [fd as u64, msg.as_ptr() as u64, msg.len() as u64, 7, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut old = MqAttrLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_GETSETATTR,
                [fd as u64, 0, &mut old as *mut MqAttrLayout as u64, 0, 0, 0,],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(old.mq_flags, 0);
    assert_eq!(old.mq_maxmsg, 4);
    assert_eq!(old.mq_msgsize, 32);
    assert_eq!(old.mq_curmsgs, 1);

    let mut out = [0u8; 32];
    let mut prio = 0u32;
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDRECEIVE,
                [
                    fd as u64,
                    out.as_mut_ptr() as u64,
                    out.len() as u64,
                    &mut prio as *mut u32 as u64,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(msg.len() as i64)
    );
    assert_eq!(&out[..msg.len()], &msg);
    assert_eq!(prio, 7);
}

#[test]
fn dispatch_mq_receive_returns_highest_priority_fifo_message() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread);
    let name = b"tx-mq-prio\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 4,
        mq_msgsize: 16,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR | O_NONBLOCK, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    let low = *b"low";
    let high1 = *b"high-a";
    let high2 = *b"high-b";

    for (msg, prio) in [(&low[..], 1), (&high1[..], 9), (&high2[..], 9)] {
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(
                    NR_MQ_TIMEDSEND,
                    [fd as u64, msg.as_ptr() as u64, msg.len() as u64, prio, 0, 0],
                ),
                &ctx,
            )),
            SyscallResult::Return(0)
        );
    }

    let mut out = [0u8; 16];
    let mut prio = 0u32;
    for expected in [&high1[..], &high2[..], &low[..]] {
        out.fill(0);
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(
                SyscallRequest::new(
                    NR_MQ_TIMEDRECEIVE,
                    [
                        fd as u64,
                        out.as_mut_ptr() as u64,
                        out.len() as u64,
                        &mut prio as *mut u32 as u64,
                        0,
                        0,
                    ],
                ),
                &ctx,
            )),
            SyscallResult::Return(expected.len() as i64)
        );
        assert_eq!(&out[..expected.len()], expected);
        assert_eq!(prio, if expected == &low[..] { 1 } else { 9 });
    }
}

#[test]
fn dispatch_mq_open_excl_unlink_and_reopen_match_name_registry() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread);
    let name = b"tx-mq-unlink\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 2,
        mq_msgsize: 16,
        ..MqAttrLayout::default()
    };
    assert!(matches!(
        mq_open(&ctx, name, O_CREAT | O_EXCL | O_RDONLY, Some(&attr)),
        SyscallResult::Return(_)
    ));
    assert_eq!(
        mq_open(&ctx, name, O_CREAT | O_EXCL | O_RDONLY, Some(&attr)),
        SyscallResult::Error(E_EXIST)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_MQ_UNLINK, [name.as_ptr() as u64, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        mq_open(&ctx, name, O_RDONLY, None),
        SyscallResult::Error(E_NOENT)
    );
}

#[test]
fn dispatch_mq_getsetattr_toggles_nonblock_for_mq_fd() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread);
    let name = b"tx-mq-nonblock\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 3,
        mq_msgsize: 24,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR | O_NONBLOCK, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FCNTL, [fd as u64, F_GETFL_CMD as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return((O_RDWR | O_NONBLOCK) as i64)
    );

    let new_attr = MqAttrLayout {
        mq_flags: 0,
        ..MqAttrLayout::default()
    };
    let mut old = MqAttrLayout::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_GETSETATTR,
                [
                    fd as u64,
                    &new_attr as *const MqAttrLayout as u64,
                    &mut old as *mut MqAttrLayout as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(old.mq_flags, O_NONBLOCK as i64);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FCNTL, [fd as u64, F_GETFL_CMD as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(O_RDWR as i64)
    );
}

#[test]
fn dispatch_mq_validates_buffer_sizes_access_and_notify_shape() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread);
    let attr = MqAttrLayout {
        mq_maxmsg: 2,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let ro_name = b"tx-mq-ro\0";
    let ro_fd = match mq_open(&ctx, ro_name, O_CREAT | O_RDONLY, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    let msg = *b"too-long!";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [ro_fd as u64, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_BADF)
    );

    let wo_name = b"tx-mq-wo\0";
    let wo_fd = match mq_open(&ctx, wo_name, O_CREAT | O_WRONLY, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDRECEIVE,
                [wo_fd as u64, msg.as_ptr() as u64, 4, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_BADF)
    );

    let rw_name = b"tx-mq-validate\0";
    let rw_fd = match mq_open(&ctx, rw_name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [rw_fd as u64, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_MSGSIZE)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [
                    rw_fd as u64,
                    msg.as_ptr() as u64,
                    attr.mq_msgsize as u64,
                    MQ_PRIO_MAX,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDRECEIVE,
                [rw_fd as u64, msg.as_ptr() as u64, 4, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_MSGSIZE)
    );

    let sev = TestSigeventPrefix {
        sigval: 0,
        sigev_signo: 10,
        sigev_notify: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_NOTIFY,
                [
                    rw_fd as u64,
                    &sev as *const TestSigeventPrefix as u64,
                    0,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_NOTIFY,
                [
                    rw_fd as u64,
                    &sev as *const TestSigeventPrefix as u64,
                    0,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(16)
    );

    let bad_sev = TestSigeventPrefix {
        sigval: 0,
        sigev_signo: 65,
        sigev_notify: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_NOTIFY,
                [
                    rw_fd as u64,
                    &bad_sev as *const TestSigeventPrefix as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_MQ_NOTIFY, [rw_fd as u64, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let thread_sev = TestSigeventPrefix {
        sigval: 0,
        sigev_signo: 0,
        sigev_notify: 2,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_NOTIFY,
                [
                    rw_fd as u64,
                    &thread_sev as *const TestSigeventPrefix as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
}

#[test]
fn dispatch_mq_notify_sigev_signal_delivers_one_shot_on_empty_to_nonempty_edge() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap.clone(), thread.clone());
    let name = b"tx-mq-notify-signal\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 4,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };

    let sigusr1 = Signum::new(10).expect("SIGUSR1");
    let handler = [0xCAFE_F00D_u64, 0, 0, 0];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_RT_SIGACTION, [10, handler.as_ptr() as u64, 0, 8, 0, 0],),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mailbox = alloc::sync::Arc::new(TaskMailbox::new());
    thread
        .payload_cap()
        .expect("leader has payload")
        .bind_mailbox(alloc::sync::Arc::downgrade(&mailbox));

    let sev = TestSigeventPrefix {
        sigval: 0,
        sigev_signo: 10,
        sigev_notify: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_NOTIFY,
                [
                    fd as u64,
                    &sev as *const TestSigeventPrefix as u64,
                    0,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let first = *b"one";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [
                    fd as u64,
                    first.as_ptr() as u64,
                    first.len() as u64,
                    1,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert!(
        thread
            .payload_cap()
            .expect("leader remains live")
            .pending()
            .is_pending(sigusr1),
        "mq_notify should post the registered SIGUSR1"
    );
    assert_eq!(
        mailbox.poll(),
        Some(MailboxEvent::SignalDelivered {
            signum: 10,
            routing: SignalRouting::ProcessDirected,
        })
    );
    assert!(
        mailbox.is_empty(),
        "mq_notify delivery is one-shot; only the empty-to-nonempty edge fires"
    );

    let second = *b"two";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [
                    fd as u64,
                    second.as_ptr() as u64,
                    second.len() as u64,
                    2,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(
        mailbox.is_empty(),
        "later sends without re-registration must not duplicate notification"
    );
}

#[test]
fn dispatch_mq_notify_registration_is_queue_wide_across_descriptors() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap.clone(), thread.clone());
    let name = b"tx-mq-notify-cross-fd\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 4,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let notify_fd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open notify fd failed: {other:?}"),
    };
    let send_fd = match mq_open(&ctx, name, O_RDWR, None) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open send fd failed: {other:?}"),
    };

    let sigusr1 = Signum::new(10).expect("SIGUSR1");
    let handler = [0xCAFE_F00D_u64, 0, 0, 0];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_RT_SIGACTION, [10, handler.as_ptr() as u64, 0, 8, 0, 0],),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    let mailbox = alloc::sync::Arc::new(TaskMailbox::new());
    thread
        .payload_cap()
        .expect("leader has payload")
        .bind_mailbox(alloc::sync::Arc::downgrade(&mailbox));

    let sev = TestSigeventPrefix {
        sigval: 0,
        sigev_signo: 10,
        sigev_notify: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_NOTIFY,
                [
                    notify_fd as u64,
                    &sev as *const TestSigeventPrefix as u64,
                    0,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let msg = *b"xfd";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [
                    send_fd as u64,
                    msg.as_ptr() as u64,
                    msg.len() as u64,
                    3,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert!(
        thread
            .payload_cap()
            .expect("leader remains live")
            .pending()
            .is_pending(sigusr1),
        "mq_notify registration must live on the queue, not the registering descriptor"
    );
    assert_eq!(
        mailbox.poll(),
        Some(MailboxEvent::SignalDelivered {
            signum: 10,
            routing: SignalRouting::ProcessDirected,
        })
    );
    assert!(mailbox.is_empty());
}

#[test]
fn dispatch_mq_notify_does_not_fire_while_blocked_receiver_consumes_message() {
    let (_setup, proc_cap, thread) = mq_setup();
    let mailbox = alloc::sync::Arc::new(TaskMailbox::new());
    let ctx = make_ctx(proc_cap.clone(), thread.clone()).with_mailbox(mailbox.clone());
    let name = b"tx-mq-notify-blocked-recv\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 4,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };

    let sigusr1 = Signum::new(10).expect("SIGUSR1");
    let handler = [0xCAFE_F00D_u64, 0, 0, 0];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_RT_SIGACTION, [10, handler.as_ptr() as u64, 0, 8, 0, 0],),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    thread
        .payload_cap()
        .expect("leader has payload")
        .bind_mailbox(alloc::sync::Arc::downgrade(&mailbox));

    let sev = TestSigeventPrefix {
        sigval: 0,
        sigev_signo: 10,
        sigev_notify: 0,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_NOTIFY,
                [
                    fd as u64,
                    &sev as *const TestSigeventPrefix as u64,
                    0,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let mut out = [0u8; 8];
    let recv_req = SyscallRequest::new(
        NR_MQ_TIMEDRECEIVE,
        [
            fd as u64,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            0,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let recv_fut = dispatch::<ShimsTestPmap>(recv_req, &ctx);
    let mut pinned_recv = Box::pin(recv_fut);
    assert!(
        matches!(pinned_recv.as_mut().poll(&mut cx), Poll::Pending),
        "blocking receiver must park before the first send"
    );

    let first = *b"one";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [
                    fd as u64,
                    first.as_ptr() as u64,
                    first.len() as u64,
                    1,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(
        !thread
            .payload_cap()
            .expect("leader remains live")
            .pending()
            .is_pending(sigusr1),
        "mq_notify must not fire when a blocked receiver consumes the message"
    );

    let mut received_first = false;
    for _ in 0..256 {
        if let Poll::Ready(result) = pinned_recv.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(first.len() as i64));
            assert_eq!(&out[..first.len()], &first);
            received_first = true;
            break;
        }
    }
    assert!(
        received_first,
        "blocked mq_receive should consume the first send"
    );
    assert!(mailbox.is_empty());

    let second = *b"two";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [
                    fd as u64,
                    second.as_ptr() as u64,
                    second.len() as u64,
                    2,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(
        thread
            .payload_cap()
            .expect("leader remains live")
            .pending()
            .is_pending(sigusr1),
        "registration should remain armed and fire on the next unconsumed arrival"
    );
    assert_eq!(
        mailbox.poll(),
        Some(MailboxEvent::SignalDelivered {
            signum: 10,
            routing: SignalRouting::ProcessDirected,
        })
    );
}

#[test]
fn dispatch_mq_raw_read_write_fail_cleanly_and_maxmsg_is_enforced() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread);
    let name = b"tx-mq-rawio\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 1,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR | O_NONBLOCK, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    let msg = *b"one";

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_READ, [fd as u64, msg.as_ptr() as u64, 1, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_WRITE, [fd as u64, msg.as_ptr() as u64, 1, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [fd as u64, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [fd as u64, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_AGAIN)
    );
}

#[test]
fn dispatch_mq_blocking_receive_parks_until_send_wakes_queue() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread).with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()));
    let name = b"tx-mq-blocking-recv\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 2,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };

    let mut out = [0u8; 8];
    let mut prio = 0u32;
    let req = SyscallRequest::new(
        NR_MQ_TIMEDRECEIVE,
        [
            fd as u64,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            &mut prio as *mut u32 as u64,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    let first = pinned.as_mut().poll(&mut cx);
    assert!(
        matches!(first, Poll::Pending),
        "blocking mq_receive on an empty queue should park; got {first:?}"
    );

    let msg = *b"wake";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [fd as u64, msg.as_ptr() as u64, msg.len() as u64, 4, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    for _ in 0..256 {
        if let Poll::Ready(result) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(msg.len() as i64));
            assert_eq!(&out[..msg.len()], &msg);
            assert_eq!(prio, 4);
            return;
        }
    }
    panic!("blocking mq_receive did not resolve after mq_timedsend woke the queue");
}

#[test]
fn dispatch_mq_timedreceive_wakes_for_process_timer_signal_deadline() {
    let (_setup, proc_cap, thread) = mq_setup();
    let timer_queue = tx_reactor::timer::TimerQueue::new();
    tx_subsystems::timer_sleep::install_timer_queue(timer_queue.clone());
    let ctx = make_ctx(proc_cap.clone(), thread.clone())
        .with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()));
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

    let name = b"tx-mq-process-timer-recv\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 2,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };

    let mut out = [0u8; 8];
    let req = SyscallRequest::new(
        NR_MQ_TIMEDRECEIVE,
        [
            fd as u64,
            out.as_mut_ptr() as u64,
            out.len() as u64,
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
        "process timer deadline should wake blocked mq_timedreceive"
    );

    let payload = thread.payload_cap().expect("live thread");
    assert!(
        payload.pending().is_pending(sigalrm),
        "mq process-timer wake should publish SIGALRM"
    );
}

#[test]
fn dispatch_mq_blocking_send_parks_until_receive_makes_space() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread).with_mailbox(alloc::sync::Arc::new(TaskMailbox::new()));
    let name = b"tx-mq-blocking-send\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 1,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let fd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };
    let first_msg = *b"first";
    let second_msg = *b"second";

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [
                    fd as u64,
                    first_msg.as_ptr() as u64,
                    first_msg.len() as u64,
                    1,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    let send_req = SyscallRequest::new(
        NR_MQ_TIMEDSEND,
        [
            fd as u64,
            second_msg.as_ptr() as u64,
            second_msg.len() as u64,
            2,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let send_fut = dispatch::<ShimsTestPmap>(send_req, &ctx);
    let mut pinned_send = Box::pin(send_fut);

    let first_poll = pinned_send.as_mut().poll(&mut cx);
    assert!(
        matches!(first_poll, Poll::Pending),
        "blocking mq_send on a full queue should park; got {first_poll:?}"
    );

    let mut out = [0u8; 8];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDRECEIVE,
                [
                    fd as u64,
                    out.as_mut_ptr() as u64,
                    out.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(first_msg.len() as i64)
    );
    assert_eq!(&out[..first_msg.len()], &first_msg);

    for _ in 0..256 {
        if let Poll::Ready(result) = pinned_send.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(0));
            return;
        }
    }
    panic!("blocking mq_send did not resolve after mq_receive made space");
}

#[test]
fn dispatch_epoll_reports_posix_mq_readable_and_writable_edges() {
    let (_setup, proc_cap, thread) = mq_setup();
    let ctx = make_ctx(proc_cap, thread);
    let epfd = create_epoll(&ctx);
    let name = b"tx-mq-epoll\0";
    let attr = MqAttrLayout {
        mq_maxmsg: 1,
        mq_msgsize: 8,
        ..MqAttrLayout::default()
    };
    let mqfd = match mq_open(&ctx, name, O_CREAT | O_RDWR, Some(&attr)) {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("mq_open failed: {other:?}"),
    };

    let mut event = TestEpollEvent {
        events: EPOLLIN | EPOLLOUT,
        _padding: 0,
        data: 0x6d71,
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_CTL,
                [
                    epfd as u64,
                    EPOLL_CTL_ADD as u64,
                    mqfd as u64,
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
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_PWAIT,
                [epfd as u64, out.as_mut_ptr() as u64, 1, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(1)
    );
    assert_eq!(out[0].events, EPOLLOUT);
    assert_eq!(out[0].data, event.data);

    let msg = *b"ready";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MQ_TIMEDSEND,
                [mqfd as u64, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    out[0] = TestEpollEvent::default();
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_EPOLL_PWAIT,
                [epfd as u64, out.as_mut_ptr() as u64, 1, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(1)
    );
    assert_eq!(out[0].events, EPOLLIN);
    assert_eq!(out[0].data, event.data);
}
