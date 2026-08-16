// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use tx_subsystems::process::bootstrap_init_process;

use crate::linux_syscall::{
    F_GETFL, F_GETPIPE_SZ, F_SETFL, F_SETPIPE_SZ, NR_CLOSE, NR_FCNTL, NR_PIPE2, NR_PPOLL,
    NR_PSELECT6, NR_READ, NR_SOCKETPAIR, NR_WRITE, NR_WRITEV, O_CLOEXEC, O_DIRECT, O_NONBLOCK,
    O_WRONLY, TTY_WRITE_MAX_INLINE,
};
use std::sync::Arc;
use tx_services::time::{
    DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle, TimeError, TimerRole, TimerTarget,
    TimerToken,
};
use tx_substrate::wake::{MailboxEvent, TaskMailbox};

#[derive(Default)]
struct YieldingWritevPageBacking {
    complete_first_page: bool,
}

impl tx_subsystems::page_backed::FsPageBacking for YieldingWritevPageBacking {
    fn fetch_page(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        offset: u64,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> StepOutcome<tx_subsystems::page_backed::Frame, step_engine::NoProgress> {
        if self.complete_first_page && offset == 0 {
            let owned = crate::adapter::step_engine::page_allocator::reserve_frame(
                crate::adapter::step_engine::page_allocator::ZeroPolicy::Zeroed,
            )
            .expect("reserve first writev destination page")
            .commit();
            return StepOutcome::Done(tx_subsystems::page_backed::Frame::from_owned(owned));
        }
        StepOutcome::yield_on_wait_source(step_engine::NoProgress, 0x5756, 1)
    }

    fn flush_page(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _offset: u64,
        _frame: &tx_subsystems::page_backed::Frame,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn truncate(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _new_size: u64,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::done(())
    }

    fn fsync_file(
        &self,
        _fs_object_id: tx_subsystems::vfs::FsObjectId,
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> StepOutcome<(), step_engine::NoProgress> {
        StepOutcome::done(())
    }
}

fn writev_pagebacked_file(
    append: bool,
    initial_size: u64,
    complete_first_page: bool,
) -> Cap<OpenFile> {
    use tx_subsystems::mount::{DevId, MountOptions, MountPayload, MountPayloadPin, SourceLabel};
    use tx_subsystems::page_backed::PageContainer;
    use tx_subsystems::vfs::FsObjectId;

    let fs_ops = Arc::new(tx_fs::tmpfs::Tmpfs::new());
    let page_backing = Arc::new(YieldingWritevPageBacking {
        complete_first_page,
    });
    let mount = MountPayload::new_cap(
        fs_ops,
        page_backing,
        None,
        DevId::new(0x5756),
        MountOptions::default(),
        "yielding-writev-test",
        SourceLabel::Static("yielding-writev-test"),
    )
    .expect("yielding writev mount payload");
    let guard = guard();
    let (pc, _) = PageContainer::find_or_create_file_cap(
        MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(mount)),
        FsObjectId::new(0x5756),
        initial_size,
        2,
        &guard,
    )
    .expect("yielding writev page container");
    let rnode = RNode::new_cap(
        FsObjectId::new(0x5756),
        InodeMeta::new(InodeKind::Regular, 0o100644),
        RNodeBacking::PageBacked { pc },
    )
    .expect("yielding writev rnode");
    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("yielding writev open file")
}

fn yielding_writev_pagebacked_file(append: bool) -> Cap<OpenFile> {
    writev_pagebacked_file(append, if append { 7 } else { 0 }, false)
}

fn partially_yielding_append_writev_file() -> Cap<OpenFile> {
    writev_pagebacked_file(true, tx_subsystems::vm::USER_PAGE_SIZE as u64 - 1, true)
}

fn map_writev_test_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, bytes: &[u8]) -> u64 {
    let range = tx_subsystems::vm::UserRange::new_aligned(
        tx_subsystems::vm::UserVirtAddr(uaddr),
        tx_subsystems::vm::USER_PAGE_SIZE,
    )
    .expect("aligned writev test range");
    let request = tx_subsystems::vm::VmMapRequest::fixed(
        range,
        tx_subsystems::vm::MapPlacement::FixedReplace,
        tx_subsystems::vm::Prot::READ_WRITE,
        tx_subsystems::vm::VmEntryFlags::PRIVATE,
        tx_subsystems::vm::VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(request).expect("map writev test bytes");
    let guard = guard();
    assert_eq!(
        ctx.aspace
            .copy_to_user(tx_hal::UserPtr::<u8>::new(uaddr), bytes, &guard),
        StepOutcome::Done(bytes.len())
    );
    uaddr as u64
}

const E_INVAL: i32 = 22;
const E_NOSYS: i32 = 38;
const E_BUSY: i32 = 16;
const E_PERM: i32 = 1;
const E_AGAIN: i32 = 11;
const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const POLLIN: i16 = 0x0001;

#[derive(Default)]
struct FdDeadlineDomain {
    registration: std::sync::Mutex<Option<FdDeadlineRegistration>>,
}

struct FdDeadlineRegistration {
    role: TimerRole,
    mailbox: std::sync::Weak<TaskMailbox>,
    token: TimerToken,
}

impl FdDeadlineDomain {
    fn take_registration(&self) -> FdDeadlineRegistration {
        self.registration
            .lock()
            .unwrap()
            .take()
            .expect("finite fd wait timeout should register a deadline")
    }
}

impl DeadlineDomain for FdDeadlineDomain {
    fn register_deadline(
        &self,
        _deadline_ns: DeadlineNs,
        role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        let TimerTarget::TaskMailbox(mailbox) = target else {
            panic!("fd wait timeout must use a task-mailbox deadline");
        };
        let token = TimerToken::new(0xFD01);
        *self.registration.lock().unwrap() = Some(FdDeadlineRegistration {
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

fn pipe2_setup() -> TestSetup {
    setup()
}

fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init for pipe2 tests");
    let thread = process.nth_thread(0).expect("leader thread");
    (process, thread)
}

fn fdset_with(fd: u32) -> [u8; 128] {
    let mut set = [0u8; 128];
    set[(fd / 8) as usize] |= 1u8 << (fd % 8);
    set
}

fn fdset_has(set: &[u8; 128], fd: u32) -> bool {
    (set[(fd / 8) as usize] & (1u8 << (fd % 8))) != 0
}

fn dispatch_socketpair(ctx: &SyscallCtx<'_>) -> [u32; 2] {
    let mut sv: [u32; 2] = [u32::MAX, u32::MAX];
    let req = SyscallRequest::new(
        NR_SOCKETPAIR,
        [AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr() as u64, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, ctx)),
        SyscallResult::Return(0)
    );
    assert_ne!(sv[0], u32::MAX, "first socket fd written");
    assert_ne!(sv[1], u32::MAX, "second socket fd written");
    assert_ne!(sv[0], sv[1], "socketpair fds distinct");
    sv
}

/// `pipe2(uaddr, 0)` succeeds, writes `(reader_fd, writer_fd)` to
/// userspace, and installs both fds in the table.
#[test]
fn dispatch_pipe2_allocates_two_fds_and_writes_pair_to_userspace() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

    let req = SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let reader_fd = pipefd[0];
    let writer_fd = pipefd[1];
    assert_ne!(reader_fd, u32::MAX, "reader_fd written");
    assert_ne!(writer_fd, u32::MAX, "writer_fd written");
    assert_ne!(reader_fd, writer_fd, "fds distinct");
    assert!(proc_cap.fd(reader_fd).is_some(), "reader fd installed");
    assert!(proc_cap.fd(writer_fd).is_some(), "writer fd installed");
    // Default flags: cloexec clear, nonblocking clear.
    assert!(!proc_cap.fd_cloexec(reader_fd));
    assert!(!proc_cap.fd_cloexec(writer_fd));
}

#[test]
fn dispatch_socketpair_stream_is_bidirectional_for_read_write() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let sv = dispatch_socketpair(&ctx);

    let ping = *b"ping";
    let write_req = SyscallRequest::new(
        NR_WRITE,
        [
            sv[0] as u64,
            ping.as_ptr() as u64,
            ping.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(write_req, &ctx)),
        SyscallResult::Return(ping.len() as i64)
    );
    let mut read_buf = [0u8; 4];
    let read_req = SyscallRequest::new(
        NR_READ,
        [
            sv[1] as u64,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(read_req, &ctx)),
        SyscallResult::Return(read_buf.len() as i64)
    );
    assert_eq!(&read_buf, b"ping");

    let pong = *b"pong";
    let write_req = SyscallRequest::new(
        NR_WRITE,
        [
            sv[1] as u64,
            pong.as_ptr() as u64,
            pong.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(write_req, &ctx)),
        SyscallResult::Return(pong.len() as i64)
    );
    let mut read_buf = [0u8; 4];
    let read_req = SyscallRequest::new(
        NR_READ,
        [
            sv[0] as u64,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(read_req, &ctx)),
        SyscallResult::Return(read_buf.len() as i64)
    );
    assert_eq!(&read_buf, b"pong");
}

#[test]
fn dispatch_socketpair_close_peer_publishes_read_eof() {
    let _setup = pipe2_setup();
    let (_proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(_proc_cap.clone(), thread);
    let sv = dispatch_socketpair(&ctx);

    let close_req = SyscallRequest::new(NR_CLOSE, [sv[1] as u64, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(close_req, &ctx)),
        SyscallResult::Return(0)
    );

    let mut read_buf = [0u8; 4];
    let read_req = SyscallRequest::new(
        NR_READ,
        [
            sv[0] as u64,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(read_req, &ctx)),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_ppoll_zero_timeout_clears_unready_socketpair_reader() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let sv = dispatch_socketpair(&ctx);

    let mut pollfd = [0u8; 8];
    pollfd[0..4].copy_from_slice(&(sv[0] as i32).to_le_bytes());
    pollfd[4..6].copy_from_slice(&POLLIN.to_le_bytes());
    let timeout = [0u64, 0u64];
    let req = SyscallRequest::new(
        NR_PPOLL,
        [
            pollfd.as_mut_ptr() as u64,
            1,
            timeout.as_ptr() as u64,
            0,
            0,
            0,
        ],
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );
    assert_eq!(
        i16::from_le_bytes(pollfd[6..8].try_into().unwrap()),
        0,
        "unready socketpair reader should report no revents"
    );
}

#[test]
fn dispatch_ppoll_positive_timeout_uses_unified_timer_registry() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let mailbox = Arc::new(TaskMailbox::new());
    let domain = Arc::new(FdDeadlineDomain::default());
    let ctx = make_ctx(proc_cap.clone(), thread)
        .with_mailbox(Arc::clone(&mailbox))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let sv = dispatch_socketpair(&ctx);

    let mut pollfd = [0u8; 8];
    pollfd[0..4].copy_from_slice(&(sv[0] as i32).to_le_bytes());
    pollfd[4..6].copy_from_slice(&POLLIN.to_le_bytes());
    let timeout = [0u64, 1_000_000u64];
    let req = SyscallRequest::new(
        NR_PPOLL,
        [
            pollfd.as_mut_ptr() as u64,
            1,
            timeout.as_ptr() as u64,
            0,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    for _ in 0..4 {
        let poll = pinned.as_mut().poll(&mut cx);
        assert!(
            matches!(poll, Poll::Pending),
            "ppoll with unreadable fd and finite timeout should park before expiry; got {poll:?}"
        );
    }
    let registration = domain.take_registration();
    assert_eq!(registration.role, TimerRole::DeadlineAbort);
    let target_mailbox = registration
        .mailbox
        .upgrade()
        .expect("deadline timer should retain the syscall task mailbox");
    assert!(target_mailbox.post(MailboxEvent::TimerFired {
        token: registration.token,
    }));

    for _ in 0..16 {
        if let Poll::Ready(result) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(0));
            assert_eq!(
                i16::from_le_bytes(pollfd[6..8].try_into().unwrap()),
                0,
                "timeout should leave the unreadable fd without revents"
            );
            return;
        }
    }
    panic!("ppoll did not resolve after unified timer expiry");
}

#[test]
fn dispatch_socketpair_empty_blocking_read_returns_eagain_without_rnode_panic() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let sv = dispatch_socketpair(&ctx);
    let mut read_buf = [0u8; 4];

    let req = SyscallRequest::new(
        NR_READ,
        [
            sv[0] as u64,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
            0,
            0,
            0,
        ],
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Error(E_AGAIN)
    );
}

#[test]
fn dispatch_socketpair_blocking_read_parks_until_peer_write() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let mailbox = Arc::new(TaskMailbox::new());
    let ctx = make_ctx(proc_cap.clone(), thread).with_mailbox(mailbox);
    let sv = dispatch_socketpair(&ctx);
    let mut read_buf = [0u8; 4];

    let read_req = SyscallRequest::new(
        NR_READ,
        [
            sv[0] as u64,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
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
        "blocking socketpair read should park before data arrives"
    );

    let ping = *b"ping";
    let write_req = SyscallRequest::new(
        NR_WRITE,
        [
            sv[1] as u64,
            ping.as_ptr() as u64,
            ping.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(write_req, &ctx)),
        SyscallResult::Return(ping.len() as i64)
    );
    assert_eq!(block_on(read), SyscallResult::Return(read_buf.len() as i64));
    assert_eq!(&read_buf, b"ping");
}

#[test]
fn dispatch_socketpair_blocking_write_parks_until_peer_read() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let mailbox = Arc::new(TaskMailbox::new());
    let ctx = make_ctx(proc_cap.clone(), thread).with_mailbox(mailbox);
    let sv = dispatch_socketpair(&ctx);
    let page = [b'x'; TTY_WRITE_MAX_INLINE];

    for _ in 0..tx_subsystems::pipe::PIPE_DEF_BUFFERS {
        let write_req = SyscallRequest::new(
            NR_WRITE,
            [
                sv[0] as u64,
                page.as_ptr() as u64,
                page.len() as u64,
                0,
                0,
                0,
            ],
        );
        assert_eq!(
            block_on(dispatch::<ShimsTestPmap>(write_req, &ctx)),
            SyscallResult::Return(page.len() as i64)
        );
    }

    let extra = *b"more";
    let write_req = SyscallRequest::new(
        NR_WRITE,
        [
            sv[0] as u64,
            extra.as_ptr() as u64,
            extra.len() as u64,
            0,
            0,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut write = Box::pin(dispatch::<ShimsTestPmap>(write_req, &ctx));
    assert!(
        matches!(write.as_mut().poll(&mut cx), Poll::Pending),
        "blocking socketpair write should park while the peer receive buffer is full"
    );

    let mut read_buf = [0u8; TTY_WRITE_MAX_INLINE];
    let read_req = SyscallRequest::new(
        NR_READ,
        [
            sv[1] as u64,
            read_buf.as_mut_ptr() as u64,
            read_buf.len() as u64,
            0,
            0,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(read_req, &ctx)),
        SyscallResult::Return(read_buf.len() as i64)
    );
    assert_eq!(block_on(write), SyscallResult::Return(extra.len() as i64));
}

#[test]
fn writev_pagebacked_prefilter_rejects_socketpair_without_rnode_panic() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let sv = dispatch_socketpair(&ctx);

    // Socketpair has no RNode/PageBacked backing, so the narrow prefilter must
    // reject it before inspecting any iovec payload.
    let iov = [0u64, 0u64];
    let req = SyscallRequest::new(NR_WRITEV, [sv[0] as u64, iov.as_ptr() as u64, 1, 0, 0, 0]);

    assert_eq!(
        crate::linux_syscall::dispatch_writev_pagebacked_oneshot(&req, &ctx),
        None,
        "socketpair writev must fall through to the generic writev path"
    );
}

#[test]
fn writev_pagebacked_yield_falls_through_to_async_dispatch() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let fd = 91;
    assert!(
        proc_cap
            .install_fd(fd, yielding_writev_pagebacked_file(false))
            .is_none(),
        "test fd is initially vacant"
    );

    let payload = map_writev_test_bytes(&ctx, 0x5500_0000, b"wv");
    let iov = [payload, 2u64];
    let iov_bytes = unsafe {
        core::slice::from_raw_parts(iov.as_ptr().cast::<u8>(), core::mem::size_of_val(&iov))
    };
    let iov_ptr = map_writev_test_bytes(&ctx, 0x5500_1000, iov_bytes);
    let req = SyscallRequest::new(NR_WRITEV, [fd as u64, iov_ptr, 1, 0, 0, 0]);

    assert_eq!(
        crate::linux_syscall::dispatch_writev_pagebacked_oneshot(&req, &ctx),
        None,
        "zero-progress PageBacked Yield must enter the waiting async writev path"
    );
}

#[test]
fn writev_pagebacked_append_yield_preserves_offset_without_progress() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let fd = 92;
    let file = yielding_writev_pagebacked_file(true);
    file.set_offset(1);
    assert!(proc_cap.install_fd(fd, file.clone()).is_none());

