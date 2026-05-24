// Auto-extracted syscall tests for the pipe/splice tail.
#![cfg_attr(test, allow(unused_imports))]

use super::*;
use crate::adapter::step_engine::{guard, page_allocator, reserve_for, sign_for, StepOutcome};
use crate::linux_syscall::{NR_PIPE2, NR_READ, NR_SPLICE, NR_TEE, NR_VMSPLICE, NR_WRITE};
use tx_subsystems::page_backed::{
    step_read_to_kernel, step_truncate, step_write_from_kernel, AnonSwapPolicy, MaterializeAccess,
    PageContainer, PageContainerKind, PageIndex,
};
use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
};

const E_INVAL: i32 = 22;
const E_SPIPE: i32 = 29;
const SPLICE_F_MOVE: u32 = 0x01;
const SPLICE_F_NONBLOCK: u32 = 0x02;
const SPLICE_F_MORE: u32 = 0x04;

#[repr(C)]
#[derive(Clone, Copy)]
struct TestIovec {
    base: u64,
    len: u64,
}

fn splice_setup() -> TestSetup {
    let setup = setup();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for splice tests: {error:?}"),
    }
    setup
}

fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let process = bootstrap_init_process(fresh_aspace()).expect("bootstrap init for splice tests");
    let thread = process.nth_thread(0).expect("leader thread");
    (process, thread)
}

fn pipe_pair(ctx: &SyscallCtx<'_>) -> (u32, u32) {
    let mut pipefd = [u32::MAX; 2];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIPE2, [pipefd.as_mut_ptr() as u64, 0, 0, 0, 0, 0]),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    (pipefd[0], pipefd[1])
}

fn write_all(ctx: &SyscallCtx<'_>, fd: u32, bytes: &[u8]) {
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_WRITE,
            [
                fd as u64,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
                0,
            ],
        ),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(bytes.len() as i64));
}

fn read_exact(ctx: &SyscallCtx<'_>, fd: u32, len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.resize(len, 0);
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_READ,
            [fd as u64, out.as_mut_ptr() as u64, len as u64, 0, 0, 0],
        ),
        ctx,
    ));
    assert_eq!(result, SyscallResult::Return(len as i64));
    out
}

fn pagebacked_open_file(id: u64, page_count: u64, size_bytes: u64) -> Cap<OpenFile> {
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    )
    .expect("page container cap");
    let guard = guard();
    match step_truncate(&pc, size_bytes, &guard) {
        StepOutcome::Done(()) | StepOutcome::Continue { .. } => {}
        other => panic!("step_truncate({size_bytes}): {other:?}"),
    }
    drop(guard);
    let rnode = {
        let raw = RNode::new(
            FsObjectId::new(id),
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

fn write_pagebacked(file: &Cap<OpenFile>, bytes: &[u8]) {
    let guard = guard();
    let pc = match file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc,
        _ => panic!("expected page-backed file"),
    };
    match step_write_from_kernel(pc, file, bytes, &guard) {
        StepOutcome::Done(n) => assert_eq!(n, bytes.len()),
        other => panic!("step_write_from_kernel: {other:?}"),
    }
}

fn read_pagebacked(file: &Cap<OpenFile>, out: &mut [u8]) {
    let guard = guard();
    let pc = match file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc,
        _ => panic!("expected page-backed file"),
    };
    match step_read_to_kernel(pc, file, out, &guard) {
        StepOutcome::Done(n) => assert_eq!(n, out.len()),
        other => panic!("step_read_to_kernel: {other:?}"),
    }
}

#[test]
fn splice_numbers_match_linux_rv64_6_17() {
    assert_eq!(NR_VMSPLICE, 75);
    assert_eq!(NR_SPLICE, 76);
    assert_eq!(NR_TEE, 77);
}

#[test]
fn dispatch_vmsplice_writes_iovec_bytes_to_pipe() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (read_fd, write_fd) = pipe_pair(&ctx);
    let first = b"zero";
    let second = b"copy";
    let iov = [
        TestIovec {
            base: first.as_ptr() as u64,
            len: first.len() as u64,
        },
        TestIovec {
            base: second.as_ptr() as u64,
            len: second.len() as u64,
        },
    ];

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_VMSPLICE,
            [
                write_fd as u64,
                iov.as_ptr() as u64,
                iov.len() as u64,
                (SPLICE_F_MOVE | SPLICE_F_MORE) as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(8));
    assert_eq!(read_exact(&ctx, read_fd, 8), b"zerocopy");
}

#[test]
fn dispatch_splice_moves_bytes_between_pipes() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (in_read, in_write) = pipe_pair(&ctx);
    let (out_read, out_write) = pipe_pair(&ctx);
    write_all(&ctx, in_write, b"abcdef");

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SPLICE,
            [
                in_read as u64,
                0,
                out_write as u64,
                0,
                4,
                SPLICE_F_NONBLOCK as u64,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(4));
    assert_eq!(read_exact(&ctx, out_read, 4), b"abcd");
    assert_eq!(read_exact(&ctx, in_read, 2), b"ef");
}

