// Auto-extracted from `tests.rs` (2026-05-08 jumbo split).
#![cfg_attr(test, allow(unused_imports))]
use super::*;
use crate::adapter::step_engine::{
    self as step_engine, guard, page_allocator, reserve_for, sign_for, StepOutcome,
};
use tx_subsystems::page_backed::{step_truncate, AnonSwapPolicy, PageContainer, PageContainerKind};
use tx_subsystems::pipe::{step_pipe2, PipeFlags};
use tx_subsystems::process::bootstrap_init_process;
use tx_subsystems::vfs::structure::{
    FsObjectId, InodeKind, InodeMeta, OpenFileFlags, RNode, RNodeBacking,
};
use tx_subsystems::vm::{AccessMode, UserVirtAddr, VmBacking, VmFault, USER_PAGE_SIZE};

use crate::linux_syscall::{
    MADV_DONTNEED, MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE, MAP_SHARED,
    MAP_SHARED_VALIDATE, MREMAP_FIXED, MREMAP_MAYMOVE, NR_MADVISE, NR_MMAP, NR_MPROTECT, NR_MREMAP,
    NR_MUNMAP, PROT_READ, PROT_WRITE,
};

const E_ACCES: i32 = 13;
const E_BADF: i32 = 9;
const E_BUSY: i32 = 16;
const E_EXIST: i32 = 17;
const E_FAULT: i32 = 14;
const E_INVAL: i32 = 22;
const E_NOMEM: i32 = 12;
const E_NODEV: i32 = 19;
const E_NOSYS: i32 = 38;
const E_OPNOTSUPP: i32 = 95;

fn vm_setup() -> TestSetup {
    let setup = setup();
    match page_allocator::claim_zero_frame() {
        Ok(_) | Err(page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for vm-syscalls tests: {error:?}"),
    }
    setup
}

fn fresh_proc_thread() -> (Cap<ProcessIdentity>, Cap<ThreadIdentity>) {
    let process =
        bootstrap_init_process(fresh_aspace()).expect("bootstrap init for vm-syscalls tests");
    let thread = process.nth_thread(0).expect("leader thread");
    (process, thread)
}

/// Build a `Cap<OpenFile>` over a fresh anon `PageContainer` of
/// `page_count` pages with size `size_bytes`. Mirrors the
/// fd-ops Wave 4 helper but local to this module.
fn pagebacked_open_file(page_count: u64, size_bytes: u64) -> Cap<OpenFile> {
    pagebacked_open_file_with_flags(
        page_count,
        size_bytes,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: false,
            nonblocking: false,
        },
    )
}

fn pagebacked_open_file_with_flags(
    page_count: u64,
    size_bytes: u64,
    flags: OpenFileFlags,
) -> Cap<OpenFile> {
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
            FsObjectId::new(9_100),
            InodeMeta::new(InodeKind::Regular, 0o100644),
            RNodeBacking::PageBacked { pc },
        );
        let res = reserve_for::<RNode>().expect("rnode reservation");
        sign_for(res, raw)
    };
    OpenFile::new_cap(rnode, flags).expect("open file cap")
}

/// `mmap(0, PAGE, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS,
/// -1, 0)` returns a page-aligned user VA on success.
#[test]
fn dispatch_mmap_anonymous_private_returns_aligned_user_va() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [
            0,
            USER_PAGE_SIZE as u64,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            u64::MAX, // fd = -1 (unused for ANONYMOUS)
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    match result {
        SyscallResult::Return(addr) => {
            // Syscall-level non-fixed mmap keeps page 0 unmapped so
            // libc/user code never observes a successful NULL mapping.
            assert!(addr > 0, "mmap returned non-null VA");
            assert_eq!(
                (addr as usize) % USER_PAGE_SIZE,
                0,
                "returned VA is page-aligned"
            );
        }
        other => panic!("expected Return, got {other:?}"),
    }
}

