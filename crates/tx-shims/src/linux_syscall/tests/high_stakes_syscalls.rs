// Focused tests for high-stakes syscall backlog entries that reuse
// existing fd-table/resource/scheduler semantics.
#![cfg_attr(test, allow(unused_imports))]

use super::*;

use crate::adapter::step_engine::{guard, page_allocator, reserve_for, sign_for, StepOutcome};
use crate::linux_syscall::{
    CLOSE_RANGE_CLOEXEC, CLOSE_RANGE_UNSHARE, NR_CLOSE_RANGE, NR_COPY_FILE_RANGE, NR_FADVISE64_64,
    NR_FALLOCATE, NR_GETRLIMIT, NR_GETRUSAGE, NR_PRCTL, NR_PREADV, NR_PREADV2, NR_PWRITE64,
    NR_PWRITEV, NR_PWRITEV2, NR_READAHEAD, NR_SCHED_GETATTR, NR_SCHED_GETPARAM,
    NR_SCHED_GETSCHEDULER, NR_SCHED_GET_PRIORITY_MAX, NR_SCHED_GET_PRIORITY_MIN,
    NR_SCHED_RR_GET_INTERVAL, NR_SCHED_SETATTR, NR_SCHED_SETSCHEDULER, NR_SCHED_YIELD,
    NR_SETRLIMIT, NR_SYNC_FILE_RANGE, RLIMIT_NOFILE,
};
use alloc::vec;
use tx_subsystems::page_backed::{
    step_read_to_kernel, step_truncate, AnonSwapPolicy, PageContainer, PageContainerKind,
};
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
};
use tx_subsystems::vm::{
    MapPlacement, Prot, UserRange, UserVirtAddr, VmBacking, VmEntryFlags, VmMapRequest,
    USER_PAGE_SIZE,
};

const E_INVAL: i32 = 22;
const E_NOSYS: i32 = 38;
const E_OPNOTSUPP: i32 = 95;

#[repr(C)]
#[derive(Clone, Copy)]
struct TestRlimit {
    rlim_cur: u64,
    rlim_max: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TestSchedParam {
    sched_priority: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TestSchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TestTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TestIovec {
    iov_base: u64,
    iov_len: u64,
}

fn hs_setup() -> TestSetup {
    let setup = setup();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for high-stakes tests: {error:?}"),
    }
    setup
}

fn pagebacked_file(id: u64, page_count: u64, size: u64) -> (Cap<OpenFile>, Cap<PageContainer>) {
    let pc = PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        page_count,
    )
    .expect("page container cap");
    let guard = guard();
    match step_truncate(&pc, size, &guard) {
        StepOutcome::Done(()) | StepOutcome::Continue { .. } => {}
        other => panic!("step_truncate({size}): {other:?}"),
    }
    drop(guard);
    let rnode = {
        let raw = RNode::new(
            FsObjectId::new(id),
            InodeMeta::new(InodeKind::Regular, 0o100644),
            RNodeBacking::PageBacked { pc: pc.clone() },
        );
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };
    let file = OpenFile::new_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
            packet: false,
        },
    )
    .expect("open file cap");
    (file, pc)
}

fn read_pc_with_file(file: &Cap<OpenFile>, pc: &Cap<PageContainer>, len: usize) -> Vec<u8> {
    let saved = file.offset();
    file.set_offset(0);
    let mut out = vec![0u8; len];
    let guard = guard();
    match step_read_to_kernel(pc, file, &mut out, &guard) {
        StepOutcome::Done(n) => out.truncate(n),
        other => panic!("step_read_to_kernel: {other:?}"),
    }
    file.set_offset(saved);
    out
}

fn map_user_bytes(ctx: &SyscallCtx<'_>, uaddr: usize, bytes: &[u8]) -> u64 {
    let range = UserRange::new_aligned(UserVirtAddr(uaddr), USER_PAGE_SIZE).expect("aligned range");
    let map_req = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        Prot::READ_WRITE,
        VmEntryFlags::PRIVATE,
        VmBacking::PrivateAnon,
    );
    ctx.aspace.try_mmap(map_req).expect("mmap anon test bytes");
    if !bytes.is_empty() {
        let guard = guard();
        let copied = ctx
            .aspace
            .copy_to_user(tx_hal::UserPtr::<u8>::new(uaddr), bytes, &guard);
        drop(guard);
        assert_eq!(copied, StepOutcome::Done(bytes.len()));
    }
    uaddr as u64
}

