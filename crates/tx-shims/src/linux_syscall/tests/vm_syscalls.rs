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
use tx_subsystems::vm::{AccessMode, UserVirtAddr, VmFault, USER_PAGE_SIZE};

use crate::linux_syscall::{
    MADV_DONTNEED, MAP_ANONYMOUS, MAP_FIXED, MAP_FIXED_NOREPLACE, MAP_PRIVATE, MAP_SHARED,
    MREMAP_FIXED, MREMAP_MAYMOVE, NR_MADVISE, NR_MMAP, NR_MPROTECT, NR_MREMAP, NR_MUNMAP,
    PROT_READ, PROT_WRITE,
};

const E_BADF: i32 = 9;
const E_EXIST: i32 = 17;
const E_INVAL: i32 = 22;
const E_NOMEM: i32 = 12;
const E_NODEV: i32 = 19;
const E_NOSYS: i32 = 38;

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