    let payload = map_writev_test_bytes(&ctx, 0x5500_2000, b"wv");
    let iov = [payload, 2u64];
    let iov_bytes = unsafe {
        core::slice::from_raw_parts(iov.as_ptr().cast::<u8>(), core::mem::size_of_val(&iov))
    };
    let iov_ptr = map_writev_test_bytes(&ctx, 0x5500_3000, iov_bytes);
    let req = SyscallRequest::new(NR_WRITEV, [fd as u64, iov_ptr, 1, 0, 0, 0]);

    assert_eq!(
        crate::linux_syscall::dispatch_writev_pagebacked_oneshot(&req, &ctx),
        None
    );
    assert_eq!(
        file.offset(),
        1,
        "zero-progress append Yield must leave the shared offset unchanged"
    );
}

#[test]
fn writev_pagebacked_partial_yield_returns_committed_prefix_once() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let fd = 93;
    let file = partially_yielding_append_writev_file();
    file.set_offset(3);
    assert!(proc_cap.install_fd(fd, file.clone()).is_none());

    let payload = map_writev_test_bytes(&ctx, 0x5500_4000, b"xy");
    let iov = [payload, 2u64];
    let iov_bytes = unsafe {
        core::slice::from_raw_parts(iov.as_ptr().cast::<u8>(), core::mem::size_of_val(&iov))
    };
    let iov_ptr = map_writev_test_bytes(&ctx, 0x5500_5000, iov_bytes);
    let req = SyscallRequest::new(NR_WRITEV, [fd as u64, iov_ptr, 1, 0, 0, 0]);

    assert_eq!(
        crate::linux_syscall::dispatch_writev_pagebacked_oneshot(&req, &ctx),
        Some(SyscallResult::Return(1)),
        "partial Yield must return the committed prefix instead of retrying it"
    );
    assert_eq!(
        file.offset(),
        tx_subsystems::vm::USER_PAGE_SIZE as u64,
        "append offset advances by exactly the committed prefix"
    );
}