#[test]
fn dispatch_tee_duplicates_pipe_bytes_without_consuming_input() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (in_read, in_write) = pipe_pair(&ctx);
    let (out_read, out_write) = pipe_pair(&ctx);
    write_all(&ctx, in_write, b"mirror");

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_TEE, [in_read as u64, out_write as u64, 4, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(4));
    assert_eq!(read_exact(&ctx, out_read, 4), b"mirr");
    assert_eq!(read_exact(&ctx, in_read, 6), b"mirror");
}

#[test]
fn dispatch_splice_rejects_pipe_offset_pointer_with_espipe() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (in_read, _in_write) = pipe_pair(&ctx);
    let (_out_read, out_write) = pipe_pair(&ctx);
    let mut off = 0u64;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SPLICE,
            [
                in_read as u64,
                &mut off as *mut u64 as u64,
                out_write as u64,
                0,
                1,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_SPIPE));
}

#[test]
fn dispatch_splice_unknown_flags_return_einval() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let (in_read, _in_write) = pipe_pair(&ctx);
    let (_out_read, out_write) = pipe_pair(&ctx);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SPLICE,
            [in_read as u64, 0, out_write as u64, 0, 1, 0x8000_0000],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_splice_file_to_pipe_updates_offset_pointer_not_fd_offset() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let file = pagebacked_open_file(9_501, 1, 0);
    proc_cap.set_fd(9, Some(file.clone()));
    write_pagebacked(&file, b"0123456789");
    file.set_offset(0);
    let (read_fd, write_fd) = pipe_pair(&ctx);
    let mut off_in = 3u64;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SPLICE,
            [9, &mut off_in as *mut u64 as u64, write_fd as u64, 0, 4, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(4));
    assert_eq!(off_in, 7);
    assert_eq!(file.offset(), 0);
    assert_eq!(read_exact(&ctx, read_fd, 4), b"3456");
}

#[test]
fn dispatch_splice_pipe_to_file_updates_output_offset_pointer_not_fd_offset() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let file = pagebacked_open_file(9_502, 1, 0);
    proc_cap.set_fd(9, Some(file.clone()));
    write_pagebacked(&file, b"abcdefgh");
    file.set_offset(1);
    let (read_fd, write_fd) = pipe_pair(&ctx);
    write_all(&ctx, write_fd, b"XYZ");
    let mut off_out = 4u64;

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SPLICE,
            [read_fd as u64, 0, 9, &mut off_out as *mut u64 as u64, 3, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(3));
    assert_eq!(off_out, 7);
    assert_eq!(file.offset(), 1);

    let mut out = [0u8; 8];
    file.set_offset(0);
    read_pagebacked(&file, &mut out);
    assert_eq!(&out, b"abcdXYZh");
}

#[test]
fn dispatch_splice_pagebacked_full_page_through_pipe_installs_shared_page() {
    let _setup = splice_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let src = pagebacked_open_file(9_503, 1, tx_subsystems::vm::USER_PAGE_SIZE as u64);
    let dst = pagebacked_open_file(9_504, 1, 0);
    proc_cap.set_fd(9, Some(src.clone()));
    proc_cap.set_fd(10, Some(dst.clone()));
    let payload = alloc::vec![b'q'; tx_subsystems::vm::USER_PAGE_SIZE];
    write_pagebacked(&src, &payload);
    src.set_offset(0);
    let (read_fd, write_fd) = pipe_pair(&ctx);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SPLICE,
            [
                9,
                0,
                write_fd as u64,
                0,
                payload.len() as u64,
                SPLICE_F_NONBLOCK as u64,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(payload.len() as i64));
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SPLICE,
            [
                read_fd as u64,
                0,
                10,
                0,
                payload.len() as u64,
                SPLICE_F_NONBLOCK as u64,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(payload.len() as i64));

    let src_pc = match src.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc,
        _ => panic!("expected page-backed src"),
    };
    let dst_pc = match dst.rnode().backing() {
        RNodeBacking::PageBacked { pc } => pc,
        _ => panic!("expected page-backed dst"),
    };
    let guard = guard();
    let src_page = match src_pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard)
    {
        StepOutcome::Done(page) => page.ppn,
        other => panic!("src materialize: {other:?}"),
    };
    let dst_page = match dst_pc.materialize_page(PageIndex::new(0), MaterializeAccess::Read, &guard)
    {
        StepOutcome::Done(page) => page.ppn,
        other => panic!("dst materialize: {other:?}"),
    };
    drop(guard);
    assert_eq!(
        src_page, dst_page,
        "full-page splice should install shared lease"
    );
}