fn map_user_scratch(ctx: &SyscallCtx<'_>, uaddr: usize) -> u64 {
    map_user_bytes(ctx, uaddr, &[])
}

fn read_user_bytes(ctx: &SyscallCtx<'_>, uaddr: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let guard = guard();
    let copied =
        ctx.aspace
            .copy_from_user(&mut out, tx_hal::UserPtr::<u8>::new(uaddr as usize), &guard);
    drop(guard);
    assert_eq!(copied, StepOutcome::Done(len));
    out
}

fn read_user_u64(ctx: &SyscallCtx<'_>, uaddr: u64) -> u64 {
    let bytes = read_user_bytes(ctx, uaddr, 8);
    u64::from_le_bytes(bytes.try_into().expect("u64 bytes"))
}

#[test]
fn dispatch_close_range_closes_sparse_inclusive_range_and_clears_cloexec() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    for fd in [3, 4, 9, 100] {
        proc_cap.set_fd(fd, Some(tx_fs::devfs::open_console_for_init()));
        proc_cap.set_fd_cloexec(fd, true);
    }
    proc_cap.set_fd_cloexec(500, true);
    let ctx = make_ctx(proc_cap.clone(), thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOSE_RANGE, [4, u32::MAX as u64, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Return(0));
    assert!(proc_cap.fd(3).is_some());
    assert!(proc_cap.fd_cloexec(3));
    for fd in [4, 9, 100] {
        assert!(proc_cap.fd(fd).is_none(), "fd {fd} should be closed");
        assert!(!proc_cap.fd_cloexec(fd), "fd {fd} cloexec should be clear");
    }
    assert!(
        !proc_cap.fd_cloexec(500),
        "stale cloexec bit in range is cleared"
    );
}

#[test]
fn dispatch_close_range_cloexec_marks_open_fds_without_closing() {
    let _setup = setup();
    let _ops = install_capturing_console();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    for fd in [3, 7, 11] {
        proc_cap.set_fd(fd, Some(tx_fs::devfs::open_console_for_init()));
    }
    let ctx = make_ctx(proc_cap.clone(), thread);

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOSE_RANGE, [3, 7, CLOSE_RANGE_CLOEXEC as u64, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Return(0));
    for fd in [3, 7, 11] {
        assert!(
            proc_cap.fd(fd).is_some(),
            "close_range CLOEXEC must not close fd {fd}"
        );
    }
    assert!(proc_cap.fd_cloexec(3));
    assert!(proc_cap.fd_cloexec(7));
    assert!(!proc_cap.fd_cloexec(11));
}