#[test]
fn dispatch_pselect6_zero_timeout_clears_unready_pipe_reader() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];
    let req = SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );

    let mut readfds = fdset_with(pipefd[0]);
    let timeout = [0u64, 0u64];
    let req = SyscallRequest::new(
        NR_PSELECT6,
        [
            pipefd[0] as u64 + 1,
            readfds.as_mut_ptr() as u64,
            0,
            0,
            timeout.as_ptr() as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );
    assert!(
        !fdset_has(&readfds, pipefd[0]),
        "unready reader bit cleared"
    );
}

#[test]
fn dispatch_pselect6_positive_timeout_uses_unified_timer_registry() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let mailbox = Arc::new(TaskMailbox::new());
    let domain = Arc::new(FdDeadlineDomain::default());
    let ctx = make_ctx(proc_cap.clone(), thread)
        .with_mailbox(Arc::clone(&mailbox))
        .with_timer_registrar(DeadlineRegistrarHandle::from_domain(domain.clone()));
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];
    let req = SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );

    let mut readfds = fdset_with(pipefd[0]);
    let timeout = [0u64, 1_000_000u64];
    let req = SyscallRequest::new(
        NR_PSELECT6,
        [
            pipefd[0] as u64 + 1,
            readfds.as_mut_ptr() as u64,
            0,
            0,
            timeout.as_ptr() as u64,
            0,
        ],
    );
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let fut = dispatch::<ShimsTestPmap>(req, &ctx);
    let mut pinned = Box::pin(fut);

    for _ in 0..4 {
        let poll = pinned.as_mut().poll(&mut cx);
        assert!(
            matches!(poll, Poll::Pending),
            "pselect6 with unreadable fd and finite timeout should park before expiry; got {poll:?}"
        );
    }
    let registration = domain.take_registration();
    assert_eq!(registration.role, TimerRole::DeadlineAbort);
    let target_mailbox = registration
        .mailbox
        .upgrade()
        .expect("deadline timer should retain the syscall task mailbox");
    assert!(target_mailbox.post(MailboxEvent::TimerFired {
        token: registration.token,
    }));

    for _ in 0..16 {
        if let Poll::Ready(result) = pinned.as_mut().poll(&mut cx) {
            assert_eq!(result, SyscallResult::Return(0));
            assert!(
                !fdset_has(&readfds, pipefd[0]),
                "timeout should clear the unready reader bit"
            );
            return;
        }
    }
    panic!("pselect6 did not resolve after unified timer expiry");
}