/// `mmap(.., 0, ..)` rejects with -EINVAL.
#[test]
fn dispatch_mmap_anonymous_private_zero_length_returns_neg_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [
            0,
            0,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            u64::MAX,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `MAP_SHARED | MAP_ANONYMOUS` should route to a real anonymous
/// PageContainer, not `PrivateAnon`; the returned mapping must fault
/// as shared page-backed memory instead of hitting BackingMismatch.
#[test]
fn dispatch_mmap_shared_anonymous_faults_through_page_container() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [
            0,
            USER_PAGE_SIZE as u64,
            PROT_READ | PROT_WRITE,
            MAP_SHARED | MAP_ANONYMOUS,
            u64::MAX,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    let addr = match result {
        SyscallResult::Return(addr) => addr as usize,
        other => panic!("expected Return, got {other:?}"),
    };

    let published = block_on(
        ctx.aspace
            .fault_script(VmFault::new(UserVirtAddr(addr), AccessMode::Write)),
    )
    .expect("shared anonymous fault should materialize");

    assert_eq!(published.page, UserVirtAddr(addr).containing_page());
}

/// `mmap(unaligned, PAGE, .., MAP_FIXED, ..)` rejects -EINVAL.
#[test]
fn dispatch_mmap_anonymous_private_unaligned_addr_with_fixed_returns_neg_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [
            0x1234, // not page-aligned
            USER_PAGE_SIZE as u64,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
            u64::MAX,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_mremap_maymove_without_fixed_ignores_new_addr_and_moves_when_needed() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let old_addr = 0x2000_0000u64;
    let seed = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                old_addr,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(seed, SyscallResult::Return(old_addr as i64));
    let blocker = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                old_addr + USER_PAGE_SIZE as u64,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(
        blocker,
        SyscallResult::Return((old_addr + USER_PAGE_SIZE as u64) as i64)
    );

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MREMAP,
            [
                old_addr,
                USER_PAGE_SIZE as u64,
                (USER_PAGE_SIZE * 2) as u64,
                MREMAP_MAYMOVE,
                0x1234, // ignored unless MREMAP_FIXED is present
                0,
            ],
        ),
        &ctx,
    ));
    let new_addr = match result {
        SyscallResult::Return(addr) => addr as usize,
        other => panic!("expected Return, got {other:?}"),
    };

    assert_eq!(new_addr % USER_PAGE_SIZE, 0);
    assert_ne!(new_addr, 0x1234);
    assert!(ctx.aspace.lookup(UserVirtAddr(old_addr as usize)).is_none());
    assert!(ctx.aspace.lookup(UserVirtAddr(new_addr)).is_some());
}

#[test]
fn dispatch_mremap_grow_without_maymove_returns_nomem_when_adjacent_range_occupied() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let old_addr = 0x3000_0000u64;
    let seed = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                old_addr,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(seed, SyscallResult::Return(old_addr as i64));
    let blocker = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                old_addr + USER_PAGE_SIZE as u64,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(
        blocker,
        SyscallResult::Return((old_addr + USER_PAGE_SIZE as u64) as i64)
    );

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MREMAP,
            [
                old_addr,
                USER_PAGE_SIZE as u64,
                (USER_PAGE_SIZE * 2) as u64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Error(E_NOMEM));
}

#[test]
fn dispatch_mremap_fixed_maymove_replaces_destination_mapping() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let old_addr = 0x3100_0000u64;
    let new_addr = 0x3200_0000u64;
    for addr in [old_addr, new_addr] {
        let seed = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    addr,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(seed, SyscallResult::Return(addr as i64));
    }

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MREMAP,
            [
                old_addr,
                USER_PAGE_SIZE as u64,
                USER_PAGE_SIZE as u64,
                MREMAP_MAYMOVE | MREMAP_FIXED,
                new_addr,
                0,
            ],
        ),
        &ctx,
    ));

    assert_eq!(result, SyscallResult::Return(new_addr as i64));
    assert!(ctx.aspace.lookup(UserVirtAddr(old_addr as usize)).is_none());
    assert!(ctx.aspace.lookup(UserVirtAddr(new_addr as usize)).is_some());
}

#[test]
fn dispatch_mremap_missing_old_mapping_returns_neg_efault() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MREMAP,
            [
                0x4000_0000,
                USER_PAGE_SIZE as u64,
                (USER_PAGE_SIZE * 2) as u64,
                MREMAP_MAYMOVE,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_FAULT));
}

/// `mmap(0, PAGE, PROT_READ, MAP_PRIVATE, fd, 0)` against a fd
/// whose rnode is `RNodeBacking::PageBacked` returns a page-aligned
/// user VA.
#[test]
fn dispatch_mmap_file_backed_against_pagebacked_fd_returns_va() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let file = pagebacked_open_file(2, USER_PAGE_SIZE as u64);
    proc_cap.set_fd(7, Some(file));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [0, USER_PAGE_SIZE as u64, PROT_READ, MAP_PRIVATE, 7, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    match result {
        SyscallResult::Return(addr) => {
            assert!(addr > 0);
            assert_eq!((addr as usize) % USER_PAGE_SIZE, 0);
        }
        other => panic!("expected Return, got {other:?}"),
    }
}