#[test]
fn dispatch_close_range_rejects_unknown_flags_and_unshare_is_unimplemented() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);

    let unknown = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOSE_RANGE, [0, 0, 0x8000, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(unknown, SyscallResult::Error(E_INVAL));

    let unshare = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_CLOSE_RANGE, [0, 0, CLOSE_RANGE_UNSHARE as u64, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(unshare, SyscallResult::Error(E_NOSYS));
}

#[test]
fn dispatch_getrlimit_and_setrlimit_alias_prlimit64_nofile() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap.clone(), thread);
    let new_limit = TestRlimit {
        rlim_cur: 4,
        rlim_max: 8,
    };
    let mut old_limit = TestRlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    let set = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_SETRLIMIT,
            [
                RLIMIT_NOFILE as u64,
                &new_limit as *const TestRlimit as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(set, SyscallResult::Return(0));

    let get = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GETRLIMIT,
            [
                RLIMIT_NOFILE as u64,
                &mut old_limit as *mut TestRlimit as u64,
                0,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(get, SyscallResult::Return(0));
    assert_eq!(old_limit.rlim_cur, 4);
    assert_eq!(old_limit.rlim_max, 8);
}

#[test]
fn dispatch_getrusage_writes_144_zero_bytes_and_preserves_tail() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut buf = [0xA5u8; 160];

    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETRUSAGE, [0, buf.as_mut_ptr() as u64, 0, 0, 0, 0]),
        &ctx,
    ));

    assert_eq!(r, SyscallResult::Return(0));
    assert_eq!(&buf[..144], &[0u8; 144]);
    assert_eq!(&buf[144..], &[0xA5u8; 16]);

    let invalid = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_GETRUSAGE, [99, buf.as_mut_ptr() as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(invalid, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_scheduler_queries_return_fixed_sched_other_model() {
    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut param = TestSchedParam { sched_priority: -1 };
    let mut interval = TestTimespec {
        tv_sec: -1,
        tv_nsec: -1,
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SCHED_GETSCHEDULER, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_GETPARAM,
                [0, &mut param as *mut TestSchedParam as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(param.sched_priority, 0);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SCHED_GET_PRIORITY_MAX, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SCHED_GET_PRIORITY_MIN, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_RR_GET_INTERVAL,
                [0, &mut interval as *mut TestTimespec as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(interval.tv_sec, 0);
    assert_eq!(interval.tv_nsec, 0);
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SCHED_YIELD, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_scheduler_attr_and_policy_round_trip_compat_state() {
    const SCHED_BATCH: u32 = 3;
    const SCHED_IDLE: u64 = 5;

    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut attr = TestSchedAttr {
        size: core::mem::size_of::<TestSchedAttr>() as u32,
        sched_policy: SCHED_BATCH,
        sched_flags: 0,
        sched_nice: 0,
        sched_priority: 0,
        sched_runtime: 0,
        sched_deadline: 0,
        sched_period: 0,
    };
    let mut out = TestSchedAttr {
        size: core::mem::size_of::<TestSchedAttr>() as u32,
        sched_policy: 0xff,
        sched_flags: 0xff,
        sched_nice: -99,
        sched_priority: 99,
        sched_runtime: 1,
        sched_deadline: 1,
        sched_period: 1,
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_SETATTR,
                [0, &mut attr as *mut TestSchedAttr as u64, 0, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_GETATTR,
                [
                    0,
                    &mut out as *mut TestSchedAttr as u64,
                    core::mem::size_of::<TestSchedAttr>() as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(out.sched_policy, SCHED_BATCH);

    let mut param = TestSchedParam { sched_priority: 0 };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_SETSCHEDULER,
                [
                    0,
                    SCHED_IDLE,
                    &mut param as *mut TestSchedParam as u64,
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
            SyscallRequest::new(NR_SCHED_GETSCHEDULER, [0, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(SCHED_IDLE as i64)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_SCHED_SETSCHEDULER,
                [0, 0, &mut param as *mut TestSchedParam as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
}

#[test]
fn dispatch_prctl_common_options_round_trip_process_state() {
    const PR_SET_PDEATHSIG: u64 = 1;
    const PR_GET_PDEATHSIG: u64 = 2;
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    const PR_SET_TIMERSLACK: u64 = 29;
    const PR_GET_TIMERSLACK: u64 = 30;

    let _setup = setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let ctx = make_ctx(proc_cap, thread);
    let mut name = [0u8; 16];
    name[..8].copy_from_slice(b"txv2-ltp");
    let mut out = [0xA5u8; 16];
    let mut pdeathsig = 0i32;

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL, [PR_SET_NAME, name.as_ptr() as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL, [PR_GET_NAME, out.as_mut_ptr() as u64, 0, 0, 0, 0],),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(&out[..8], b"txv2-ltp");

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL, [PR_SET_TIMERSLACK, 123_456, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL, [PR_GET_TIMERSLACK, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(123_456)
    );

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PRCTL, [PR_SET_PDEATHSIG, 15, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_PRCTL,
                [
                    PR_GET_PDEATHSIG,
                    &mut pdeathsig as *mut i32 as u64,
                    0,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(pdeathsig, 15);
}

#[test]
fn dispatch_positioned_write_and_vector_io_restore_original_offset() {
    let _setup = hs_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let (file, pc) = pagebacked_file(92_001, 1, 0);
    proc_cap.set_fd(6, Some(file.clone()));
    file.set_offset(123);
    let ctx = make_ctx(proc_cap, thread);

    let first = map_user_bytes(&ctx, 0x5200_0000, b"abcd");
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PWRITE64, [6, first, 4, 2, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(4)
    );
    assert_eq!(file.offset(), 123);

    let left = map_user_bytes(&ctx, 0x5200_1000, b"XY");
    let right = map_user_bytes(&ctx, 0x5200_2000, b"Z");
    let iov = [
        TestIovec {
            iov_base: left,
            iov_len: 2,
        },
        TestIovec {
            iov_base: right,
            iov_len: 1,
        },
    ];
    let iov_ptr = map_user_bytes(&ctx, 0x5200_3000, unsafe {
        core::slice::from_raw_parts(iov.as_ptr() as *const u8, core::mem::size_of_val(&iov))
    });
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PWRITEV, [6, iov_ptr, 2, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(3)
    );
    assert_eq!(file.offset(), 123);

    let dst1 = map_user_scratch(&ctx, 0x5200_4000);
    let dst2 = map_user_scratch(&ctx, 0x5200_5000);
    let riov = [
        TestIovec {
            iov_base: dst1,
            iov_len: 2,
        },
        TestIovec {
            iov_base: dst2,
            iov_len: 2,
        },
    ];
    let riov_ptr = map_user_bytes(&ctx, 0x5200_6000, unsafe {
        core::slice::from_raw_parts(riov.as_ptr() as *const u8, core::mem::size_of_val(&riov))
    });
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PREADV2, [6, riov_ptr, 2, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(4)
    );
    assert_eq!(file.offset(), 123);
    assert_eq!(read_user_bytes(&ctx, dst1, 2), b"XY");
    assert_eq!(read_user_bytes(&ctx, dst2, 2), b"Zb");
    assert_eq!(&read_pc_with_file(&file, &pc, 6), b"XYZbcd");

    let unsupported = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PWRITEV2, [6, iov_ptr, 1, 0, 0, 1]),
        &ctx,
    ));
    assert_eq!(unsupported, SyscallResult::Error(E_OPNOTSUPP));
}

#[test]
fn dispatch_file_advice_readahead_and_sync_file_range_validate_inputs() {
    let _setup = hs_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let (file, _pc) = pagebacked_file(92_002, 1, 0);
    proc_cap.set_fd(6, Some(file));
    let ctx = make_ctx(proc_cap, thread);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FADVISE64_64, [6, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FADVISE64_64, [6, 0, 0, 99, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_READAHEAD, [99, 0, 1, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(9)
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_SYNC_FILE_RANGE, [6, 0, 0, 0x8, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_INVAL)
    );
}

#[test]
fn dispatch_fallocate_grows_pagebacked_file_and_rejects_modes() {
    let _setup = hs_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let (file, pc) = pagebacked_file(92_003, 2, 4);
    proc_cap.set_fd(6, Some(file.clone()));
    let ctx = make_ctx(proc_cap, thread);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FALLOCATE, [6, 0, 8, 16, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert!(pc.size_bytes() >= 24);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FALLOCATE, [6, 1, 0, 1, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0)
    );
    assert_eq!(pc.size_bytes(), 24);

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FALLOCATE, [6, 2, 0, 1, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_OPNOTSUPP)
    );
}

#[test]
fn dispatch_copy_file_range_updates_offset_pointers_without_touching_fd_offsets() {
    let _setup = hs_setup();
    let proc_cap = bootstrap();
    let thread = first_thread(&proc_cap);
    let (src_file, src_pc) = pagebacked_file(92_004, 1, 0);
    let (dst_file, dst_pc) = pagebacked_file(92_005, 1, 0);
    proc_cap.set_fd(6, Some(src_file.clone()));
    proc_cap.set_fd(7, Some(dst_file.clone()));
    src_file.set_offset(77);
    dst_file.set_offset(88);
    let ctx = make_ctx(proc_cap, thread);

    let payload = map_user_bytes(&ctx, 0x5200_7000, b"hello-copy");
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_PWRITE64, [6, payload, 10, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(10)
    );

    let off_in = map_user_bytes(&ctx, 0x5200_8000, &2u64.to_le_bytes());
    let off_out = map_user_bytes(&ctx, 0x5200_9000, &1u64.to_le_bytes());
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_COPY_FILE_RANGE, [6, off_in, 7, off_out, 5, 0]),
            &ctx,
        )),
        SyscallResult::Return(5)
    );

    assert_eq!(read_user_u64(&ctx, off_in), 7);
    assert_eq!(read_user_u64(&ctx, off_out), 6);
    assert_eq!(src_file.offset(), 77);
    assert_eq!(dst_file.offset(), 88);
    assert_eq!(&read_pc_with_file(&dst_file, &dst_pc, 6), b"\0llo-c");
    assert_eq!(&read_pc_with_file(&src_file, &src_pc, 10), b"hello-copy");
}