#[test]
fn dispatch_pselect6_reports_pipe_reader_after_write() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];
    let req = SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );

    let byte = [b'x'];
    let req = SyscallRequest::new(
        NR_WRITE,
        [pipefd[1] as u64, byte.as_ptr() as u64, 1, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(1)
    );

    let mut readfds = fdset_with(pipefd[0]);
    let timeout = [0u64, 0u64];
    let req = SyscallRequest::new(
        NR_PSELECT6,
        [
            pipefd[0] as u64 + 1,
            readfds.as_mut_ptr() as u64,
            0,
            0,
            timeout.as_ptr() as u64,
            0,
        ],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(1)
    );
    assert!(
        fdset_has(&readfds, pipefd[0]),
        "ready reader bit remains set"
    );
}

/// `pipe2(uaddr, O_CLOEXEC)` sets the cloexec bit on both fds.
#[test]
fn dispatch_pipe2_with_o_cloexec_sets_cloexec_on_both_fds() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

    let req = SyscallRequest::new(
        NR_PIPE2,
        [pipefd.as_mut_ptr() as u64, O_CLOEXEC as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    assert!(proc_cap.fd_cloexec(pipefd[0]), "reader cloexec set");
    assert!(proc_cap.fd_cloexec(pipefd[1]), "writer cloexec set");
}

/// `pipe2(uaddr, O_NONBLOCK)` threads through to OpenFile.flags
/// on both ends.
#[test]
fn dispatch_pipe2_with_o_nonblock_sets_nonblocking_on_both_openfiles() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

    let req = SyscallRequest::new(
        NR_PIPE2,
        [pipefd.as_mut_ptr() as u64, O_NONBLOCK as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(0));
    let reader = proc_cap.fd(pipefd[0]).expect("reader fd installed");
    let writer = proc_cap.fd(pipefd[1]).expect("writer fd installed");
    assert!(reader.flags().nonblocking, "reader nonblocking");
    assert!(writer.flags().nonblocking, "writer nonblocking");
}

/// `pipe2(uaddr, O_DIRECT)` creates a packet-mode pipe. A short
/// read consumes the whole packet, discarding the unread tail, so the
/// next read starts at the next write packet.
#[test]
fn dispatch_pipe2_with_o_direct_uses_packet_mode() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

    let req = SyscallRequest::new(
        NR_PIPE2,
        [pipefd.as_mut_ptr() as u64, O_DIRECT as u64, 0, 0, 0, 0],
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(req, &ctx)),
        SyscallResult::Return(0)
    );
    let reader = proc_cap.fd(pipefd[0]).expect("reader fd installed");
    let writer = proc_cap.fd(pipefd[1]).expect("writer fd installed");
    assert!(reader.flags().packet, "reader packet mode");
    assert!(writer.flags().packet, "writer packet mode");

    let first = *b"abcdef";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_WRITE,
                [
                    pipefd[1] as u64,
                    first.as_ptr() as u64,
                    first.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(first.len() as i64)
    );
    let second = *b"XY";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_WRITE,
                [
                    pipefd[1] as u64,
                    second.as_ptr() as u64,
                    second.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(second.len() as i64)
    );

    let mut small = [0u8; 3];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_READ,
                [
                    pipefd[0] as u64,
                    small.as_mut_ptr() as u64,
                    small.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(small.len() as i64)
    );
    assert_eq!(&small, b"abc");

    let mut next = [0u8; 4];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_READ,
                [
                    pipefd[0] as u64,
                    next.as_mut_ptr() as u64,
                    next.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(second.len() as i64)
    );
    assert_eq!(&next[..second.len()], b"XY");
}

