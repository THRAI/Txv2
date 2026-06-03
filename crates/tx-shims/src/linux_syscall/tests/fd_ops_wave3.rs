// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use tx_subsystems::process::bootstrap_init_process;

use crate::linux_syscall::{
    F_GETPIPE_SZ, F_SETPIPE_SZ, NR_FCNTL, NR_PIPE2, NR_PSELECT6, NR_WRITE, O_CLOEXEC, O_DIRECT,
    O_NONBLOCK,
};

const E_INVAL: i32 = 22;
const E_NOSYS: i32 = 38;
const E_BUSY: i32 = 16;
const E_PERM: i32 = 1;

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

/// `pipe2(uaddr, O_DIRECT)` returns `-ENOSYS` (packet-mode pipes
/// are out of scope for the slice).
#[test]
fn dispatch_pipe2_with_o_direct_returns_neg_enosys() {
    let _setup = pipe2_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);
    let mut pipefd: [u32; 2] = [u32::MAX, u32::MAX];

    let req = SyscallRequest::new(
        NR_PIPE2,
        [pipefd.as_mut_ptr() as u64, O_DIRECT as u64, 0, 0, 0, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
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
