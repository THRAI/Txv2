// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::adapter::step_engine::{self as step_engine, guard, page_allocator, reserve_for, sign_for, StepOutcome};
use tx_subsystems::page_backed::{step_truncate, AnonSwapPolicy, PageContainer, PageContainerKind};
use tx_subsystems::pipe::{step_pipe2, PipeFlags};
use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
};

use crate::linux_syscall::{NR_LSEEK, SEEK_CUR, SEEK_END, SEEK_SET};

const E_BADF: i32 = 9;
const E_INVAL: i32 = 22;
const E_SPIPE: i32 = 29;

fn lseek_setup() -> TestSetup {
    let setup = setup();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for fd-ops wave4 tests: {error:?}"),
    }
    setup
}

fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init for lseek tests");
    let thread = process.nth_thread(0).expect("leader thread");
    (process, thread)
}

/// Build a `Cap<OpenFile>` over a fresh anon `PageContainer` of
/// `page_count` pages. The container's visible size is set to
/// `size_bytes` via `step_truncate` so SEEK_END has a stable
/// number to assert against.
fn pagebacked_open_file(page_count: u64, size_bytes: u64) -> Cap<OpenFile> {
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    )
    .expect("page container cap");
    let guard = guard();
    match step_truncate(&pc, size_bytes, &guard) {
        StepOutcome::Done(())
        | StepOutcome::Continue { .. } => {}
        other => panic!("step_truncate({size_bytes}): {other:?}"),
    }
    drop(guard);
    let rnode = {
        let raw = RNode::new(
            FsObjectId::new(8_900),
            InodeMeta::new(InodeKind::Regular, 0o100644),
            RNodeBacking::PageBacked { pc },
        );
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };
    OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
    )
    .expect("open file cap")
}

/// `lseek(fd, 17, SEEK_SET)` returns `17` and persists `17` as
/// the per-fd offset on a freshly-opened PageBacked fd.
#[test]
fn dispatch_lseek_seek_set_returns_new_offset_and_persists_on_openfile() {
    let _setup = lseek_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let file = pagebacked_open_file(2, tx_subsystems::vm::USER_PAGE_SIZE as u64);
    proc_cap.set_fd(7, Some(file));
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_LSEEK, [7, 17, SEEK_SET as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(17));
    let installed = proc_cap.fd(7).expect("fd 7 installed");
    assert_eq!(installed.offset(), 17);
}

/// `lseek(fd, +5, SEEK_CUR)` adds 5 to the current offset.
/// Sequence: lseek(20, SEEK_SET) → lseek(+5, SEEK_CUR). Final
/// value 25.
#[test]
fn dispatch_lseek_seek_cur_adds_to_current_offset() {
    let _setup = lseek_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let file = pagebacked_open_file(2, tx_subsystems::vm::USER_PAGE_SIZE as u64);
    proc_cap.set_fd(7, Some(file));
    let ctx = make_ctx(proc_cap.clone(), thread);

    // First, set to 20 via SEEK_SET.
    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_LSEEK, [7, 20, SEEK_SET as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(20));

    // Now SEEK_CUR(+5) → 25.
    let r2 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_LSEEK, [7, 5, SEEK_CUR as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Return(25));
    assert_eq!(proc_cap.fd(7).expect("fd 7").offset(), 25);
}

/// `lseek(fd, +offset, SEEK_END)` returns `pc.size_bytes() +
/// offset` for a PageBacked fd. Pin the size to 1024 via
/// `step_truncate`; then SEEK_END(+8) → 1032.
#[test]
fn dispatch_lseek_seek_end_returns_size_plus_offset_for_pagebacked_fd() {
    let _setup = lseek_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    // Size = 1024 (well under the 1-page capacity).
    let file = pagebacked_open_file(1, 1024);
    proc_cap.set_fd(7, Some(file));
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_LSEEK, [7, 8, SEEK_END as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Return(1032));
    assert_eq!(proc_cap.fd(7).expect("fd 7").offset(), 1032);
}

/// `lseek(pipe_fd, 0, SEEK_SET)` returns `-ESPIPE`. Pipes are
/// non-seekable per Linux semantics; even SEEK_SET 0 (the
/// "tell me your current offset" no-op) returns ESPIPE.
#[test]
fn dispatch_lseek_on_pipe_fd_returns_neg_espipe() {
    let _setup = lseek_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let (reader_cap, _writer_cap) =
        step_pipe2(PipeFlags::default()).expect("pipe2 for lseek-espipe test");
    proc_cap.set_fd(11, Some(reader_cap));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_LSEEK, [11, 0, SEEK_SET as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_SPIPE));
}

/// `lseek(tty_fd, 0, SEEK_SET)` returns `-ESPIPE`. TTYs are
/// non-seekable like pipes; the dispatch routes the same
/// `Errno::ESPIPE` regardless of whether the underlying
/// `StructPayload` is `Tty` or `Pipe`.
#[test]
fn dispatch_lseek_on_tty_fd_returns_neg_espipe() {
    let _setup = lseek_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(13, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_LSEEK, [13, 0, SEEK_SET as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_SPIPE));
}

/// `lseek(fd, -1, SEEK_SET)` returns `-EINVAL`. A negative
/// resulting offset is rejected before the `set_offset` write;
/// the per-fd offset is unchanged.
#[test]
fn dispatch_lseek_negative_result_returns_neg_einval() {
    let _setup = lseek_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let file = pagebacked_open_file(1, 64);
    proc_cap.set_fd(7, Some(file));
    let ctx = make_ctx(proc_cap.clone(), thread);

    let req = SyscallRequest::new(NR_LSEEK, [7, (-1i64) as u64, SEEK_SET as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
    // Pre-call default offset is 0; the failed call must not
    // mutate it.
    assert_eq!(proc_cap.fd(7).expect("fd 7").offset(), 0);
}

/// `lseek(fd, 0, 99)` returns `-EINVAL`. Whence values outside
/// {SEEK_SET, SEEK_CUR, SEEK_END} are rejected with EINVAL,
/// matching Linux's `lseek(2)` errno surface.
#[test]
fn dispatch_lseek_invalid_whence_returns_neg_einval() {
    let _setup = lseek_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let file = pagebacked_open_file(1, 64);
    proc_cap.set_fd(7, Some(file));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_LSEEK, [7, 0, 99, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `lseek(99, 0, SEEK_SET)` against an unallocated fd returns
/// `-EBADF`. Defence in depth — the BTreeMap lookup returns
/// `None` before the per-OpenFile step runs.
#[test]
fn dispatch_lseek_unknown_fd_returns_neg_ebadf() {
    let _setup = lseek_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(NR_LSEEK, [99, 0, SEEK_SET as u64, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}
