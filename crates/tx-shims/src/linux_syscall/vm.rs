//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, StepOutcome};
use tx_scripts::drive;
use tx_substrate::step::DriveMode;
use tx_substrate::step::Errno as V3Errno;
use tx_subsystems::vm::step_ops::{
    VmBrkOp, VmMapOp, VmMlockOp, VmMsyncOp, VmMunlockOp, VmProtectOp, VmRemapOp, VmUnmapOp,
};

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
/// - `MAP_HUGETLB` / `MAP_LOCKED` / `MAP_POPULATE` / `MAP_STACK` etc.
///   are recognised but ignored (best-effort hints).
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
    let entry_flags =
        VmEntryFlags::new(shared, flags & MAP_GROWSDOWN != 0, flags & MAP_LOCKED != 0);

    // Build the backing.
    let backing = if anonymous {
        VmBacking::PrivateAnon
    } else {
        if fd < 0 {
            return SyscallResult::Error(EBADF_VALUE);
        }
        let file = match resolve_fd(&ctx.process, fd as u32) {
            Some(f) => f,
            None => return SyscallResult::Error(EBADF_VALUE),
        };
        match extract_page_container(&file) {
            Some(pc) => VmBacking::Page { pc, offset },
            None => return SyscallResult::Error(errno_to_i32(Errno::ENODEV)),
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
        Err(errno) => SyscallResult::Error(errno_to_i32(Into::<Errno>::into(errno))),
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
        Err(errno) => SyscallResult::Error(errno_to_i32(Into::<Errno>::into(errno))),
    }
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
        Err(errno) => SyscallResult::Error(errno_to_i32(Into::<Errno>::into(errno))),
    }
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
        Err(errno) => SyscallResult::Error(errno_to_i32(Into::<Errno>::into(errno))),
    }
}

/// `mremap(old_addr, old_size, new_size, flags, new_addr)` — Linux
/// RV64 generic syscall #216.
///
/// Slice 2 wraps `AddressSpace::try_mremap`, which only supports the
/// disjoint-range form (`old_range ∩ new_range == ∅`). The Linux
/// `MREMAP_FIXED | MREMAP_MAYMOVE` shape musl emits maps cleanly to
/// this contract; in-place grow without `MAYMOVE` would need
/// `try_mremap`'s contract extended (deferred).
pub(super) async fn sys_mremap(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let old_addr = args[0];
    let old_size_in = args[1] as usize;
    let new_size_in = args[2] as usize;
    let _flags = args[3];
    let new_addr = args[4];

    if old_size_in == 0 || new_size_in == 0 {
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
    if !UserVirtAddr::new(old_addr as usize).is_page_aligned()
        || !UserVirtAddr::new(new_addr as usize).is_page_aligned()
    {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let old_range = match UserRange::new_aligned(UserVirtAddr::new(old_addr as usize), old_size) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };
    let new_range = match UserRange::new_aligned(UserVirtAddr::new(new_addr as usize), new_size) {
        Ok(r) => r,
        Err(_) => return SyscallResult::Error(EINVAL_VALUE),
    };

    let request = VmRemapRequest::new(old_range, new_range);
    let op = VmRemapOp {
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
        Ok(outcome) => SyscallResult::Return(outcome.new_range.start().as_usize() as i64),
        Err(errno) => SyscallResult::Error(errno_to_i32(Into::<Errno>::into(errno))),
    }
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
    let advice = match advice_raw {
        MADV_NORMAL => MadviseAdvice::Normal,
        MADV_RANDOM => MadviseAdvice::Random,
        MADV_SEQUENTIAL => MadviseAdvice::Sequential,
        MADV_WILLNEED => MadviseAdvice::WillNeed,
        MADV_DONTNEED => MadviseAdvice::DontNeed,
        MADV_FREE => MadviseAdvice::Free,
        _ => return SyscallResult::Error(ENOSYS_VALUE),
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
        Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
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
pub(super) async fn sys_futex<'a>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult {
    let uaddr = args[0];
    let op_full = args[1] as u32;
    let val = args[2] as u32;

    let op = op_full & FUTEX_CMD_MASK;

    match op {
        FUTEX_WAIT => {
            use tx_scripts::drive;
            use tx_substrate::step::DriveMode;
            use tx_subsystems::futex::FutexWaitOp;

            let mut script_ctx = build_subject_script_ctx(ctx);
            let mailbox_arc = script_ctx.mailbox().cloned();
            let timer_wheel_arc = script_ctx.timer_wheel().cloned();
            let delegate_registry_arc = script_ctx.delegate_registry().cloned();

            // First, probe the futex word without parking. EAGAIN
            // here means "word != val" → return -EAGAIN immediately
            // (the predecessor equality check, per POSIX).
            {
                let guard = step_engine::guard();
                let outcome = tx_subsystems::futex::step_futex_wait(uaddr, val, &guard);
                match outcome {
                    StepOutcome::Err(e) if e == V3Errno::EAGAIN => {
                        return SyscallResult::Error(errno_to_i32(Errno::EAGAIN));
                    }
                    StepOutcome::Err(e) => {
                        return SyscallResult::Error(errno_to_i32(Errno::from(e)));
                    }
                    // Yield / Continue / Done: fall through to drive().
                    _ => {}
                }
            }

            // Park and wait.  drive() parks on the futex bucket's
            // WaitSource via resolve_on_wait_source, wakes when
            // step_futex_wake fires the bucket, and re-steps.
            // After waking, EAGAIN means "word changed → wake was
            // meaningful" → return 0.
            //
            // Op acquires its own epoch guard inside `step()`; no
            // guard crosses `drive(...).await` (REACTOR_v0,
            // STEP_MODEL_v2 §1, INVARIANTS_v5 EBR-7).
            let op = FutexWaitOp { uaddr, val };
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
                    if errno == Errno::EAGAIN {
                        SyscallResult::Return(0)
                    } else {
                        SyscallResult::Error(errno_to_i32(errno))
                    }
                }
            }
        }
        FUTEX_WAKE => {
            let mut script_ctx = build_subject_script_ctx(ctx);
            let mut op = FutexWakeOp { uaddr, n: val };
            match step_engine::drive_oneshot(&mut op, &mut script_ctx) {
                Ok(woken) => SyscallResult::Return(woken as i64),
                Err(v3errno) => SyscallResult::Error(errno_to_i32(Errno::from(v3errno))),
            }
        }
        // FUTEX_REQUEUE / CMP_REQUEUE / WAKE_OP / LOCK_PI /
        // UNLOCK_PI / TRYLOCK_PI / WAIT_BITSET / WAKE_BITSET — out
        // of scope for v1. musl's libc init only emits FUTEX_WAIT
        // and FUTEX_WAKE so these are not on the critical path.
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}
