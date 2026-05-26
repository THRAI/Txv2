//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, Errno as V3Errno, StepOutcome};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use tx_hal::UserPtr;
use tx_scripts::drive;
use tx_substrate::step::DriveMode;
use tx_subsystems::vm::step_ops::{
    VmBrkOp, VmMapOp, VmMlockOp, VmMsyncOp, VmMunlockOp, VmProtectOp, VmRemapOp, VmUnmapOp,
};

const MEMFD_NAME_MAX: usize = 249;
const MFD_HUGE_MASK: u64 = 0x3f << 26;

static NEXT_MEMFD_FS_OBJECT_ID: AtomicU64 = AtomicU64::new(0x6d66_6400);

/// `brk(requested)` per `txdoc:VM-5-8-BRK`.
///
/// - `requested == 0`: report the current break (Linux's "brk(0)
///   returns current_brk" idiom; matches glibc's `__sbrk(0)` probe).
/// - On any error from `brk_script` (including `InvalidRange` for
///   `requested < brk_base`): return the *unchanged* current break.
///   Linux's brk(2) **never** returns a negative errno; on failure
///   userspace observes "the break didn't move" and is responsible
///   for noticing.
pub(super) async fn sys_brk(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let requested = args[0];

    let brk_base = ctx.process.brk_base();
    let current_brk = ctx.process.current_brk();

    // Requested == 0 is the "report current" idiom; never call into
    // the script (which would treat zero as `requested < brk_base`
    // and return InvalidRange).
    if requested == 0 {
        return SyscallResult::Return(current_brk as i64);
    }

    let base = UserVirtAddr(brk_base as usize);
    let cur = UserVirtAddr(current_brk as usize);
    let req = UserVirtAddr(requested as usize);

    let op = VmBrkOp {
        aspace: &ctx.aspace,
        brk_base: base,
        current_brk: cur,
        requested_brk: req,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(new_brk) => {
            ctx.process.set_current_brk(new_brk.0 as u64);
            SyscallResult::Return(new_brk.0 as i64)
        }
        Err(_) => {
            // Linux: brk(2) never returns -errno. On failure (range
            // below brk_base, OOM, mapping conflict) report the
            // unchanged current break. Userspace detects "no
            // movement" by comparing against the prior break.
            SyscallResult::Return(current_brk as i64)
        }
    }
}

// =====================================================================
// Slice 2 of the shell-prompt roadmap — VM syscalls.
//
// `mmap` / `munmap` / `mprotect` / `mremap` / `madvise` / `msync` are
// pure plumbing on top of the VM execution primitives landed earlier
// (`AddressSpace::try_mmap`, `try_munmap`, `try_mprotect`, `try_mremap`,
// `madvise`, `msync`). The arms below decode the Linux PROT_* / MAP_* /
// MADV_* / MREMAP_* / MS_* flag bytes into the subsystem-shared `Prot` /
// `VmEntryFlags` / `MapPlacement` / `MadviseAdvice` / `VmBacking`
// shapes, build the request value, and dispatch — there is no
// step-loop discipline because the inner VM steps are themselves
// synchronous (msync is the lone exception, awaiting `step_fsync` per
// File-backed page container).
//
// User-VA discipline: `addr` (mmap/munmap/mprotect/madvise) is an
// integer hint, not a dereferenced pointer; msync's `addr/length`
// shape only walks existing recipes and never dereferences the user
// VA either. No user-VA copy lane is invoked from these arms.
//
// See `docs/progress/plans/2026-05-07-shell-prompt-roadmap.md` Slice 2.
// =====================================================================