/// `mmap(0, PAGE, PROT_READ, MAP_PRIVATE, tty_fd, 0)` returns
/// -ENODEV. TTY rnodes are `StructBacked { Tty }`, not
/// PageBacked; mmap rejects rather than installing a recipe with
/// `VmBacking::None`.
#[test]
fn dispatch_mmap_file_backed_against_tty_fd_returns_neg_enodev() {
    let _setup = vm_setup();
    let _ops = install_capturing_console();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(13, Some(tx_fs::devfs::open_console_for_init()));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [0, USER_PAGE_SIZE as u64, PROT_READ, MAP_PRIVATE, 13, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NODEV));
}

/// `mmap(.., MAP_PRIVATE, pipe_fd, ..)` returns -ENODEV. Pipes are
/// `StructBacked { Pipe }`; same rejection as TTY.
#[test]
fn dispatch_mmap_file_backed_against_pipe_fd_returns_neg_enodev() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let (reader_cap, _writer_cap) =
        step_pipe2(PipeFlags::default()).expect("pipe2 for vm-syscalls test");
    proc_cap.set_fd(11, Some(reader_cap));
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [0, USER_PAGE_SIZE as u64, PROT_READ, MAP_PRIVATE, 11, 0],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NODEV));
}

/// `mmap(.., MAP_FIXED, ..)` over an existing mapping silently
/// replaces it (no -EEXIST). First call installs at a known page;
/// second call with MAP_FIXED at the same page succeeds.
#[test]
fn dispatch_mmap_with_map_fixed_replaces_existing_mapping() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    // First, install at a known fixed addr (chosen high to avoid
    // colliding with anywhere-allocator output).
    let target = 0x1_0000_0000u64;
    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(target as i64));

    // Now MAP_FIXED at the same target — silently replaces.
    let r2 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Return(target as i64));
}

/// `mmap(.., MAP_FIXED_NOREPLACE, ..)` over an existing mapping
/// returns -EEXIST.
#[test]
fn dispatch_mmap_with_map_fixed_noreplace_on_overlap_returns_neg_eexist() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x1_0000_0000u64;
    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(target as i64));

    let r2 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Error(E_EXIST));
}

/// `mmap` with neither MAP_SHARED nor MAP_PRIVATE rejects -EINVAL.
#[test]
fn dispatch_mmap_with_neither_shared_nor_private_returns_neg_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [
            0,
            USER_PAGE_SIZE as u64,
            PROT_READ | PROT_WRITE,
            MAP_ANONYMOUS, // no MAP_PRIVATE / MAP_SHARED
            u64::MAX,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

#[test]
fn dispatch_mmap_shared_validate_unknown_flag_returns_neg_eopnotsupp() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(3, Some(pagebacked_open_file(1, USER_PAGE_SIZE as u64)));
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_SHARED_VALIDATE | (1 << 10),
                3,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_OPNOTSUPP));
}

#[test]
fn dispatch_mmap_file_backed_write_only_fd_returns_neg_eacces() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    proc_cap.set_fd(
        3,
        Some(pagebacked_open_file_with_flags(
            1,
            USER_PAGE_SIZE as u64,
            OpenFileFlags {
                read: false,
                write: true,
                append: false,
                cloexec: false,
                nonblocking: false,
            },
        )),
    );
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                0,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE,
                3,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_ACCES));
}

#[test]
fn dispatch_mmap_file_backed_closed_fd_precedes_zero_length_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MMAP, [0, 0, PROT_WRITE, MAP_SHARED, 99, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}

#[test]
fn dispatch_mmap_file_backed_negative_fd_precedes_zero_length_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MMAP, [0, 0, PROT_WRITE, MAP_SHARED, u64::MAX, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}

/// `munmap(addr, length)` against a previously-installed mmap
/// returns 0 and unmaps the range; a follow-up MAP_FIXED_NOREPLACE
/// at the same addr now succeeds (proves the recipe is gone).
#[test]
fn dispatch_munmap_against_existing_mapping_returns_zero_and_unmaps() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x1_0000_0000u64;
    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(target as i64));

    let unmap = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MUNMAP, [target, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(unmap, SyscallResult::Return(0));

    // The slot is now free — MAP_FIXED_NOREPLACE succeeds.
    let r2 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r2, SyscallResult::Return(target as i64));
}

/// `munmap(addr, length)` against a fully-disjoint unmapped range
/// returns 0 — Linux's permissive semantic, matching
/// `try_munmap`'s `rewrite_unmap` (no entries intersect, so no
/// rewrite happens; the call is a no-op success).
#[test]
fn dispatch_munmap_against_unmapped_range_returns_zero() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x1_0000_0000u64;
    let r = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MUNMAP, [target, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(r, SyscallResult::Return(0));
}

