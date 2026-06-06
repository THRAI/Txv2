// Auto-extracted from `tests.rs` style modules.
#![cfg_attr(test, allow(unused_imports))]
use super::*;

use crate::linux_syscall::{NR_EVENTFD2, NR_IO_URING_ENTER, NR_IO_URING_SETUP};
use tx_subsystems::io_uring::SqeStub;

const E_BADF: i32 = 9;
const E_INVAL: i32 = 22;

fn io_uring_setup() -> (TestSetup, Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    (setup, proc_cap, thread)
}

fn create_io_uring(ctx: &SyscallCtx<'_>, entries: u32) -> i64 {
    match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_IO_URING_SETUP, [entries as u64, 0, 0, 0, 0, 0]),
        ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("io_uring_setup: {other:?}"),
    }
}

#[test]
fn dispatch_io_uring_enter_zero_submit_returns_zero() {
    let (_setup, proc_cap, thread) = io_uring_setup();
    let ctx = make_ctx(proc_cap, thread);
    let fd = create_io_uring(&ctx, 2);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_IO_URING_ENTER, [fd as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
}

#[test]
fn dispatch_io_uring_enter_drains_in_kernel_submission_ring() {
    let (_setup, proc_cap, thread) = io_uring_setup();
    let ctx = make_ctx(proc_cap.clone(), thread);
    let fd = create_io_uring(&ctx, 4);
    let ring_file = proc_cap.fd(fd as u32).expect("uring fd installed");
    let ring = ring_file.io_uring().expect("uring backing").clone();
    ring.push_sqe_for_test(SqeStub::new(0, 0x1111))
        .expect("first sqe");
    ring.push_sqe_for_test(SqeStub::new(0, 0x2222))
        .expect("second sqe");

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_IO_URING_ENTER, [fd as u64, 1, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(1));
    assert_eq!(ring.sq_len(), 1);
    let cqe = ring.pop_cqe().expect("submitted sqe produces cqe");
    assert_eq!(cqe.user_data, 0x1111);
    assert_eq!(cqe.res, 0);
    assert_eq!(cqe.flags, 0);
}

#[test]
fn dispatch_io_uring_enter_validates_fd_kind_and_flags() {
    let (_setup, proc_cap, thread) = io_uring_setup();
    let ctx = make_ctx(proc_cap, thread);

    let invalid = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_IO_URING_ENTER, [99, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(invalid, SyscallResult::Error(E_BADF));

    let eventfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_EVENTFD2, [0, 0, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd,
        other => panic!("eventfd2: {other:?}"),
    };
    let wrong_kind = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_IO_URING_ENTER, [eventfd as u64, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(wrong_kind, SyscallResult::Error(E_INVAL));

    let ring_fd = create_io_uring(&ctx, 2);
    let bad_flags = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_IO_URING_ENTER, [ring_fd as u64, 0, 0, 0xffff_0000, 0, 0]),
        &ctx,
    ));
    assert_eq!(bad_flags, SyscallResult::Error(E_INVAL));
}