/// `mmap(addr, length, prot, flags, fd, offset)` — Linux RV64 generic
/// syscall #222.
///
/// Decodes Linux `PROT_*` / `MAP_*` into the subsystem-shared `Prot` /
/// `VmEntryFlags` shapes, builds a `VmBacking` (`PrivateAnon` for
/// `MAP_ANONYMOUS`; `Page { pc, offset }` for file-backed via the fd's
/// resolved `RNodeBacking::PageBacked`), and dispatches to
/// `AddressSpace::try_mmap`. Returns the chosen user VA on success
/// (matches Linux's `void *mmap(...)` shape — caller treats negative
/// returns as `-errno`).
///
/// Slice 2 scope (2026-05-07):
///
/// - `PROT_NONE` is the zero pattern (`Prot::NONE`); `PROT_GROWSDOWN` /
///   `PROT_GROWSUP` are recognised but return `-ENOSYS` (the underlying
///   `Prot` value type has no equivalent and they pair with
///   `MAP_GROWSDOWN`, which is rare in practice).
/// - Exactly one of `MAP_SHARED` / `MAP_PRIVATE` is required.
/// - `MAP_FIXED` → `MapPlacement::FixedReplace` (silently overwrites).
/// - `MAP_FIXED_NOREPLACE` → `MapPlacement::RequireFree` at the
///   requested addr; overlap returns `-EEXIST`.
/// - `MAP_ANONYMOUS` without `MAP_PRIVATE | MAP_SHARED` rejects
///   (`-EINVAL`). With either, the backing is `VmBacking::PrivateAnon`
///   regardless of shared/private (Slice 2 does not yet model shared
///   anon as distinct).
/// - `MAP_HUGETLB` / `MAP_POPULATE` / `MAP_STACK` etc. are recognised
///   but ignored (best-effort hints). `MAP_LOCKED` and process-local
///   `mlockall(MCL_FUTURE)` are represented by `VmEntryFlags.locked`.
/// - File-backed mmap requires `fd` to resolve to an `OpenFile` whose
///   rnode is `RNodeBacking::PageBacked` — TTY / pipe / chardev /
///   directory / symlink → `-ENODEV`.
pub(super) async fn sys_mmap(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let prot_bits = args[2];
    let flags = args[3];
    let fd = args[4] as i32;
    let offset = args[4 + 1]; // args[5]

    // Length validation. Linux rounds the byte length up to a whole
    // page; addr (when MAP_FIXED is set) must already be page-aligned.
    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    // Decode `prot`. Only the documented bits are accepted; anything
    // else is `-EINVAL`. PROT_GROWSDOWN/GROWSUP recognised but
    // unsupported (`-ENOSYS`) — the underlying `Prot` shape has no
    // equivalent.
    let prot_recognised =
        PROT_READ | PROT_WRITE | PROT_EXEC | PROT_NONE | PROT_GROWSDOWN | PROT_GROWSUP;
    if prot_bits & !prot_recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if prot_bits & (PROT_GROWSDOWN | PROT_GROWSUP) != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    let prot = Prot::new(
        prot_bits & PROT_READ != 0,
        prot_bits & PROT_WRITE != 0,
        prot_bits & PROT_EXEC != 0,
    );

    // Decode `flags`. Exactly one of MAP_SHARED / MAP_PRIVATE required.
    let private = flags & MAP_PRIVATE != 0;
    let shared = flags & MAP_SHARED != 0;
    if private == shared {
        // both unset, or both set
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let fixed = flags & MAP_FIXED != 0;
    let fixed_noreplace = flags & MAP_FIXED_NOREPLACE != 0;
    let anonymous = flags & MAP_ANONYMOUS != 0;
    // MAP_GROWSDOWN: stack-expansion hint. txKernel has no stack
    // expansion (no `expand_stack` script), so reject it explicitly
    // rather than silently setting `VmEntryFlags.grows_down` with no
    // observable effect.
    if flags & MAP_GROWSDOWN != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let entry_flags = VmEntryFlags::new(
        shared,
        flags & MAP_GROWSDOWN != 0,
        flags & MAP_LOCKED != 0 || ctx.process.mlock_future(),
    );

    // Build the backing.
    let backing = if anonymous {
        if shared {
            let page_count = (length / USER_PAGE_SIZE) as u64;
            let pc = match PageContainer::new_cap(
                PageContainerKind::Anon {
                    swap_policy: AnonSwapPolicy::Reclaimable,
                },
                page_count,
            ) {
                Ok(pc) => pc,
                Err(_) => return SyscallResult::Error(errno_to_i32(Errno::ENOMEM)),
            };
            VmBacking::Page { pc, offset: 0 }
        } else {
            VmBacking::PrivateAnon
        }
    } else {
        if fd < 0 {
            return SyscallResult::Error(EBADF_VALUE);
        }
        let file = match resolve_fd(&ctx.process, fd as u32) {
            Some(f) => f,
            None => return SyscallResult::Error(EBADF_VALUE),
        };
        if shared
            && prot.write
            && (file.has_memfd_seal(F_SEAL_WRITE) || file.has_memfd_seal(F_SEAL_FUTURE_WRITE))
        {
            return SyscallResult::Error(EPERM_VALUE);
        }
        match extract_page_container(&file) {
            Some(pc) => VmBacking::Page { pc, offset },
            None => return SyscallResult::error_from(Errno::ENODEV),
        }
    };

    // Build the request. MAP_FIXED → FixedReplace; MAP_FIXED_NOREPLACE
    // → RequireFree at the requested addr; otherwise Anywhere over the
    // full V1 user range.
    let request = if fixed || fixed_noreplace {
        if !UserVirtAddr::new(addr as usize).is_page_aligned() {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
            Ok(r) => r,
            Err(_) => return SyscallResult::Error(EINVAL_VALUE),
        };
        let placement = if fixed_noreplace {
            MapPlacement::RequireFree
        } else {
            MapPlacement::FixedReplace
        };
        VmMapRequest::fixed(range, placement, prot, entry_flags, backing)
    } else {
        // Linux mmap(addr=NULL, !MAP_FIXED) returns a chosen mapping
        // address, and user programs commonly treat NULL as failure or
        // a sentinel. Keep page 0 unmapped for syscall-allocated
        // mappings while leaving the lower VM layer's full-user range
        // semantics unchanged for fixed/exec paths.
        let window = match UserRange::new_aligned(
            UserVirtAddr(USER_PAGE_SIZE),
            UserRange::full_user_v1().len() - USER_PAGE_SIZE,
        ) {
            Ok(range) => range,
            Err(_) => return SyscallResult::Error(EINVAL_VALUE),
        };
        let page_count = length / USER_PAGE_SIZE;
        VmMapRequest::anywhere(window, page_count, prot, entry_flags, backing)
    };

    let op = VmMapOp {
        aspace: &ctx.aspace,
        request,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(outcome) => SyscallResult::Return(outcome.range.start().as_usize() as i64),
        Err(errno) => {
            // MAP_FIXED_NOREPLACE → AlreadyMapped maps to EEXIST per
            // Linux's distinct semantic for that flag.
            let code = if fixed_noreplace && errno == crate::adapter::step_engine::Errno::EEXIST {
                errno_to_i32(Errno::EEXIST)
            } else {
                errno_to_i32(Into::<Errno>::into(errno))
            };
            SyscallResult::Error(code)
        }
    }
}

/// `munmap(addr, length)` — Linux RV64 generic syscall #215.
///
/// `addr` must be page-aligned and `length` is rounded up to a whole
/// page (matching Linux). Wraps `AddressSpace::try_munmap`.
pub(super) async fn sys_munmap(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    let op = VmUnmapOp {
        aspace: &ctx.aspace,
        range,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(_commit) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}

/// `mlock(addr, len)` — Linux RV64 generic syscall #228.
///
/// Under no-swap, mlock is purely observational: it sets
/// `VmEntryFlags.locked` on every VMA overlapping the range for
/// `/proc/<pid>/maps` reporting. No pages are faulted in; the flag is
/// a hint that survives across fork (the child inherits locked flags
/// via recipe cloning).
///
/// `addr` is rounded down to a page boundary; `len` is rounded up.
/// Returns 0 on success, `-errno` on failure.
pub(super) async fn sys_mlock(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let len_in = args[1] as usize;

    if len_in == 0 {
        return SyscallResult::Return(0);
    }
    let start = addr & !(USER_PAGE_SIZE as u64 - 1);
    let Some(len) = len_in.checked_next_multiple_of(USER_PAGE_SIZE) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    let Ok(range) = UserRange::new_aligned(UserVirtAddr(start as usize), len) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let op = VmMlockOp {
        aspace: &ctx.aspace,
        range,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(_commit) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}

/// `mlock2(addr, len, flags)` — Linux RV64 generic syscall #284.
///
/// Tx has no swap, so `MLOCK_ONFAULT` and eager locking collapse to the
/// same observational VMA flag. Unknown flags are rejected per Linux.
pub(super) async fn sys_mlock2(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let flags = args[2];
    if flags & !MLOCK_ONFAULT != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    sys_mlock(args, ctx).await
}

/// `mlockall(flags)` — Linux RV64 generic syscall #230.
///
/// Under Tx's no-swap policy this is an observational VMA-flag update:
/// `MCL_CURRENT` marks every current recipe locked. `MCL_FUTURE` sets a
/// process-local policy so later mappings are born locked.
pub(super) async fn sys_mlockall(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let flags = args[0];
    let known = MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT;
    if flags & !known != 0 || flags & (MCL_CURRENT | MCL_FUTURE) == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    if flags & MCL_CURRENT != 0 {
        match set_all_current_mlock(ctx, true).await {
            Ok(()) => {}
            Err(errno) => return SyscallResult::error_from(Into::<Errno>::into(errno)),
        }
    }
    if flags & MCL_FUTURE != 0 {
        ctx.process.set_mlock_future(true);
    }

    SyscallResult::Return(0)
}

/// `munlock(addr, len)` — Linux RV64 generic syscall #229.
///
/// Clears `VmEntryFlags.locked` on every VMA overlapping the range.
/// Same rounding and error semantics as `sys_mlock`.
pub(super) async fn sys_munlock(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let len_in = args[1] as usize;

    if len_in == 0 {
        return SyscallResult::Return(0);
    }
    let start = addr & !(USER_PAGE_SIZE as u64 - 1);
    let Some(len) = len_in.checked_next_multiple_of(USER_PAGE_SIZE) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    let Ok(range) = UserRange::new_aligned(UserVirtAddr(start as usize), len) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };

    let op = VmMunlockOp {
        aspace: &ctx.aspace,
        range,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(_commit) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}

/// `memfd_create(name, flags)` — Linux RV64 generic syscall #279.
///
/// Implements the PageBacked v1 contract: a memfd is an anonymous
/// `PageContainer` wrapped by a synthetic pathless regular-file RNode
/// and installed as a read/write fd. `MFD_ALLOW_SEALING` controls the
/// initial seal set: without it, Linux starts the file with
/// `F_SEAL_SEAL`; with it, callers can add seals via fcntl.
pub(super) fn sys_memfd_create(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let name_uaddr = args[0];
    let flags = args[1];
    let known_flags =
        MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_HUGETLB | MFD_NOEXEC_SEAL | MFD_EXEC | MFD_HUGE_MASK;
    if flags & !known_flags != 0
        || flags & (MFD_NOEXEC_SEAL | MFD_EXEC) == (MFD_NOEXEC_SEAL | MFD_EXEC)
    {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & (MFD_HUGETLB | MFD_HUGE_MASK) != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    if name_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    let name = match bootstrap_read_user_cstr(&ctx.aspace, name_uaddr, MEMFD_NAME_MAX + 1) {
        Ok(name) => name,
        Err(Errno::ENAMETOOLONG) => return SyscallResult::Error(EINVAL_VALUE),
        Err(_) => return SyscallResult::Error(EFAULT_VALUE),
    };
    if name.len() > MEMFD_NAME_MAX {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    let memfd_capacity_pages = (FULL_USER_V1_TOP as u64) / (USER_PAGE_SIZE as u64);
    let pc = match PageContainer::new_cap(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        memfd_capacity_pages,
    ) {
        Ok(pc) => pc,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    pc.set_size_bytes(0);
    let rnode = match tx_subsystems::vfs::RNode::new_cap(
        tx_subsystems::vfs::FsObjectId::new(
            NEXT_MEMFD_FS_OBJECT_ID.fetch_add(1, Ordering::Relaxed),
        ),
        InodeMeta::new(InodeKind::Regular, 0o100600),
        RNodeBacking::PageBacked { pc },
    ) {
        Ok(rnode) => rnode,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let initial_seals = if flags & MFD_ALLOW_SEALING != 0 {
        0
    } else {
        F_SEAL_SEAL
    };
    let file = match OpenFile::new_memfd_cap(
        rnode,
        OpenFileFlags {
            read: true,
            write: true,
            append: false,
            cloexec: flags & MFD_CLOEXEC != 0,
            nonblocking: false,
        },
        initial_seals,
    ) {
        Ok(file) => file,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let fd = ctx.process.allocate_fd();
    let (soft_limit, _) = ctx.process.rlimit_nofile();
    if fd >= soft_limit {
        return SyscallResult::Error(EMFILE_VALUE);
    }
    let _ = ctx.process.install_fd(fd, file);
    if flags & MFD_CLOEXEC != 0 {
        ctx.process.set_fd_cloexec(fd, true);
    }

    SyscallResult::Return(fd as i64)
}

/// `get_mempolicy(policy, nodemask, maxnode, addr, flags)` — Linux
/// RV64 generic syscall #236.
///
/// Tx phase 1 has one memory node and no persistent NUMA policy. This
/// reports `MPOL_DEFAULT` for policy queries and node mask `{0}` for
/// `MPOL_F_MEMS_ALLOWED`.
pub(super) fn sys_get_mempolicy(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let policy_uaddr = args[0];
    let nodemask_uaddr = args[1];
    let maxnode = args[2];
    let addr = args[3];
    let flags = args[4];

    let known_flags = MPOL_F_ADDR | MPOL_F_NODE | MPOL_F_MEMS_ALLOWED;
    if flags & !known_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & MPOL_F_MEMS_ALLOWED != 0 && flags != MPOL_F_MEMS_ALLOWED {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & MPOL_F_NODE != 0 && flags & MPOL_F_ADDR == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & MPOL_F_ADDR != 0 && ctx.aspace.lookup(UserVirtAddr(addr as usize)).is_none() {
        return SyscallResult::Error(EFAULT_VALUE);
    }

    if policy_uaddr != 0 {
        let value = if flags & MPOL_F_NODE != 0 {
            0
        } else {
            MPOL_DEFAULT as i32
        };
        if let Err(errno) = bootstrap_write_user::<i32>(&ctx.aspace, policy_uaddr, value) {
            return SyscallResult::error_from(errno);
        }
    }
    if nodemask_uaddr != 0 && maxnode > 0 {
        let mask = if flags & MPOL_F_MEMS_ALLOWED != 0 {
            1u64
        } else {
            0u64
        };
        if let Err(errno) = bootstrap_write_user::<u64>(&ctx.aspace, nodemask_uaddr, mask) {
            return SyscallResult::error_from(errno);
        }
    }

    SyscallResult::Return(0)
}

/// `set_mempolicy(mode, nodemask, maxnode)` — Linux RV64 generic
/// syscall #237.
///
/// Accepts Linux policy modes that can collapse onto Tx's single node.
/// The policy is not persisted because phase 1 has no NUMA allocator.
pub(super) fn sys_set_mempolicy(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    match validate_single_node_policy(args[0], args[1], args[2], ctx) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno),
    }
}

/// `mbind(start, len, mode, nodemask, maxnode, flags)` — Linux RV64
/// generic syscall #235.
///
/// Validates the target range and single-node policy, then records no
/// persistent binding because Tx phase 1 has no NUMA placement state.
pub(super) fn sys_mbind(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let len_in = args[1] as usize;
    let mode = args[2];
    let nodemask_uaddr = args[3];
    let maxnode = args[4];
    let flags = args[5];

    let known_flags = MPOL_MF_STRICT | MPOL_MF_MOVE | MPOL_MF_MOVE_ALL;
    if flags & !known_flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if len_in == 0 {
        return SyscallResult::Return(0);
    }
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let Some(len) = len_in.checked_next_multiple_of(USER_PAGE_SIZE) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    let Ok(range) = UserRange::new_aligned(UserVirtAddr(addr as usize), len) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    if !range_fully_mapped(ctx, range) {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    match validate_single_node_policy(mode, nodemask_uaddr, maxnode, ctx) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::Error(errno),
    }
}

/// `migrate_pages(pid, maxnode, old_nodes, new_nodes)` — Linux RV64
/// generic syscall #238.
///
/// Tx phase 1 has one memory node, so node-0 to node-0 migration is a
/// no-op and returns zero pages migrated.
pub(super) fn sys_migrate_pages(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let pid = args[0] as i64;
    let maxnode = args[1];
    let old_nodes_uaddr = args[2];
    let new_nodes_uaddr = args[3];

    if let Err(errno) = validate_process_target(pid, ctx) {
        return SyscallResult::Error(errno);
    }
    let old_nodes = match read_single_node_mask(old_nodes_uaddr, maxnode, ctx) {
        Ok(mask) => mask,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let new_nodes = match read_single_node_mask(new_nodes_uaddr, maxnode, ctx) {
        Ok(mask) => mask,
        Err(errno) => return SyscallResult::Error(errno),
    };
    if old_nodes & !1 != 0 || new_nodes & !1 != 0 || new_nodes == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    SyscallResult::Return(0)
}

/// `move_pages(pid, nr_pages, pages, nodes, status, flags)` — Linux
/// RV64 generic syscall #239.
///
/// Supports Tx's single-node query path and node-0 no-op moves for the
/// current process. Each mapped page reports status node `0`.
pub(super) fn sys_move_pages(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let pid = args[0] as i64;
    let nr_pages = args[1] as usize;
    let pages_uaddr = args[2];
    let nodes_uaddr = args[3];
    let status_uaddr = args[4];
    let flags = args[5];

    if flags & !(MPOL_MF_MOVE | MPOL_MF_MOVE_ALL) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if let Err(errno) = validate_process_target(pid, ctx) {
        return SyscallResult::Error(errno);
    }
    if nr_pages == 0 {
        return SyscallResult::Return(0);
    }
    if pages_uaddr == 0 || status_uaddr == 0 {
        return SyscallResult::Error(EFAULT_VALUE);
    }
    if nr_pages > 4096 {
        return SyscallResult::Error(EINVAL_VALUE);
    }

    for index in 0..nr_pages {
        let page_ptr_uaddr = pages_uaddr + (index * core::mem::size_of::<u64>()) as u64;
        let page = match bootstrap_read_user::<u64>(&ctx.aspace, page_ptr_uaddr) {
            Ok(page) => page,
            Err(errno) => return SyscallResult::error_from(errno),
        };
        if nodes_uaddr != 0 {
            let node_uaddr = nodes_uaddr + (index * core::mem::size_of::<i32>()) as u64;
            let node = match bootstrap_read_user::<i32>(&ctx.aspace, node_uaddr) {
                Ok(node) => node,
                Err(errno) => return SyscallResult::error_from(errno),
            };
            if node != 0 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
        }
        let status = if ctx.aspace.lookup(UserVirtAddr(page as usize)).is_some() {
            0
        } else {
            -EFAULT_VALUE
        };
        let status_slot = status_uaddr + (index * core::mem::size_of::<i32>()) as u64;
        if let Err(errno) = bootstrap_write_user::<i32>(&ctx.aspace, status_slot, status) {
            return SyscallResult::error_from(errno);
        }
    }

    SyscallResult::Return(0)
}

/// `process_vm_readv(pid, local_iov, liovcnt, remote_iov, riovcnt,
/// flags)` — Linux RV64 generic syscall #270.
///
/// Phase 1 supports only the current process (`pid == 0` or the
/// caller pid), which collapses both iovec arrays onto `ctx.aspace`.
/// Cross-process address-space lookup and ptrace/credential permission
/// checks are deferred to the process/cred integration slice.
pub(super) fn sys_process_vm_readv(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    sys_process_vm_transfer(args, ctx, ProcessVmDirection::Read)
}

/// `process_vm_writev(pid, local_iov, liovcnt, remote_iov, riovcnt,
/// flags)` — Linux RV64 generic syscall #271. See
/// `sys_process_vm_readv` for the phase-1 self-process scope.
pub(super) fn sys_process_vm_writev(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    sys_process_vm_transfer(args, ctx, ProcessVmDirection::Write)
}

/// `process_madvise(pidfd, vec, vlen, behavior, flags)` — Linux RV64
/// generic syscall #440.
///
/// The first supported slice resolves real pidfd-backed `OpenFile`s and
/// applies the existing VM advice operation to the target process's
/// current address space. Permission policy is intentionally narrow:
/// only the caller's own process is accepted until ptrace/cred checks
/// are modeled.
pub(super) fn sys_process_madvise(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let pidfd = args[0] as u32;
    let iov_uaddr = args[1];
    let iovcnt = args[2];
    let advice_raw = args[3];
    let flags = args[4];

    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let Some(target_process) = resolve_pidfd_process(ctx, pidfd) else {
        return SyscallResult::Error(EBADF_VALUE);
    };
    if target_process.pid.0 != ctx.process.pid.0 {
        return SyscallResult::Error(EPERM_VALUE);
    }
    let Some(target_aspace) = target_process.aspace_cap() else {
        return SyscallResult::Error(ESRCH_VALUE);
    };
    let advice = match decode_madvise_advice(advice_raw) {
        Ok(advice) => advice,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let iovs = match read_process_vm_iovs(iov_uaddr, iovcnt, ctx) {
        Ok(iovs) => iovs,
        Err(errno) => return SyscallResult::Error(errno),
    };

    let mut total = 0i64;
    for iov in iovs {
        if iov.len == 0 {
            continue;
        }
        if !UserVirtAddr::new(iov.base as usize).is_page_aligned() {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let len = match iov.len.checked_next_multiple_of(USER_PAGE_SIZE) {
            Some(len) => len,
            None => return SyscallResult::Error(EINVAL_VALUE),
        };
        let range = match UserRange::new_aligned(UserVirtAddr::new(iov.base as usize), len) {
            Ok(range) => range,
            Err(_) => {
                if total > 0 {
                    return SyscallResult::Return(total);
                }
                return SyscallResult::Error(EINVAL_VALUE);
            }
        };
        if let Err(error) = target_aspace.madvise(range, advice) {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(vmmap_error_to_i32(error));
        }
        total += iov.len as i64;
    }

    SyscallResult::Return(total)
}

/// `munlockall()` — Linux RV64 generic syscall #231.
///
/// Clears the observational lock flag from every current VMA and clears
/// the process-local future-lock policy.
pub(super) async fn sys_munlockall(ctx: &SyscallCtx<'_>) -> SyscallResult {
    ctx.process.set_mlock_future(false);
    match set_all_current_mlock(ctx, false).await {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}

async fn set_all_current_mlock(ctx: &SyscallCtx<'_>, locked: bool) -> Result<(), V3Errno> {
    let entries = ctx.aspace.recipes_snapshot();
    for entry in entries {
        let mut script_ctx = build_subject_script_ctx(ctx);
        let mailbox_arc = script_ctx.mailbox().cloned();
        let timer_wheel_arc = script_ctx.timer_wheel().cloned();
        let delegate_registry_arc = script_ctx.delegate_registry().cloned();

        if locked {
            let op = VmMlockOp {
                aspace: &ctx.aspace,
                range: entry.range,
            };
            drive(
                op,
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_wheel_arc.as_ref(),
            )
            .await?;
        } else {
            let op = VmMunlockOp {
                aspace: &ctx.aspace,
                range: entry.range,
            };
            drive(
                op,
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_wheel_arc.as_ref(),
            )
            .await?;
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
struct ProcessVmIov {
    base: u64,
    len: usize,
}

#[derive(Clone, Copy)]
enum ProcessVmDirection {
    Read,
    Write,
}

fn sys_process_vm_transfer(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
    direction: ProcessVmDirection,
) -> SyscallResult {
    let pid = args[0] as i64;
    let local_iov_uaddr = args[1];
    let local_iovcnt = args[2];
    let remote_iov_uaddr = args[3];
    let remote_iovcnt = args[4];
    let flags = args[5];

    if flags != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if let Err(errno) = validate_process_target(pid, ctx) {
        return SyscallResult::Error(errno);
    }

    let local_iovs = match read_process_vm_iovs(local_iov_uaddr, local_iovcnt, ctx) {
        Ok(iovs) => iovs,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let remote_iovs = match read_process_vm_iovs(remote_iov_uaddr, remote_iovcnt, ctx) {
        Ok(iovs) => iovs,
        Err(errno) => return SyscallResult::Error(errno),
    };

    copy_process_vm_iovs(ctx, &local_iovs, &remote_iovs, direction)
}

fn read_process_vm_iovs(
    iov_uaddr: u64,
    iovcnt: u64,
    ctx: &SyscallCtx<'_>,
) -> Result<Vec<ProcessVmIov>, i32> {
    if iovcnt > 1024 {
        return Err(EINVAL_VALUE);
    }
    if iovcnt == 0 {
        return Ok(Vec::new());
    }

    const IOVEC_BYTES: u64 = 16;
    let mut iovs = Vec::with_capacity(iovcnt as usize);
    let mut total: i64 = 0;
    for index in 0..iovcnt {
        let Some(ent_ptr) = iov_uaddr.checked_add(index.saturating_mul(IOVEC_BYTES)) else {
            return Err(EINVAL_VALUE);
        };
        let mut ent_bytes = [0u8; IOVEC_BYTES as usize];
        bootstrap_copy_from_user(&ctx.aspace, &mut ent_bytes, ent_ptr).map_err(errno_to_i32)?;
        let base = u64::from_le_bytes(ent_bytes[0..8].try_into().unwrap());
        let len_raw = u64::from_le_bytes(ent_bytes[8..16].try_into().unwrap());
        let len = usize::try_from(len_raw).map_err(|_| EINVAL_VALUE)?;
        let len_i64 = i64::try_from(len).map_err(|_| EINVAL_VALUE)?;
        total = total.checked_add(len_i64).ok_or(EINVAL_VALUE)?;
        iovs.push(ProcessVmIov { base, len });
    }
    Ok(iovs)
}

fn copy_process_vm_iovs(
    ctx: &SyscallCtx<'_>,
    local_iovs: &[ProcessVmIov],
    remote_iovs: &[ProcessVmIov],
    direction: ProcessVmDirection,
) -> SyscallResult {
    const CHUNK_MAX: usize = USER_PAGE_SIZE;

    let mut local_index = 0usize;
    let mut remote_index = 0usize;
    let mut local_offset = 0usize;
    let mut remote_offset = 0usize;
    let mut total = 0i64;
    let mut chunk = Vec::new();

    while local_index < local_iovs.len() && remote_index < remote_iovs.len() {
        if local_offset == local_iovs[local_index].len {
            local_index += 1;
            local_offset = 0;
            continue;
        }
        if remote_offset == remote_iovs[remote_index].len {
            remote_index += 1;
            remote_offset = 0;
            continue;
        }

        let local = local_iovs[local_index];
        let remote = remote_iovs[remote_index];
        let chunk_len = (local.len - local_offset)
            .min(remote.len - remote_offset)
            .min(CHUNK_MAX);
        if chunk_len == 0 {
            continue;
        }

        chunk.resize(chunk_len, 0);
        let Some(local_addr) = local.base.checked_add(local_offset as u64) else {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EFAULT_VALUE);
        };
        let Some(remote_addr) = remote.base.checked_add(remote_offset as u64) else {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::Error(EFAULT_VALUE);
        };
        let result = match direction {
            ProcessVmDirection::Read => {
                bootstrap_copy_from_user(&ctx.aspace, &mut chunk, remote_addr)
                    .and_then(|()| bootstrap_copy_to_user(&ctx.aspace, local_addr, &chunk))
            }
            ProcessVmDirection::Write => {
                bootstrap_copy_from_user(&ctx.aspace, &mut chunk, local_addr)
                    .and_then(|()| bootstrap_copy_to_user(&ctx.aspace, remote_addr, &chunk))
            }
        };
        if let Err(errno) = result {
            if total > 0 {
                return SyscallResult::Return(total);
            }
            return SyscallResult::error_from(errno);
        }

        total += chunk_len as i64;
        local_offset += chunk_len;
        remote_offset += chunk_len;
    }

    SyscallResult::Return(total)
}

fn resolve_pidfd_process(ctx: &SyscallCtx<'_>, pidfd: u32) -> Option<Cap<ProcessIdentity>> {
    resolve_fd(&ctx.process, pidfd).and_then(|file| file.pidfd_process().cloned())
}

fn validate_single_node_policy(
    raw_mode: u64,
    nodemask_uaddr: u64,
    maxnode: u64,
    ctx: &SyscallCtx<'_>,
) -> Result<(), i32> {
    let mode_flags = raw_mode & (MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES);
    if mode_flags == (MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES) {
        return Err(EINVAL_VALUE);
    }
    let mode = raw_mode & !(MPOL_F_STATIC_NODES | MPOL_F_RELATIVE_NODES);
    if !matches!(
        mode,
        MPOL_DEFAULT
            | MPOL_PREFERRED
            | MPOL_BIND
            | MPOL_INTERLEAVE
            | MPOL_LOCAL
            | MPOL_PREFERRED_MANY
    ) {
        return Err(EINVAL_VALUE);
    }

    let mask = read_single_node_mask(nodemask_uaddr, maxnode, ctx)?;
    if mask & !1 != 0 {
        return Err(EINVAL_VALUE);
    }
    if mode == MPOL_DEFAULT && mask != 0 {
        return Err(EINVAL_VALUE);
    }
    if matches!(mode, MPOL_BIND | MPOL_INTERLEAVE | MPOL_PREFERRED_MANY) && mask == 0 {
        return Err(EINVAL_VALUE);
    }
    Ok(())
}

fn read_single_node_mask(
    nodemask_uaddr: u64,
    maxnode: u64,
    ctx: &SyscallCtx<'_>,
) -> Result<u64, i32> {
    if nodemask_uaddr == 0 || maxnode == 0 {
        return Ok(0);
    }
    bootstrap_read_user::<u64>(&ctx.aspace, nodemask_uaddr).map_err(errno_to_i32)
}

fn validate_process_target(pid: i64, ctx: &SyscallCtx<'_>) -> Result<(), i32> {
    if pid < 0 {
        return Err(EINVAL_VALUE);
    }
    if pid == 0 || pid as u32 == ctx.process.pid.0 {
        return Ok(());
    }
    match process_by_pid(Pid(pid as u32)) {
        Some(_) => Err(EPERM_VALUE),
        None => Err(ESRCH_VALUE),
    }
}

fn range_fully_mapped(ctx: &SyscallCtx<'_>, range: UserRange) -> bool {
    let entries = ctx.aspace.recipes_overlapping(range);
    let mut cursor = range.start().as_usize();
    let end = range.end().as_usize();
    for entry in entries {
        let start = entry.range.start().as_usize();
        let entry_end = entry.range.end().as_usize();
        if start > cursor {
            return false;
        }
        if entry_end > cursor {
            cursor = entry_end;
            if cursor >= end {
                return true;
            }
        }
    }
    false
}

/// `mprotect(addr, length, prot)` — Linux RV64 generic syscall #226.
///
/// Wraps `AddressSpace::try_mprotect`. PROT_GROWSDOWN/GROWSUP not
/// supported (returns `-ENOSYS`); other prot validation matches
/// `sys_mmap`.
pub(super) async fn sys_mprotect(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let prot_bits = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let prot_recognised =
        PROT_READ | PROT_WRITE | PROT_EXEC | PROT_NONE | PROT_GROWSDOWN | PROT_GROWSUP;
    if prot_bits & !prot_recognised != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if prot_bits & (PROT_GROWSDOWN | PROT_GROWSUP) != 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }
    let prot = Prot::new(
        prot_bits & PROT_READ != 0,
        prot_bits & PROT_WRITE != 0,
        prot_bits & PROT_EXEC != 0,
    );

    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    let op = VmProtectOp {
        aspace: &ctx.aspace,
        range,
        prot,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(_commit) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}

/// `mremap(old_addr, old_size, new_size, flags, new_addr)` — Linux
/// RV64 generic syscall #216.
pub(super) async fn sys_mremap(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let old_addr = args[0];
    let old_size_in = args[1] as usize;
    let new_size_in = args[2] as usize;
    let flags = args[3];
    let new_addr = args[4];

    if old_size_in == 0 || new_size_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let known_flags = MREMAP_MAYMOVE | MREMAP_FIXED | MREMAP_DONTUNMAP;
    if flags & !known_flags != 0 || flags & MREMAP_DONTUNMAP != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let may_move = flags & MREMAP_MAYMOVE != 0;
    let fixed = flags & MREMAP_FIXED != 0;
    if fixed && !may_move {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let old_size = match old_size_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let new_size = match new_size_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(old_addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let old_range = match UserRange::new_aligned(UserVirtAddr::new(old_addr as usize), old_size) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    let request = if fixed {
        if !UserVirtAddr::new(new_addr as usize).is_page_aligned() {
            return SyscallResult::Error(EINVAL_VALUE);
        }
        let new_range = match UserRange::new_aligned(UserVirtAddr::new(new_addr as usize), new_size)
        {
            Ok(r) => r,
            Err(_) => return SyscallResult::Error(EINVAL_VALUE),
        };
        VmRemapRequest::fixed_replace(old_range, new_range)
    } else {
        let in_place_range =
            match UserRange::new_aligned(UserVirtAddr::new(old_addr as usize), new_size) {
                Ok(r) => r,
                Err(_) => return SyscallResult::Error(EINVAL_VALUE),
            };
        VmRemapRequest::in_place(old_range, in_place_range)
    };

    let outcome = drive_vm_remap(ctx, request).await;
    let result = match outcome {
        Ok(outcome) => Ok(outcome),
        Err(errno)
            if !fixed && may_move && (errno == V3Errno::ENOMEM || errno == V3Errno::EEXIST) =>
        {
            let page_count = new_size / USER_PAGE_SIZE;
            let window = match UserRange::new_aligned(
                UserVirtAddr(USER_PAGE_SIZE),
                FULL_USER_V1_TOP - USER_PAGE_SIZE,
            ) {
                Ok(range) => range,
                Err(_) => return SyscallResult::Error(EINVAL_VALUE),
            };
            let Some(dst_range) = ctx.aspace.find_free_range(window, page_count) else {
                return SyscallResult::Error(errno_to_i32(Errno::ENOMEM));
            };
            let move_request = VmRemapRequest::new(old_range, dst_range);
            drive_vm_remap(ctx, move_request).await
        }
        Err(errno) if !fixed && !may_move && errno == V3Errno::EEXIST => Err(V3Errno::ENOMEM),
        Err(errno) => Err(errno),
    };

    match result {
        Ok(outcome) => SyscallResult::Return(outcome.new_range.start().as_usize() as i64),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}

async fn drive_vm_remap(
    ctx: &SyscallCtx<'_>,
    request: VmRemapRequest,
) -> Result<tx_subsystems::vm::VmRemapOutcome, V3Errno> {
    let op = VmRemapOp {
        aspace: &ctx.aspace,
        request,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
}

/// `remap_file_pages(start, size, prot, pgoff, flags)` — Linux RV64
/// generic syscall #234.
///
/// Linux keeps this obsolete syscall as a compatibility wrapper for
/// nonlinear file mappings. Tx's v1 slice accepts the common LTP shape:
/// an existing shared PageBacked VMA range is replaced in place with the
/// same protection/flags and the same PageContainer at `pgoff` pages.
pub(super) async fn sys_remap_file_pages(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let size_in = args[1] as usize;
    let prot = args[2];
    let pgoff = args[3];
    let flags = args[4];

    if size_in == 0 || prot != 0 || flags & !MAP_NONBLOCK != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let size = match size_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(size) => size,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), size) {
        Ok(range) => range,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    let offset = match pgoff.checked_mul(USER_PAGE_SIZE as u64) {
        Some(offset) => offset,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };

    let Some((pc, prot, entry_flags)) = remap_file_pages_target(ctx, range) else {
        return SyscallResult::Error(EINVAL_VALUE);
    };
    let request = VmMapRequest::fixed(
        range,
        MapPlacement::FixedReplace,
        prot,
        entry_flags,
        VmBacking::Page { pc, offset },
    );
    let op = VmMapOp {
        aspace: &ctx.aspace,
        request,
    };
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        delegate_registry_arc.as_deref(),
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(_) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(Into::<Errno>::into(errno)),
    }
}

fn remap_file_pages_target(
    ctx: &SyscallCtx<'_>,
    range: UserRange,
) -> Option<(Cap<PageContainer>, Prot, VmEntryFlags)> {
    let entries = ctx.aspace.recipes_overlapping(range);
    let mut cursor = range.start().as_usize();
    let end = range.end().as_usize();
    let mut target_pc: Option<Cap<PageContainer>> = None;
    let mut target_prot: Option<Prot> = None;
    let mut target_flags: Option<VmEntryFlags> = None;

    for entry in entries {
        if entry.range.start().as_usize() > cursor {
            return None;
        }
        if !entry.flags.shared {
            return None;
        }
        let VmBacking::Page { pc, .. } = entry.backing else {
            return None;
        };
        match &target_pc {
            Some(existing) if existing.raw() != pc.raw() => return None,
            None => target_pc = Some(pc.clone()),
            _ => {}
        }
        match target_prot {
            Some(prot) if prot != entry.prot => return None,
            None => target_prot = Some(entry.prot),
            _ => {}
        }
        match target_flags {
            Some(flags) if flags != entry.flags => return None,
            None => target_flags = Some(entry.flags),
            _ => {}
        }

        cursor = cursor.max(entry.range.end().as_usize());
        if cursor >= end {
            return Some((
                target_pc.expect("page-backed target recorded"),
                target_prot.expect("prot recorded"),
                target_flags.expect("flags recorded"),
            ));
        }
    }

    None
}

/// `mincore(addr, length, vec)` — Linux RV64 generic syscall #232.
///
/// Copies one byte per covered page to `vec`; bit 0 is set when the page
/// has a resident pmap entry at the moment of observation.
pub(super) fn sys_mincore<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let vec_uaddr = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !range_is_fully_mapped_for_mincore(&ctx.aspace, range) {
        return SyscallResult::error_from(Errno::ENOMEM);
    }

    let residency = ctx.aspace.mincore(range);
    let mut vec = Vec::with_capacity(residency.len());
    vec.extend(residency.into_iter().map(|resident| u8::from(resident)));
    match bootstrap_copy_to_user(&ctx.aspace, vec_uaddr, &vec) {
        Ok(()) => SyscallResult::Return(0),
        Err(errno) => SyscallResult::error_from(errno),
    }
}

fn range_is_fully_mapped_for_mincore(aspace: &AddressSpace, range: UserRange) -> bool {
    let mut cursor = range.start().as_usize();
    for entry in aspace.recipes_overlapping(range) {
        let entry_start = entry.range.start().as_usize();
        let entry_end = entry.range.end().as_usize();
        if entry_start > cursor {
            return false;
        }
        cursor = cursor.max(entry_end);
        if cursor >= range.end().as_usize() {
            return true;
        }
    }
    false
}

/// `madvise(addr, length, advice)` — Linux RV64 generic syscall #233.
///
/// Slice 2 honours `MADV_NORMAL` / `RANDOM` / `SEQUENTIAL` / `WILLNEED`
/// (all observation-only no-ops in `AddressSpace::madvise`) and
/// `MADV_DONTNEED` / `MADV_FREE` (range-scoped pmap teardown). Other
/// advice values return `-ENOSYS` — the underlying `MadviseAdvice`
/// enum has no slot for them.
pub(super) fn sys_madvise<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let advice_raw = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let advice = match decode_madvise_advice(advice_raw) {
        Ok(advice) => advice,
        Err(errno) => return SyscallResult::Error(errno),
    };
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    match ctx.aspace.madvise(range, advice) {
        Ok(()) => SyscallResult::Return(0),
        Err(error) => SyscallResult::Error(vmmap_error_to_i32(error)),
    }
}

fn decode_madvise_advice(advice_raw: u64) -> Result<MadviseAdvice, i32> {
    match advice_raw {
        MADV_NORMAL => Ok(MadviseAdvice::Normal),
        MADV_RANDOM => Ok(MadviseAdvice::Random),
        MADV_SEQUENTIAL => Ok(MadviseAdvice::Sequential),
        MADV_WILLNEED => Ok(MadviseAdvice::WillNeed),
        MADV_DONTNEED => Ok(MadviseAdvice::DontNeed),
        MADV_FREE => Ok(MadviseAdvice::Free),
        _ => Err(ENOSYS_VALUE),
    }
}

/// `msync(addr, length, flags)` — Linux RV64 generic syscall #227.
///
/// The lone async VM arm: `AddressSpace::msync` calls into
/// `step_fsync` per File-backed page container, which can return
/// `Blocked` against the page-cache wait source. The arm awaits the
/// carrier and re-polls until `step_fsync` reaches `Done`.
///
/// `flags` (`MS_ASYNC` / `MS_SYNC` / `MS_INVALIDATE`) is recognised
/// but ignored — Slice 2 always behaves as `MS_SYNC` (synchronous
/// flush) and never invalidates non-flushed cache state.
pub(super) async fn sys_msync<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let _flags = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(r) => r,
        None => return SyscallResult::Error(EINVAL_VALUE),
    };
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let _delegate_registry_arc = script_ctx.delegate_registry().cloned();
    let op = VmMsyncOp {
        aspace: &ctx.aspace,
        range,
    };
    match drive(
        op,
        &mut script_ctx,
        DriveMode::Waiting,
        mailbox_arc.as_ref(),
        None,
        timer_wheel_arc.as_ref(),
    )
    .await
    {
        Ok(()) => SyscallResult::Return(0),
        Err(v3errno) => SyscallResult::error_from(Errno::from(v3errno)),
    }
}

/// Resolve an `OpenFile` to its underlying `Cap<PageContainer>` if
/// the rnode backing is `RNodeBacking::PageBacked`. TTY / pipe /
/// chardev / directory / symlink rnodes return `None`; the caller
/// surfaces this as `-ENODEV` per Linux's `mmap(2)` errno surface for
/// unsupported file types.
pub(super) fn extract_page_container(
    file: &Cap<OpenFile>,
) -> Option<Cap<tx_subsystems::page_backed::PageContainer>> {
    use tx_subsystems::vfs::structure::RNodeBacking;
    match file.rnode().backing() {
        RNodeBacking::PageBacked { pc } => Some(pc.clone()),
        _ => None,
    }
}

/// Translate `VmMapError` into a Linux RV64 generic ABI errno
/// magnitude. Slice 2 mapping:
///
/// - `AlreadyMapped` → `EEXIST` (17). Real Linux returns `EEXIST`
///   only for `MAP_FIXED_NOREPLACE`; other shapes silently replace.
///   `sys_mmap` overrides this for non-`FIXED_NOREPLACE` paths
///   before calling here, but `try_mmap`'s contract still surfaces
///   `AlreadyMapped` for the `RequireFree` placement, so EEXIST is
///   the right magnitude for the error-class on its own.
/// - `InvalidRange` → `EINVAL` (22).
/// - `MissingMapping` → `EINVAL` (22). Linux's `munmap` returns 0
///   for an unmapped region; `try_munmap`'s `MissingMapping` is the
///   "fully-disjoint range" shape that `mprotect` / `madvise` /
///   `mremap` need to flag.
/// - `NoFreeRange` → `ENOMEM` (12). Linux's "no usable address
///   range" magnitude.
/// - `WouldBlock` → `EAGAIN` (11). Slice 2 only reaches this for
///   in-flight VM contention; the synchronous arms surface the
///   raw `WouldBlock` rather than spinning.
/// - `BackingOffsetOverflow` → `EINVAL` (22). Bad offset arithmetic.
/// - `Pmap(_)` → `EIO` (5). Catch-all for the lower-level pmap
///   error variants; never produced by the `try_*` step's
///   non-async lane in practice.
pub(super) fn vmmap_error_to_i32(error: VmMapError) -> i32 {
    match error {
        VmMapError::AlreadyMapped => errno_to_i32(Errno::EEXIST),
        VmMapError::InvalidRange => EINVAL_VALUE,
        VmMapError::MissingMapping => EINVAL_VALUE,
        VmMapError::NoFreeRange => errno_to_i32(Errno::ENOMEM),
        VmMapError::WouldBlock => EAGAIN_VALUE,
        VmMapError::BackingOffsetOverflow => EINVAL_VALUE,
        VmMapError::Pmap(_) => errno_to_i32(Errno::EIO),
        VmMapError::Private(_) => errno_to_i32(Errno::ENOMEM),
    }
}

/// `futex(uaddr, op, val, timeout, uaddr2, val3)` — Linux RV64
/// generic syscall #98.
///
/// Slice 3 of the shell-prompt roadmap (2026-05-07). Required for
/// musl libc init: musl uses futex internally for `pthread_once`-
/// style guards even in single-threaded programs, and would otherwise
/// trip on `-ENOSYS` within the first few thousand instructions of
/// `__init_libc`.
///
/// v1 supports `FUTEX_WAIT` and `FUTEX_WAKE` only; other ops
/// (`REQUEUE`, `CMP_REQUEUE`, `WAKE_OP`, `LOCK_PI`, `WAIT_BITSET`
/// etc.) return `-ENOSYS`. `FUTEX_PRIVATE_FLAG` and
/// `FUTEX_CLOCK_REALTIME` flag bits are accepted but ignored —
/// per-process isolation is implicit (each process has its own
/// aspace and the user word at `uaddr` lives in that aspace), and
/// timeout support is deferred to Slice 4 with the timer-wait
/// carrier. The `timeout` (args[3]), `uaddr2` (args[4]), and
/// `val3` (args[5]) arguments are ignored in v1.
///
/// **`FUTEX_WAIT` semantics.** Loops on the canonical wait-carrier
/// discipline:
///
/// 1. Take a fresh `epoch::guard()` and call `step_futex_wait`.
/// 2. `Blocked(token)` → set `parked = true`, `await` the wait
///    future, loop back to (1).
/// 3. `Done(())` / `Advanced(())` → return `0`.
/// 4. `Err(EAGAIN)` → distinguishes "first-call mismatch" (return
///    `-EAGAIN` to userspace per Linux) from "post-wake re-check
///    showed the word changed" (return `0` per the WAIT contract)
///    via the `parked` flag tracked across loop iterations.
/// 5. Other `Err(errno)` → return `-errno`.
pub(super) async fn sys_futex<'a, P: TimeIf>(
    args: [u64; 6],
    ctx: &SyscallCtx<'a>,
) -> SyscallResult {
    let uaddr = args[0];
    let op_full = args[1] as u32;
    let val = args[2] as u32;
    let timeout_uaddr = args[3];
    let val2 = args[3] as u32;
    let uaddr2 = args[4];
    let bitset = args[5] as u32;

    let op = op_full & FUTEX_CMD_MASK;

    match op {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            use tx_scripts::drive;
            use tx_substrate::step::Deadline;
            use tx_substrate::step::DriveMode;
            use tx_subsystems::futex::{FutexWaitOp, FUTEX_WAKE_MASK};

            if uaddr == 0 {
                return SyscallResult::error_from(Errno::EINVAL);
            }
            if op == FUTEX_WAIT_BITSET && bitset == 0 {
                return SyscallResult::Error(EINVAL_VALUE);
            }

            let wait_mask = if op == FUTEX_WAIT_BITSET {
                bitset as u64
            } else {
                FUTEX_WAKE_MASK
            };

            // Validate the user address before probing — the futex
            // subsystem reads *uaddr via `read_volatile` (bootstrap
            // exemption), which traps the kernel on an unmapped page.
            // A pre-check with the safe `read_user` accessor converts
            // the trap into a graceful -EFAULT.
            {
                let guard = step_engine::guard();
                let user_ptr = UserPtr::<u32>::new(uaddr as usize);
                if let StepOutcome::Err(_) = ctx.aspace.read_user(user_ptr, &guard) {
                    return SyscallResult::error_from(Errno::EFAULT);
                }
            }
            let mut timeout_deadline_ns = None;
            if timeout_uaddr != 0 {
                let Some(timeout_ns) = read_timespec_at(&ctx.aspace, timeout_uaddr) else {
                    return SyscallResult::Error(EINVAL_VALUE);
                };
                if timeout_ns == 0 {
                    let guard = step_engine::guard();
                    let user_ptr = UserPtr::<u32>::new(uaddr as usize);
                    match ctx.aspace.read_user(user_ptr, &guard) {
                        StepOutcome::Done(observed) if observed == val => {
                            return SyscallResult::Error(110);
                        }
                        StepOutcome::Done(_) => return SyscallResult::Error(EAGAIN_VALUE),
                        StepOutcome::Err(_) => return SyscallResult::error_from(Errno::EFAULT),
                        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                            return SyscallResult::error_from(Errno::EFAULT);
                        }
                    }
                }
                timeout_deadline_ns = Some(<P as TimeIf>::read_ns().saturating_add(timeout_ns));
            }
            let (deadline_ns, interrupted_by_process_timer) =
                futex_wait_deadline(ctx, timeout_deadline_ns);

            let mut script_ctx = build_subject_script_ctx(ctx);
            if let Some(deadline_ns) = deadline_ns {
                script_ctx = script_ctx.with_deadline(Deadline::from_raw(deadline_ns));
            }
            let mailbox_arc = script_ctx.mailbox().cloned();
            let timer_wheel_arc = script_ctx.timer_wheel().cloned();
            let delegate_registry_arc = script_ctx.delegate_registry().cloned();

            // Park and wait.  drive() parks on the futex bucket's
            // WaitSource via resolve_on_wait_source, wakes when
            // step_futex_wake fires the bucket, and re-steps.  The
            // FutexWaitOp records the resume and completes immediately
            // after a wake; Linux FUTEX_WAIT returns 0 for the wake and
            // leaves condition re-checking to userspace.
            //
            // Op acquires its own epoch guard inside `step()`; no
            // guard crosses `drive(...).await` (REACTOR_v0,
            // STEP_MODEL_v2 §1, INVARIANTS_v5 EBR-7).
            let op = FutexWaitOp {
                uaddr,
                val,
                aspace: &ctx.aspace,
                interest_mask: wait_mask,
                woken: false,
                waiting: false,
            };
            match drive(
                op,
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_wheel_arc.as_ref(),
            )
            .await
            {
                Ok(()) => SyscallResult::Return(0),
                Err(v3errno) => {
                    let errno: Errno = v3errno.into();
                    if interrupted_by_process_timer && errno == Errno::ETIMEDOUT {
                        let now_ns = <P as TimeIf>::read_ns().max(deadline_ns.unwrap_or_default());
                        super::time::poll_expired_process_timers_at(ctx, now_ns);
                        return SyscallResult::Error(EINTR_VALUE);
                    }
                    SyscallResult::error_from(errno)
                }
            }
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            if op == FUTEX_WAKE_BITSET && bitset == 0 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            let wake_mask = if op == FUTEX_WAKE_BITSET {
                bitset as u64
            } else {
                tx_subsystems::futex::FUTEX_WAKE_MASK
            };
            futex_wake_oneshot(ctx, uaddr, val, wake_mask)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => {
            if uaddr2 == 0 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            if op == FUTEX_CMP_REQUEUE {
                let guard = step_engine::guard();
                let user_ptr = UserPtr::<u32>::new(uaddr as usize);
                match ctx.aspace.read_user(user_ptr, &guard) {
                    StepOutcome::Done(observed) if observed == bitset => {}
                    StepOutcome::Done(_) => return SyscallResult::Error(EAGAIN_VALUE),
                    StepOutcome::Err(_) => return SyscallResult::error_from(Errno::EFAULT),
                    StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                        return SyscallResult::error_from(Errno::EFAULT);
                    }
                }
            }
            let mut script_ctx = build_subject_script_ctx(ctx);
            let guard = step_engine::guard();
            let outcome = tx_subsystems::futex::step_futex_requeue_in(
                &ctx.aspace,
                uaddr,
                uaddr2,
                val,
                val2,
                &guard,
            );
            drop(guard);
            match outcome {
                StepOutcome::Done(count) => SyscallResult::Return(count as i64),
                StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
                StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                    let _ = &mut script_ctx;
                    SyscallResult::error_from(Errno::EIO)
                }
            }
        }
        FUTEX_WAKE_OP => {
            if uaddr2 == 0 {
                return SyscallResult::Error(EINVAL_VALUE);
            }
            let first = match futex_wake_count(ctx, uaddr, val) {
                Ok(woken) => woken,
                Err(result) => return result,
            };
            let second = match futex_wake_count(ctx, uaddr2, val2) {
                Ok(woken) => woken,
                Err(result) => return result,
            };
            SyscallResult::Return(first.saturating_add(second) as i64)
        }
        FUTEX_LOCK_PI => futex_pi_lock(ctx, uaddr, false),
        FUTEX_TRYLOCK_PI => futex_pi_lock(ctx, uaddr, true),
        FUTEX_UNLOCK_PI => futex_pi_unlock(ctx, uaddr),
        // FUTEX_REQUEUE / CMP_REQUEUE / WAKE_OP / LOCK_PI /
        // UNLOCK_PI / TRYLOCK_PI / WAIT_BITSET / WAKE_BITSET — out
        // of scope for v1. musl's libc init only emits FUTEX_WAIT
        // and FUTEX_WAKE so these are not on the critical path.
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

fn futex_wait_deadline(
    ctx: &SyscallCtx<'_>,
    timeout_deadline_ns: Option<u64>,
) -> (Option<u64>, bool) {
    if ctx.mailbox.is_none() || ctx.timer_wheel.is_none() {
        return (timeout_deadline_ns, false);
    }
    match (
        timeout_deadline_ns,
        ctx.process.next_process_timer_deadline_ns(),
    ) {
        (Some(timeout), Some(timer)) if timer <= timeout => (Some(timer), true),
        (None, Some(timer)) => (Some(timer), true),
        _ => (timeout_deadline_ns, false),
    }
}

fn futex_wake_oneshot(ctx: &SyscallCtx<'_>, uaddr: u64, n: u32, wake_mask: u64) -> SyscallResult {
    match futex_wake_count_masked(ctx, uaddr, n, wake_mask) {
        Ok(woken) => SyscallResult::Return(woken as i64),
        Err(result) => result,
    }
}

fn futex_wake_count(ctx: &SyscallCtx<'_>, uaddr: u64, n: u32) -> Result<u32, SyscallResult> {
    futex_wake_count_masked(ctx, uaddr, n, tx_subsystems::futex::FUTEX_WAKE_MASK)
}

fn futex_wake_count_masked(
    ctx: &SyscallCtx<'_>,
    uaddr: u64,
    n: u32,
    wake_mask: u64,
) -> Result<u32, SyscallResult> {
    let mut script_ctx = build_subject_script_ctx(ctx);
    if wake_mask == tx_subsystems::futex::FUTEX_WAKE_MASK {
        let mut op = FutexWakeOp {
            uaddr,
            n,
            aspace: &ctx.aspace,
        };
        match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
            Ok(woken) => Ok(woken),
            Err(v3errno) => Err(SyscallResult::error_from(Errno::from(v3errno))),
        }
    } else {
        let guard = step_engine::guard();
        let outcome = tx_subsystems::futex::step_futex_wake_masked_in(
            &ctx.aspace,
            uaddr,
            n,
            wake_mask,
            &guard,
        );
        drop(guard);
        match outcome {
            StepOutcome::Done(woken) => Ok(woken),
            StepOutcome::Err(errno) => Err(SyscallResult::error_from(Errno::from(errno))),
            StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                Err(SyscallResult::error_from(Errno::EIO))
            }
        }
    }
}

fn futex_owner_tid(ctx: &SyscallCtx<'_>) -> u32 {
    ctx.thread.tid.0
}

fn futex_pi_lock(ctx: &SyscallCtx<'_>, uaddr: u64, try_only: bool) -> SyscallResult {
    let owner = futex_owner_tid(ctx);
    let guard = step_engine::guard();
    let outcome = if try_only {
        tx_subsystems::futex::step_futex_trylock_pi_in(&ctx.aspace, uaddr, owner, &guard)
    } else {
        tx_subsystems::futex::step_futex_lock_pi_in(&ctx.aspace, uaddr, owner, false, &guard)
    };
    drop(guard);
    match outcome {
        StepOutcome::Done(()) => SyscallResult::Return(0),
        StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            SyscallResult::error_from(Errno::EIO)
        }
    }
}

fn futex_pi_unlock(ctx: &SyscallCtx<'_>, uaddr: u64) -> SyscallResult {
    let owner = futex_owner_tid(ctx);
    let guard = step_engine::guard();
    let outcome = tx_subsystems::futex::step_futex_unlock_pi_in(&ctx.aspace, uaddr, owner, &guard);
    drop(guard);
    match outcome {
        StepOutcome::Done(_) => SyscallResult::Return(0),
        StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            SyscallResult::error_from(Errno::EIO)
        }
    }
}