/// `mprotect(addr, length, PROT_READ)` against an existing
/// PROT_READ|PROT_WRITE mapping flips it to PROT_READ.
#[test]
fn dispatch_mprotect_flips_existing_mapping_protection() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x1_0000_0000u64;
    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(target as i64));

    let mprotect = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MPROTECT,
            [target, USER_PAGE_SIZE as u64, PROT_READ, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(mprotect, SyscallResult::Return(0));
}

/// `madvise(addr, length, MADV_DONTNEED)` against an existing
/// mapping returns 0 (range-scoped pmap teardown; recipe
/// preserved).
#[test]
fn dispatch_madvise_dontneed_on_existing_mapping_returns_zero() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x1_0000_0000u64;
    let r1 = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(r1, SyscallResult::Return(target as i64));

    let advice = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MADVISE,
            [target, USER_PAGE_SIZE as u64, MADV_DONTNEED, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(advice, SyscallResult::Return(0));
}

/// `mincore(addr, length, vec)` copies one residency byte per page:
/// low bit set for a resident PTE, clear for a recipe-only page.
#[test]
fn dispatch_mincore_copies_residency_vector_to_user() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x3300_0000u64;
    let vec_addr = 0x3400_0000u64;
    let map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                (USER_PAGE_SIZE * 2) as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map, SyscallResult::Return(target as i64));
    let vec_map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                vec_addr,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(vec_map, SyscallResult::Return(vec_addr as i64));

    block_on(ctx.aspace.fault_script(VmFault::new(
        UserVirtAddr(target as usize),
        AccessMode::Read,
    )))
    .expect("first target page materializes");

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MINCORE,
            [target, (USER_PAGE_SIZE * 2) as u64, vec_addr, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    let mut vec = [0xffu8; 2];
    let guard = guard();
    let copied = ctx.aspace.copy_from_user(
        &mut vec,
        tx_hal::UserPtr::<u8>::new(vec_addr as usize),
        &guard,
    );
    drop(guard);
    match copied {
        StepOutcome::Done(2) | StepOutcome::Continue { .. } => {}
        other => panic!("copy mincore vec from user: {other:?}"),
    }
    assert_eq!(vec, [1, 0]);
}

/// Linux rejects a `mincore` query whose target range is unmapped.
#[test]
fn dispatch_mincore_unmapped_range_returns_neg_enomem() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let vec_addr = 0x3500_0000u64;
    let vec_map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                vec_addr,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(vec_map, SyscallResult::Return(vec_addr as i64));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MINCORE,
            [0x3600_0000, USER_PAGE_SIZE as u64, vec_addr, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_NOMEM));
}

/// `mlock2(..., MLOCK_ONFAULT)` is accepted under Tx's no-swap policy
/// and marks the covered VMA as locked for observability.
#[test]
fn dispatch_mlock2_onfault_marks_mapping_locked() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x3700_0000u64;
    let map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map, SyscallResult::Return(target as i64));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MLOCK2,
            [target, USER_PAGE_SIZE as u64, MLOCK_ONFAULT, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    let entry = ctx
        .aspace
        .lookup(UserVirtAddr(target as usize))
        .expect("mapped VMA");
    assert!(entry.flags.locked);
}

/// `mlock2` rejects unknown flag bits.
#[test]
fn dispatch_mlock2_unknown_flags_returns_neg_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MLOCK2,
            [0x3800_0000, USER_PAGE_SIZE as u64, 0x2, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `mlockall(MCL_CURRENT)` marks every current VMA locked under Tx's
/// no-swap observational-lock policy.
#[test]
fn dispatch_mlockall_current_marks_all_current_mappings_locked() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    for addr in [0x3900_0000u64, 0x3a00_0000u64] {
        let map = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    addr,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(map, SyscallResult::Return(addr as i64));
    }

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MLOCKALL, [MCL_CURRENT, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));

    for addr in [0x3900_0000usize, 0x3a00_0000usize] {
        let entry = ctx.aspace.lookup(UserVirtAddr(addr)).expect("mapped VMA");
        assert!(entry.flags.locked, "VMA at {addr:#x} should be locked");
    }
}

/// `munlockall()` clears lock state from every current VMA.
#[test]
fn dispatch_munlockall_clears_all_current_mapping_locks() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x3b00_0000u64;
    let map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map, SyscallResult::Return(target as i64));
    let lock = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MLOCKALL, [MCL_CURRENT, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(lock, SyscallResult::Return(0));
    assert!(
        ctx.aspace
            .lookup(UserVirtAddr(target as usize))
            .expect("mapped VMA")
            .flags
            .locked
    );

    let unlock = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MUNLOCKALL, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(unlock, SyscallResult::Return(0));
    assert!(
        !ctx.aspace
            .lookup(UserVirtAddr(target as usize))
            .expect("mapped VMA")
            .flags
            .locked
    );
}