#[test]
fn dispatch_fcntl_setfl_toggles_pipe_packet_mode() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [pipefd[1] as u64, F_SETFL as u64, O_DIRECT as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    let writer = proc_cap.fd(pipefd[1]).expect("writer fd installed");
    assert!(writer.flags().packet, "writer packet mode set");
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FCNTL, [pipefd[1] as u64, F_GETFL as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return((O_WRONLY | O_DIRECT) as i64)
    );

    let first = *b"abcdef";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_WRITE,
                [
                    pipefd[1] as u64,
                    first.as_ptr() as u64,
                    first.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(first.len() as i64)
    );
    let second = *b"XY";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_WRITE,
                [
                    pipefd[1] as u64,
                    second.as_ptr() as u64,
                    second.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(second.len() as i64)
    );
    let mut small = [0u8; 3];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_READ,
                [
                    pipefd[0] as u64,
                    small.as_mut_ptr() as u64,
                    small.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(small.len() as i64)
    );
    assert_eq!(&small, b"abc");
    let mut next = [0u8; 4];
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_READ,
                [
                    pipefd[0] as u64,
                    next.as_mut_ptr() as u64,
                    next.len() as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(second.len() as i64)
    );
    assert_eq!(&next[..second.len()], b"XY");

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FCNTL, [pipefd[1] as u64, F_SETFL as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(!writer.flags().packet, "writer packet mode cleared");
}

/// `pipe2(uaddr, junk_bits)` returns `-EINVAL` for any
/// unrecognised flag bits.
#[test]
fn dispatch_pipe2_with_junk_flags_returns_neg_einval() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

    // 0x80000000 is well outside the recognised set
    // (O_CLOEXEC | O_NONBLOCK | O_DIRECT).
    let junk: u64 = 0x8000_0000;
    let req = SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, junk, 0, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_fcntl_getpipe_sz_returns_default_pipe_capacity() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_FCNTL,
            [pipefd[0] as u64, F_GETPIPE_SZ as u64, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(
        result,
        SyscallResult::Return((16 * tx_subsystems::vm::USER_PAGE_SIZE) as i64)
    );
}

#[test]
fn dispatch_fcntl_setpipe_sz_rounds_and_rejects_busy_shrink() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_FCNTL,
            [
                pipefd[1] as u64,
                F_SETPIPE_SZ as u64,
                (tx_subsystems::vm::USER_PAGE_SIZE + 1) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(
        result,
        SyscallResult::Return((2 * tx_subsystems::vm::USER_PAGE_SIZE) as i64)
    );

    let fill = alloc::vec![b'x'; tx_subsystems::vm::USER_PAGE_SIZE];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_WRITE,
            [
                pipefd[1] as u64,
                fill.as_ptr() as u64,
                fill.len() as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(fill.len() as i64));
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_WRITE,
            [
                pipefd[1] as u64,
                fill.as_ptr() as u64,
                fill.len() as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(fill.len() as i64));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_FCNTL,
            [
                pipefd[1] as u64,
                F_SETPIPE_SZ as u64,
                tx_subsystems::vm::USER_PAGE_SIZE as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_BUSY));
}

#[test]
fn dispatch_fcntl_setpipe_sz_rejects_v1_limit() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_FCNTL,
            [
                pipefd[0] as u64,
                F_SETPIPE_SZ as u64,
                2 * 1024 * 1024,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_PERM));
}