/// `mlockall(MCL_FUTURE)` marks subsequently-created mappings locked;
/// `munlockall()` clears that process-local future policy.
#[test]
fn dispatch_mlockall_future_locks_later_mappings_until_munlockall() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let future_lock = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MLOCKALL, [MCL_FUTURE, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(future_lock, SyscallResult::Return(0));

    let locked_addr = 0x3c00_0000u64;
    let map_locked = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                locked_addr,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map_locked, SyscallResult::Return(locked_addr as i64));
    assert!(
        ctx.aspace
            .lookup(UserVirtAddr(locked_addr as usize))
            .expect("future-locked VMA")
            .flags
            .locked
    );

    let unlock = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MUNLOCKALL, [0, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(unlock, SyscallResult::Return(0));

    let unlocked_addr = 0x3d00_0000u64;
    let map_unlocked = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                unlocked_addr,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map_unlocked, SyscallResult::Return(unlocked_addr as i64));
    assert!(
        !ctx.aspace
            .lookup(UserVirtAddr(unlocked_addr as usize))
            .expect("post-munlockall VMA")
            .flags
            .locked
    );
}

/// `MCL_ONFAULT` is only valid when paired with MCL_CURRENT or MCL_FUTURE.
#[test]
fn dispatch_mlockall_onfault_without_target_returns_neg_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MLOCKALL, [MCL_ONFAULT, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `memfd_create` returns a VFS-shaped fd backed by an anonymous
/// PageContainer, so the fd can be truncated and mapped like a regular
/// PageBacked file.
#[test]
fn dispatch_memfd_create_returns_pagebacked_fd_that_mmaps() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap.clone(), thread);

    let name = b"ltp-mm\0";
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MEMFD_CREATE,
            [name.as_ptr() as u64, MFD_CLOEXEC, 0, 0, 0, 0],
        ),
        &ctx,
    ));
    let fd = match result {
        SyscallResult::Return(fd) => fd as u32,
        other => panic!("expected memfd fd, got {other:?}"),
    };
    assert!(proc_cap.fd_cloexec(fd), "MFD_CLOEXEC should set fd cloexec");

    let file = proc_cap.fd(fd).expect("memfd installed");
    assert!(
        matches!(file.rnode().backing(), RNodeBacking::PageBacked { .. }),
        "memfd must expose a PageBacked rnode"
    );

    let truncate = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_FTRUNCATE, [fd as u64, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(truncate, SyscallResult::Return(0));

    let target = 0x3c00_0000u64;
    let mapped = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_FIXED,
                fd as u64,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(mapped, SyscallResult::Return(target as i64));

    let published = block_on(ctx.aspace.fault_script(VmFault::new(
        UserVirtAddr(target as usize),
        AccessMode::Write,
    )))
    .expect("memfd mapping should fault through PageBacked");
    assert_eq!(
        published.page,
        UserVirtAddr(target as usize).containing_page()
    );
}

/// Unknown memfd flag bits are rejected before any fd is installed.
#[test]
fn dispatch_memfd_create_unknown_flags_returns_neg_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let name = b"badflag\0";
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MEMFD_CREATE, [name.as_ptr() as u64, 1 << 63, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

/// `MFD_ALLOW_SEALING` starts with no seals; `F_ADD_SEALS` accumulates
/// the requested bits and `F_SEAL_SEAL` closes the set.
#[test]
fn dispatch_memfd_fcntl_seals_round_trip_and_lock() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let name = b"seal-roundtrip\0";
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MEMFD_CREATE,
            [name.as_ptr() as u64, MFD_ALLOW_SEALING, 0, 0, 0, 0],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("expected memfd fd, got {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FCNTL, [fd, F_GET_SEALS as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [
                    fd,
                    F_ADD_SEALS as u64,
                    (F_SEAL_SHRINK | F_SEAL_SEAL) as u64,
                    0,
                    0,
                    0
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FCNTL, [fd, F_GET_SEALS as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return((F_SEAL_SHRINK | F_SEAL_SEAL) as i64),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [fd, F_ADD_SEALS as u64, F_SEAL_GROW as u64, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Error(E_PERM),
    );
}

/// Without `MFD_ALLOW_SEALING`, Linux creates the memfd with
/// `F_SEAL_SEAL` already present.
#[test]
fn dispatch_memfd_without_allow_sealing_starts_sealed() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let name = b"seal-default\0";
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_MEMFD_CREATE, [name.as_ptr() as u64, 0, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("expected memfd fd, got {other:?}"),
    };

    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FCNTL, [fd, F_GET_SEALS as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(F_SEAL_SEAL as i64),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [fd, F_ADD_SEALS as u64, F_SEAL_SHRINK as u64, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Error(E_PERM),
    );
}

/// Seal enforcement blocks writes and shrink/grow truncation according
/// to the installed seal mask.
#[test]
fn dispatch_memfd_seals_block_write_and_resize() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let name = b"seal-enforce\0";
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MEMFD_CREATE,
            [name.as_ptr() as u64, MFD_ALLOW_SEALING, 0, 0, 0, 0],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("expected memfd fd, got {other:?}"),
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FTRUNCATE, [fd, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [
                    fd,
                    F_ADD_SEALS as u64,
                    (F_SEAL_WRITE | F_SEAL_GROW | F_SEAL_SHRINK) as u64,
                    0,
                    0,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(0),
    );

    let bytes = b"x";
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_WRITE,
                [fd, bytes.as_ptr() as u64, bytes.len() as u64, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Error(E_PERM),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FTRUNCATE, [fd, (USER_PAGE_SIZE * 2) as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_PERM),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FTRUNCATE, [fd, 0, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Error(E_PERM),
    );
}

/// `F_SEAL_FUTURE_WRITE` rejects new shared writable mappings while
/// still allowing read-only shared mappings.
#[test]
fn dispatch_memfd_future_write_seal_blocks_writable_shared_mmap() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let name = b"seal-mmap\0";
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MEMFD_CREATE,
            [name.as_ptr() as u64, MFD_ALLOW_SEALING, 0, 0, 0, 0],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("expected memfd fd, got {other:?}"),
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FTRUNCATE, [fd, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [fd, F_ADD_SEALS as u64, F_SEAL_FUTURE_WRITE as u64, 0, 0, 0],
            ),
            &ctx,
        )),
        SyscallResult::Return(0),
    );

    let ro_target = 0x3c20_0000u64;
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    ro_target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ,
                    MAP_SHARED | MAP_FIXED,
                    fd,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(ro_target as i64),
    );

    let rw_target = 0x3c30_0000u64;
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    rw_target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_FIXED,
                    fd,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Error(E_PERM),
    );
}

/// `F_SEAL_WRITE` cannot be added while a writable shared mapping of
/// the memfd is already live in the caller address space.
#[test]
fn dispatch_memfd_write_seal_rejects_existing_writable_shared_mmap() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let name = b"seal-busy\0";
    let fd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MEMFD_CREATE,
            [name.as_ptr() as u64, MFD_ALLOW_SEALING, 0, 0, 0, 0],
        ),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("expected memfd fd, got {other:?}"),
    };
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_FTRUNCATE, [fd, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0),
    );

    let target = 0x3c40_0000u64;
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    target,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED | MAP_FIXED,
                    fd,
                    0,
                ],
            ),
            &ctx,
        )),
        SyscallResult::Return(target as i64),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [fd, F_ADD_SEALS as u64, F_SEAL_WRITE as u64, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Error(E_BUSY),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(NR_MUNMAP, [target, USER_PAGE_SIZE as u64, 0, 0, 0, 0]),
            &ctx,
        )),
        SyscallResult::Return(0),
    );
    assert_eq!(
        block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_FCNTL,
                [fd, F_ADD_SEALS as u64, F_SEAL_WRITE as u64, 0, 0, 0]
            ),
            &ctx,
        )),
        SyscallResult::Return(0),
    );
}

/// Phase-1 Tx is single-node, so the default memory policy is
/// MPOL_DEFAULT with an empty policy nodemask.
#[test]
fn dispatch_get_mempolicy_default_writes_policy_and_empty_mask() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let mut policy = -1i32;
    let mut mask = u64::MAX;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GET_MEMPOLICY,
            [
                (&mut policy as *mut i32) as u64,
                (&mut mask as *mut u64) as u64,
                64,
                0,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(policy, MPOL_DEFAULT as i32);
    assert_eq!(mask, 0);
}

/// `MPOL_F_MEMS_ALLOWED` reports the only online/allowed NUMA node.
#[test]
fn dispatch_get_mempolicy_mems_allowed_reports_node_zero() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let mut mask = 0u64;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_GET_MEMPOLICY,
            [
                0,
                (&mut mask as *mut u64) as u64,
                64,
                0,
                MPOL_F_MEMS_ALLOWED,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(mask, 1);
}

/// Default process policy is accepted as a no-op on the single-node model.
#[test]
fn dispatch_set_mempolicy_default_null_mask_returns_zero() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_SET_MEMPOLICY, [MPOL_DEFAULT, 0, 0, 0, 0, 0]),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
}

/// `mbind` accepts a default policy over a mapped range and records no
/// persistent NUMA policy in phase 1.
#[test]
fn dispatch_mbind_default_on_mapped_range_returns_zero() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x3d00_0000u64;
    let map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map, SyscallResult::Return(target as i64));

    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MBIND,
            [target, USER_PAGE_SIZE as u64, MPOL_DEFAULT, 0, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
}

/// On the phase-1 single-node model, migrating node 0 to node 0 is a no-op.
#[test]
fn dispatch_migrate_pages_node_zero_to_node_zero_returns_zero() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let old_nodes = 1u64;
    let new_nodes = 1u64;
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MIGRATE_PAGES,
            [
                0,
                64,
                (&old_nodes as *const u64) as u64,
                (&new_nodes as *const u64) as u64,
                0,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
}

/// Query-only `move_pages` reports every resident page on node 0.
#[test]
fn dispatch_move_pages_query_reports_node_zero_for_mapped_page() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x3e00_0000u64;
    let map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map, SyscallResult::Return(target as i64));

    let pages = [target];
    let mut status = [-1i32];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MOVE_PAGES,
            [
                0,
                1,
                pages.as_ptr() as u64,
                0,
                status.as_mut_ptr() as u64,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(0));
    assert_eq!(status, [0]);
}

/// Self-process `process_vm_readv` copies bytes from remote iovecs into
/// local iovecs without needing cross-process address-space lookup.
#[test]
fn dispatch_process_vm_readv_self_copies_remote_to_local() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let remote = 0x3f00_0000u64;
    let local = 0x3f10_0000u64;
    for addr in [remote, local] {
        let map = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    addr,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(map, SyscallResult::Return(addr as i64));
    }

    let seed_guard = guard();
    let seeded = ctx.aspace.copy_to_user(
        tx_hal::UserPtr::<u8>::new(remote as usize),
        b"read",
        &seed_guard,
    );
    drop(seed_guard);
    match seeded {
        StepOutcome::Done(4) | StepOutcome::Continue { .. } => {}
        other => panic!("seed remote process_vm_readv page: {other:?}"),
    }

    let local_iov = [local, 4u64];
    let remote_iov = [remote, 4u64];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_PROCESS_VM_READV,
            [
                0,
                local_iov.as_ptr() as u64,
                1,
                remote_iov.as_ptr() as u64,
                1,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(4));

    let mut copied = [0u8; 4];
    let read_guard = guard();
    let read = ctx.aspace.copy_from_user(
        &mut copied,
        tx_hal::UserPtr::<u8>::new(local as usize),
        &read_guard,
    );
    drop(read_guard);
    match read {
        StepOutcome::Done(4) | StepOutcome::Continue { .. } => {}
        other => panic!("read local process_vm_readv page: {other:?}"),
    }
    assert_eq!(&copied, b"read");
}

/// Self-process `process_vm_writev` copies bytes from local iovecs into
/// remote iovecs.
#[test]
fn dispatch_process_vm_writev_self_copies_local_to_remote() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let local = 0x3f20_0000u64;
    let remote = 0x3f30_0000u64;
    for addr in [local, remote] {
        let map = block_on(dispatch::<ShimsTestPmap>(
            SyscallRequest::new(
                NR_MMAP,
                [
                    addr,
                    USER_PAGE_SIZE as u64,
                    PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                    u64::MAX,
                    0,
                ],
            ),
            &ctx,
        ));
        assert_eq!(map, SyscallResult::Return(addr as i64));
    }

    let seed_guard = guard();
    let seeded = ctx.aspace.copy_to_user(
        tx_hal::UserPtr::<u8>::new(local as usize),
        b"write",
        &seed_guard,
    );
    drop(seed_guard);
    match seeded {
        StepOutcome::Done(5) | StepOutcome::Continue { .. } => {}
        other => panic!("seed local process_vm_writev page: {other:?}"),
    }

    let local_iov = [local, 5u64];
    let remote_iov = [remote, 5u64];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_PROCESS_VM_WRITEV,
            [
                0,
                local_iov.as_ptr() as u64,
                1,
                remote_iov.as_ptr() as u64,
                1,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(5));

    let mut copied = [0u8; 5];
    let read_guard = guard();
    let read = ctx.aspace.copy_from_user(
        &mut copied,
        tx_hal::UserPtr::<u8>::new(remote as usize),
        &read_guard,
    );
    drop(read_guard);
    match read {
        StepOutcome::Done(5) | StepOutcome::Continue { .. } => {}
        other => panic!("read remote process_vm_writev page: {other:?}"),
    }
    assert_eq!(&copied, b"write");
}

/// `remap_file_pages` rewrites a shared PageBacked VMA to point at a
/// different page offset within the same PageContainer.
#[test]
fn dispatch_remap_file_pages_shared_pagebacked_rewrites_backing_offset() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let file = pagebacked_open_file(4, (USER_PAGE_SIZE * 4) as u64);
    proc_cap.set_fd(17, Some(file));
    let ctx = make_ctx(proc_cap, thread);

    let target = 0x3f40_0000u64;
    let map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_FIXED,
                17,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map, SyscallResult::Return(target as i64));

    let remap = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_REMAP_FILE_PAGES,
            [target, USER_PAGE_SIZE as u64, 0, 2, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(remap, SyscallResult::Return(0));

    let entry = ctx
        .aspace
        .lookup(UserVirtAddr(target as usize))
        .expect("remapped VMA");
    match entry.backing {
        VmBacking::Page { offset, .. } => assert_eq!(offset, (USER_PAGE_SIZE * 2) as u64),
        other => panic!("expected PageBacked remap, got {other:?}"),
    }
}

/// `process_madvise` applies advice to the process referenced by a
/// pidfd. The first supported slice covers self pidfds and reuses the
/// existing AddressSpace::madvise behavior.
#[test]
fn dispatch_process_madvise_self_pidfd_returns_advised_bytes() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let pid = proc_cap.pid.0;
    let ctx = make_ctx(proc_cap, thread);

    let pidfd = match block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(NR_PIDFD_OPEN, [pid as u64, 0, 0, 0, 0, 0]),
        &ctx,
    )) {
        SyscallResult::Return(fd) => fd as u64,
        other => panic!("expected pidfd, got {other:?}"),
    };

    let target = 0x3f50_0000u64;
    let map = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_MMAP,
            [
                target,
                USER_PAGE_SIZE as u64,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
                u64::MAX,
                0,
            ],
        ),
        &ctx,
    ));
    assert_eq!(map, SyscallResult::Return(target as i64));

    let iov = [target, USER_PAGE_SIZE as u64];
    let result = block_on(dispatch::<ShimsTestPmap>(
        SyscallRequest::new(
            NR_PROCESS_MADVISE,
            [pidfd, iov.as_ptr() as u64, 1, MADV_DONTNEED, 0, 0],
        ),
        &ctx,
    ));
    assert_eq!(result, SyscallResult::Return(USER_PAGE_SIZE as i64));
}

/// `madvise(.., 99)` returns -ENOSYS for unsupported advice
/// values. The slice maps the documented six values
/// (NORMAL/RANDOM/SEQUENTIAL/WILLNEED/DONTNEED/FREE); any other
/// raw value is rejected.
#[test]
fn dispatch_madvise_unsupported_advice_returns_neg_enosys() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    // Use addr 0 + length PAGE; the advice rejection happens
    // before range parsing, so we don't need a real mapping.
    let target = 0x1_0000_0000u64;
    let req = SyscallRequest::new(NR_MADVISE, [target, USER_PAGE_SIZE as u64, 99, 0, 0, 0]);
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_NOSYS));
}

/// `mmap` with unknown PROT bits (high bit, not in the recognised
/// set) returns -EINVAL.
#[test]
fn dispatch_mmap_with_unknown_prot_bits_returns_neg_einval() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [
            0,
            USER_PAGE_SIZE as u64,
            0x8000, // unknown bit, not in PROT_READ/WRITE/EXEC/NONE/GROWSDOWN/GROWSUP
            MAP_PRIVATE | MAP_ANONYMOUS,
            u64::MAX,
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_INVAL));
}

// E_BADF kept reachable for fd-required mmap shapes — assert
// the no-anonymous + bad fd path returns -EBADF (not -ENODEV)
// when fd is negative.
#[test]
fn dispatch_mmap_file_backed_with_negative_fd_returns_neg_ebadf() {
    let _setup = vm_setup();
    let (proc_cap, thread) = fresh_proc_thread();
    let ctx = make_ctx(proc_cap, thread);

    let req = SyscallRequest::new(
        NR_MMAP,
        [
            0,
            USER_PAGE_SIZE as u64,
            PROT_READ,
            MAP_PRIVATE,
            u64::MAX, // fd = -1, no MAP_ANONYMOUS
            0,
        ],
    );
    let result = block_on(dispatch::<ShimsTestPmap>(req, &ctx));
    assert_eq!(result, SyscallResult::Error(E_BADF));
}
