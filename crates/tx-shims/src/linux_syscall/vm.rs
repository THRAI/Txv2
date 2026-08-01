//! Auto-extracted from `mod.rs` during the 2026-05-08 jumbo-mod
//! split. The implementations here are unchanged; only the
//! enclosing module changed. Helpers and constants used here live
//! either in this submodule or in the shared parent (`super::*`).

use core::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::adapter::step_engine::{self as step_engine, Cap, Errno as V3Errno, StepOutcome};
use tx_hal::UserPtr;
use tx_scripts::drive;
use tx_services::time::{timekeeper, timekeeper_clock, ClockRead, TimekeeperClock, TimekeeperIf};
use tx_substrate::step::DriveMode;
use tx_substrate::wake::MailboxSchedulerHint;
use tx_subsystems::vm::step_ops::{
    VmBrkOp, VmMapOp, VmMlockOp, VmMsyncOp, VmMunlockOp, VmProtectOp, VmRemapOp, VmUnmapOp,
};

/// Soft budget for the linear `brk` heap before callers are asked to fall
/// back to `mmap`.
///
/// Linux `brk(2)` is allowed to fail by returning the unchanged break. The
/// OSComp musl image uses oldmalloc, whose small-allocation path otherwise
/// grows `brk` one page at a time; on the current VM substrate that turns
/// libcbench malloc into tens of thousands of tiny syscall/VM operations.
/// Keeping a small budget preserves simple `sbrk` compatibility (including
/// basic-musl's 64-byte checks) while nudging allocator-scale growth onto
/// musl's exponentially growing mmap fallback.
pub(super) const BRK_LINEAR_HEAP_SOFT_LIMIT_BYTES: usize = USER_PAGE_SIZE * 64;

static FUTEX_WAIT_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static FUTEX_WAKE_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);
static VM_RECIPE_TRACE_SAMPLE: AtomicU64 = AtomicU64::new(0);

fn futex_trace_sample(counter: &AtomicU64) -> bool {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    n == 1 || n % 256 == 0
}

fn emit_futex_trace(name: &[u8], value: i64) {
    if let Some(observer) = tx_observe::current() {
        observer.counter(
            tx_observe::EventNameId::from_raw(tx_observe::fnv1a32(name)),
            value,
        );
    }
}

fn emit_futex_wait_entry(uaddr: u64, val: u32, op: u32, wait_mask: u64) {
    emit_futex_trace(b"debug.futex.wait.uaddr", uaddr as i64);
    emit_futex_trace(b"debug.futex.wait.val", i64::from(val));
    emit_futex_trace(b"debug.futex.wait.op", i64::from(op));
    emit_futex_trace(b"debug.futex.wait.mask", wait_mask as i64);
}

fn emit_futex_wake_entry(uaddr: u64, n: u32, op: u32, wake_mask: u64) {
    emit_futex_trace(b"debug.futex.wake.uaddr", uaddr as i64);
    emit_futex_trace(b"debug.futex.wake.n", i64::from(n));
    emit_futex_trace(b"debug.futex.wake.op", i64::from(op));
    emit_futex_trace(b"debug.futex.wake.mask", wake_mask as i64);
}

fn emit_futex_result(name: &[u8], result: &SyscallResult) {
    let code = match result {
        SyscallResult::Return(v) => *v,
        SyscallResult::CloneReturn { value, .. } => *value,
        SyscallResult::Error(e) => -i64::from(*e),
        SyscallResult::NoReturn => i64::MIN + 1,
        SyscallResult::ExecCommitted => i64::MIN + 2,
        SyscallResult::SigreturnRestored => i64::MIN + 3,
        SyscallResult::SigreturnContextRestored => i64::MIN + 4,
    };
    emit_futex_trace(name, code);
    tx_observe::dump_registered_if_requested();
}

fn brk_growth_exceeds_soft_limit(brk_base: u64, current_brk: u64, requested: u64) -> bool {
    if requested <= current_brk {
        return false;
    }
    let Some(limit) = brk_base.checked_add(BRK_LINEAR_HEAP_SOFT_LIMIT_BYTES as u64) else {
        return false;
    };
    requested > limit
}

fn emit_vm_recipe_summary(name: &[u8], ctx: &SyscallCtx<'_>) {
    let n = VM_RECIPE_TRACE_SAMPLE.fetch_add(1, Ordering::Relaxed) + 1;
    if n != 1 && n % 512 != 0 {
        return;
    }
    emit_futex_trace(name, n as i64);
    let stats = ctx.aspace.stats();
    emit_futex_trace(b"debug.vm.recipe.count", stats.recipe_count as i64);
    emit_futex_trace(b"debug.vm.recipe.vm_size", stats.vm_size as i64);
    tx_observe::dump_registered_if_requested();
}

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

    if brk_growth_exceeds_soft_limit(brk_base, current_brk, requested) {
        return SyscallResult::Return(current_brk as i64);
    }

    match ctx.aspace.try_brk(base, cur, req) {
        Ok(new_brk) => {
            ctx.process.set_current_brk(new_brk.0 as u64);
            return SyscallResult::Return(new_brk.0 as i64);
        }
        Err(VmMapError::WouldBlock) => {}
        Err(_) => return SyscallResult::Return(current_brk as i64),
    }

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
            VmBacking::Page {
                pc: pc.into(),
                offset: 0,
            }
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
        // Linux file-mapping access checks (mm/mmap.c): every file
        // mapping requires the descriptor be open for reading, and a
        // writable shared mapping additionally requires write access.
        if !file.flags().read {
            return SyscallResult::error_from(Errno::EACCES);
        }
        if shared && (prot_bits & PROT_WRITE) != 0 && !file.flags().write {
            return SyscallResult::error_from(Errno::EACCES);
        }
        match extract_page_container(&file) {
            Some(pc) => VmBacking::Page {
                pc: pc.into(),
                offset,
            },
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

    match ctx.aspace.try_mmap(request.clone()) {
        Ok(outcome) => {
            emit_vm_recipe_summary(b"debug.vm.recipe.mmap", ctx);
            return SyscallResult::Return(outcome.range.start().as_usize() as i64);
        }
        Err(VmMapError::WouldBlock) => {}
        Err(error) => {
            let code = if fixed_noreplace && error == VmMapError::AlreadyMapped {
                errno_to_i32(Errno::EEXIST)
            } else {
                vmmap_error_to_i32(error)
            };
            return SyscallResult::Error(code);
        }
    }

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
        Ok(outcome) => {
            emit_vm_recipe_summary(b"debug.vm.recipe.mmap", ctx);
            SyscallResult::Return(outcome.range.start().as_usize() as i64)
        }
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

pub(super) fn sys_mmap_private_anon_try(
    args: [u64; 6],
    aspace: &Cap<AddressSpace>,
) -> Option<SyscallResult> {
    let addr = args[0];
    let length_in = args[1] as usize;
    let prot_bits = args[2];
    let flags = args[3];
    let fd = args[4] as i32;
    let offset = args[5];

    if flags & MAP_ANONYMOUS == 0 || flags & MAP_PRIVATE == 0 || flags & MAP_SHARED != 0 {
        return None;
    }
    if fd != -1 || offset != 0 {
        return None;
    }

    if length_in == 0 {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return Some(SyscallResult::Error(EINVAL_VALUE)),
    };

    let prot_recognised =
        PROT_READ | PROT_WRITE | PROT_EXEC | PROT_NONE | PROT_GROWSDOWN | PROT_GROWSUP;
    if prot_bits & !prot_recognised != 0 {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }
    if prot_bits & (PROT_GROWSDOWN | PROT_GROWSUP) != 0 {
        return Some(SyscallResult::Error(ENOSYS_VALUE));
    }
    if flags & MAP_GROWSDOWN != 0 {
        return Some(SyscallResult::Error(EINVAL_VALUE));
    }

    let prot = Prot::new(
        prot_bits & PROT_READ != 0,
        prot_bits & PROT_WRITE != 0,
        prot_bits & PROT_EXEC != 0,
    );
    let entry_flags = VmEntryFlags::new(false, flags & MAP_GROWSDOWN != 0, flags & MAP_LOCKED != 0);
    let backing = VmBacking::PrivateAnon;
    let fixed = flags & MAP_FIXED != 0;
    let fixed_noreplace = flags & MAP_FIXED_NOREPLACE != 0;

    let request = if fixed || fixed_noreplace {
        if !UserVirtAddr::new(addr as usize).is_page_aligned() {
            return Some(SyscallResult::Error(EINVAL_VALUE));
        }
        let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
            Ok(r) => r,
            Err(_) => return Some(SyscallResult::Error(EINVAL_VALUE)),
        };
        let placement = if fixed_noreplace {
            MapPlacement::RequireFree
        } else {
            MapPlacement::FixedReplace
        };
        VmMapRequest::fixed(range, placement, prot, entry_flags, backing)
    } else {
        let window = match UserRange::new_aligned(
            UserVirtAddr(USER_PAGE_SIZE),
            UserRange::full_user_v1().len() - USER_PAGE_SIZE,
        ) {
            Ok(range) => range,
            Err(_) => return Some(SyscallResult::Error(EINVAL_VALUE)),
        };
        let page_count = length / USER_PAGE_SIZE;
        VmMapRequest::anywhere(window, page_count, prot, entry_flags, backing)
    };

    match aspace.try_mmap(request) {
        Ok(outcome) => Some(SyscallResult::Return(
            outcome.range.start().as_usize() as i64
        )),
        Err(VmMapError::WouldBlock) => None,
        Err(error) => {
            let code = if fixed_noreplace && error == VmMapError::AlreadyMapped {
                errno_to_i32(Errno::EEXIST)
            } else {
                vmmap_error_to_i32(error)
            };
            Some(SyscallResult::Error(code))
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

    match ctx.aspace.try_munmap(range) {
        Ok(_commit) => return SyscallResult::Return(0),
        Err(VmMapError::WouldBlock) => {}
        Err(error) => return SyscallResult::Error(vmmap_error_to_i32(error)),
    }

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
    if let Some(errno) = memlock_limit_error(ctx, range.len() as u64) {
        return SyscallResult::Error(errno);
    }
    if !range_fully_mapped(ctx, range) {
        return SyscallResult::Error(ENOMEM_VALUE);
    }

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
    if !range_fully_mapped(ctx, range) {
        return SyscallResult::Error(ENOMEM_VALUE);
    }

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

const MCL_CURRENT: u64 = 0x1;
const MCL_FUTURE: u64 = 0x2;
const MCL_ONFAULT: u64 = 0x4;
const MLOCK_ONFAULT: u64 = 0x1;

fn range_fully_mapped(ctx: &SyscallCtx<'_>, range: UserRange) -> bool {
    range
        .iter_pages()
        .all(|page| ctx.aspace.lookup(page.start_addr()).is_some())
}

fn memlock_limit_error(ctx: &SyscallCtx<'_>, bytes: u64) -> Option<i32> {
    if ctx.cred().euid.is_root() {
        return None;
    }
    let (cur, _) = ctx.process.rlimit_memlock();
    if bytes <= cur {
        None
    } else if cur == 0 {
        Some(EPERM_VALUE)
    } else {
        Some(ENOMEM_VALUE)
    }
}

/// `mlockall(flags)` — Linux RV64 generic syscall #230.
pub(super) async fn sys_mlockall(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let flags = args[0];
    if flags == 0 || flags & !(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT) != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if flags & MCL_CURRENT != 0 {
        let locked_len = ctx
            .aspace
            .recipes_snapshot()
            .into_iter()
            .try_fold(0u64, |acc, entry| acc.checked_add(entry.range.len() as u64));
        let Some(locked_len) = locked_len else {
            return SyscallResult::Error(ENOMEM_VALUE);
        };
        if let Some(errno) = memlock_limit_error(ctx, locked_len) {
            return SyscallResult::Error(errno);
        }
    }
    if flags & MCL_CURRENT != 0 {
        for entry in ctx.aspace.recipes_snapshot() {
            if let Err(errno) = drive_vm_lock(ctx, entry.range, true).await {
                return SyscallResult::error_from(Into::<Errno>::into(errno));
            }
        }
    }
    SyscallResult::Return(0)
}

/// `munlockall()` — Linux RV64 generic syscall #231.
pub(super) async fn sys_munlockall(_args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    for entry in ctx.aspace.recipes_snapshot() {
        if let Err(errno) = drive_vm_lock(ctx, entry.range, false).await {
            return SyscallResult::error_from(Into::<Errno>::into(errno));
        }
    }
    SyscallResult::Return(0)
}

async fn drive_vm_lock(
    ctx: &SyscallCtx<'_>,
    range: UserRange,
    locked: bool,
) -> Result<tx_subsystems::vm::VmMapCommit, V3Errno> {
    let mut script_ctx = build_subject_script_ctx(ctx);
    let mailbox_arc = script_ctx.mailbox().cloned();
    let timer_wheel_arc = script_ctx.timer_wheel().cloned();
    let delegate_registry_arc = script_ctx.delegate_registry().cloned();
    if locked {
        drive(
            VmMlockOp {
                aspace: &ctx.aspace,
                range,
            },
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_wheel_arc.as_ref(),
        )
        .await
    } else {
        drive(
            VmMunlockOp {
                aspace: &ctx.aspace,
                range,
            },
            &mut script_ctx,
            DriveMode::Waiting,
            mailbox_arc.as_ref(),
            delegate_registry_arc.as_deref(),
            timer_wheel_arc.as_ref(),
        )
        .await
    }
}

/// `mlock2(addr, len, flags)` — Linux RV64 generic syscall #284.
pub(super) async fn sys_mlock2(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let flags = args[2];
    if flags & !MLOCK_ONFAULT != 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    sys_mlock(args, ctx).await
}

/// `mincore(addr, length, vec)` — Linux RV64 generic syscall #232.
pub(super) fn sys_mincore(args: [u64; 6], ctx: &SyscallCtx<'_>) -> SyscallResult {
    let addr = args[0];
    let length_in = args[1] as usize;
    let vec_uaddr = args[2];

    if length_in == 0 {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    if !UserVirtAddr::new(addr as usize).is_page_aligned() {
        return SyscallResult::Error(EINVAL_VALUE);
    }
    let length = match length_in.checked_next_multiple_of(USER_PAGE_SIZE) {
        Some(rounded) => rounded,
        None => return SyscallResult::Error(ENOMEM_VALUE),
    };
    let range = match UserRange::new_aligned(UserVirtAddr::new(addr as usize), length) {
        Ok(range) => range,
        Err(_) => return SyscallResult::Error(ENOMEM_VALUE),
    };

    if range
        .iter_pages()
        .any(|page| ctx.aspace.lookup(page.start_addr()).is_none())
    {
        return SyscallResult::Error(ENOMEM_VALUE);
    }

    let resident = ctx.aspace.mincore(range);
    for (index, is_resident) in resident.into_iter().enumerate() {
        let byte = if is_resident { 1u8 } else { 0u8 };
        if let Err(errno) = bootstrap_write_user::<u8>(&ctx.aspace, vec_uaddr + index as u64, byte)
        {
            return SyscallResult::Error(errno_to_i32(errno));
        }
    }

    SyscallResult::Return(0)
}

/// `remap_file_pages(start, size, prot, pgoff, flags)` — Linux generic
/// syscall #234.
pub(super) fn sys_remap_file_pages(args: [u64; 6]) -> SyscallResult {
    let start = args[0];
    let size = args[1];
    let prot = args[2];
    let pgoff = args[3];
    let flags = args[4];

    if start == 0 && size == 0 && prot == 0 && pgoff == 0 && flags == 0 {
        return SyscallResult::Error(ENOSYS_VALUE);
    }

    SyscallResult::Error(EINVAL_VALUE)
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
    match ctx.aspace.try_mprotect(range, prot) {
        Ok(_) => {
            emit_vm_recipe_summary(b"debug.vm.recipe.mprotect", ctx);
            return SyscallResult::Return(0);
        }
        Err(VmMapError::WouldBlock) => {}
        Err(error) => return SyscallResult::Error(vmmap_error_to_i32(error)),
    }

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
        Ok(_commit) => {
            emit_vm_recipe_summary(b"debug.vm.recipe.mprotect", ctx);
            SyscallResult::Return(0)
        }
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
        // Advisory hints we do not act on yet — fork-inheritance
        // (DONTFORK/DOFORK), KSM (MERGEABLE/UNMERGEABLE), THP
        // (HUGEPAGE/NOHUGEPAGE), coredump (DONTDUMP/DODUMP),
        // WIPEONFORK/KEEPONFORK, and COLD/PAGEOUT. These are valid
        // Linux advices, so accept them as no-ops rather than failing
        // (LTP madvise01). Values are the stable generic
        // `<linux/mman.h>` numbers REMOVE(9)..PAGEOUT(21).
        9..=21 => return SyscallResult::Return(0),
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
    use tx_subsystems::vfs::structure::{OpenFileBacking, RNodeBacking};
    // Only VFS rnode-backed files have an rnode at all. Special fds
    // (epoll, eventfd, signalfd, timerfd, userfaultfd, io_uring, …) carry
    // no rnode and `OpenFile::rnode()` panics on them, so guard on the
    // OpenFile backing before touching it — callers (mmap, close-time
    // writeback) treat `None` as "no page container", which is correct
    // for these fd types.
    if !matches!(file.backing(), OpenFileBacking::Rnode { .. }) {
        return None;
    }
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
        VmMapError::PageAlloc(_) => errno_to_i32(Errno::ENOMEM),
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
/// Current support covers wait/wake, bitset wait/wake, requeue /
/// cmp-requeue, wake-op, and best-effort PI lock/trylock/unlock.
/// `FUTEX_PRIVATE_FLAG` is implicit in the per-aspace futex key.
/// `FUTEX_WAIT` treats `timeout` as a relative duration, while
/// `FUTEX_WAIT_BITSET` treats it as an absolute deadline
/// (`FUTEX_CLOCK_REALTIME` converts that absolute realtime value to
/// the monotonic timer base used by the reactor).
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
pub(super) async fn sys_futex<'a, P>(args: [u64; 6], ctx: &SyscallCtx<'a>) -> SyscallResult
where
    TimekeeperClock<P>: ClockRead,
{
    let uaddr = args[0];
    let op_full = args[1] as u32;
    let val = args[2] as u32;
    let timeout_uaddr = args[3];
    let val2 = args[3] as u32;
    let uaddr2 = args[4];
    let bitset = args[5] as u32;

    let op = op_full & FUTEX_CMD_MASK;
    let private = (op_full & FUTEX_PRIVATE_FLAG) != 0;

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
            let trace_wait = futex_trace_sample(&FUTEX_WAIT_TRACE_SAMPLE);
            if trace_wait {
                emit_futex_wait_entry(uaddr, val, op, wait_mask);
            }

            // Validate the user address before probing — the futex
            // subsystem reads *uaddr via `read_volatile` (bootstrap
            // exemption), which traps the kernel on an unmapped page.
            // A pre-check with the safe `read_user` accessor converts
            // the trap into a graceful -EFAULT.
            {
                let guard = step_engine::guard();
                let user_ptr = UserPtr::<u32>::new(uaddr as usize);
                if let StepOutcome::Err(_) = ctx.aspace.read_user(user_ptr, &guard) {
                    let result = SyscallResult::error_from(Errno::EFAULT);
                    if trace_wait {
                        emit_futex_result(b"debug.futex.wait.result", &result);
                    }
                    return result;
                }
            }
            let mut deadline_ns = None;
            if timeout_uaddr != 0 {
                let Some(timeout_ns) = read_timespec_at(&ctx.aspace, timeout_uaddr) else {
                    return SyscallResult::Error(EINVAL_VALUE);
                };
                let now_ns = timekeeper_clock::<P>().monotonic_now_ns();
                let next_deadline_ns = if op == FUTEX_WAIT_BITSET {
                    if (op_full & FUTEX_CLOCK_REALTIME) != 0 {
                        timekeeper().monotonic_deadline_from_realtime_ns(timeout_ns)
                    } else {
                        timeout_ns
                    }
                } else {
                    now_ns.saturating_add(timeout_ns)
                };
                if next_deadline_ns <= now_ns {
                    let guard = step_engine::guard();
                    let user_ptr = UserPtr::<u32>::new(uaddr as usize);
                    match ctx.aspace.read_user(user_ptr, &guard) {
                        StepOutcome::Done(observed) if observed == val => {
                            let result = SyscallResult::Error(110);
                            if trace_wait {
                                emit_futex_result(b"debug.futex.wait.result", &result);
                            }
                            return result;
                        }
                        StepOutcome::Done(_) => {
                            let result = SyscallResult::Error(EAGAIN_VALUE);
                            if trace_wait {
                                emit_futex_result(b"debug.futex.wait.result", &result);
                            }
                            return result;
                        }
                        StepOutcome::Err(_) => {
                            let result = SyscallResult::error_from(Errno::EFAULT);
                            if trace_wait {
                                emit_futex_result(b"debug.futex.wait.result", &result);
                            }
                            return result;
                        }
                        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
                            let result = SyscallResult::error_from(Errno::EFAULT);
                            if trace_wait {
                                emit_futex_result(b"debug.futex.wait.result", &result);
                            }
                            return result;
                        }
                    }
                }
                deadline_ns = Some(next_deadline_ns);
            }

            let mut script_ctx = build_subject_script_ctx(ctx);
            if let Some(deadline_ns) = deadline_ns {
                script_ctx = script_ctx.with_deadline(Deadline::from_raw(deadline_ns));
            }
            let mailbox_arc = script_ctx.mailbox().cloned();
            let timer_registrar_handle = script_ctx.timer_registrar().cloned();
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
                tid: Some(ctx.thread.tid.0),
                woken: false,
                waiting: false,
                registered_source_id: None,
                private,
            };
            match drive(
                op,
                &mut script_ctx,
                DriveMode::Waiting,
                mailbox_arc.as_ref(),
                delegate_registry_arc.as_deref(),
                timer_registrar_handle.as_ref(),
            )
            .await
            {
                Ok(()) => {
                    let result = SyscallResult::Return(0);
                    if trace_wait {
                        emit_futex_result(b"debug.futex.wait.result", &result);
                    }
                    result
                }
                Err(v3errno) => {
                    let errno: Errno = v3errno.into();
                    let result = SyscallResult::error_from(errno);
                    if trace_wait {
                        emit_futex_result(b"debug.futex.wait.result", &result);
                    }
                    result
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
            let trace_wake = futex_trace_sample(&FUTEX_WAKE_TRACE_SAMPLE);
            if trace_wake {
                emit_futex_wake_entry(uaddr, val, op, wake_mask);
            }
            let result = futex_wake_oneshot(
                ctx,
                uaddr,
                val,
                wake_mask,
                MailboxSchedulerHint::WakeHandoff,
                private,
            );
            if trace_wake {
                emit_futex_result(b"debug.futex.wake.result", &result);
            }
            result
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
            let outcome = tx_subsystems::futex::step_futex_requeue_scoped_in(
                &ctx.aspace,
                uaddr,
                uaddr2,
                val,
                val2,
                private,
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
            let first = match futex_wake_count(ctx, uaddr, val, private) {
                Ok(woken) => woken,
                Err(result) => return result,
            };
            let second = match futex_wake_count(ctx, uaddr2, val2, private) {
                Ok(woken) => woken,
                Err(result) => return result,
            };
            SyscallResult::Return(first.saturating_add(second) as i64)
        }
        FUTEX_LOCK_PI => futex_pi_lock(ctx, uaddr, false),
        FUTEX_TRYLOCK_PI => futex_pi_lock(ctx, uaddr, true),
        FUTEX_UNLOCK_PI => futex_pi_unlock(ctx, uaddr, private),
        // FUTEX_REQUEUE / CMP_REQUEUE / WAKE_OP / LOCK_PI /
        // UNLOCK_PI / TRYLOCK_PI / WAIT_BITSET / WAKE_BITSET — out
        // of scope for v1. musl's libc init only emits FUTEX_WAIT
        // and FUTEX_WAKE so these are not on the critical path.
        _ => SyscallResult::Error(ENOSYS_VALUE),
    }
}

pub(super) fn sys_futex_oneshot(args: [u64; 6], ctx: &SyscallCtx<'_>) -> Option<SyscallResult> {
    sys_futex_oneshot_with_wake_hint(args, ctx, MailboxSchedulerHint::WakeHandoff)
}

pub(super) fn sys_futex_oneshot_with_wake_hint(
    args: [u64; 6],
    ctx: &SyscallCtx<'_>,
    wake_hint: MailboxSchedulerHint,
) -> Option<SyscallResult> {
    let uaddr = args[0];
    let op_full = args[1] as u32;
    let val = args[2] as u32;
    let bitset = args[5] as u32;
    let op = op_full & FUTEX_CMD_MASK;
    let private = (op_full & FUTEX_PRIVATE_FLAG) != 0;

    match op {
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            if op == FUTEX_WAKE_BITSET && bitset == 0 {
                return Some(SyscallResult::Error(EINVAL_VALUE));
            }
            let wake_mask = if op == FUTEX_WAKE_BITSET {
                bitset as u64
            } else {
                tx_subsystems::futex::FUTEX_WAKE_MASK
            };
            let trace_wake = futex_trace_sample(&FUTEX_WAKE_TRACE_SAMPLE);
            if trace_wake {
                emit_futex_wake_entry(uaddr, val, op, wake_mask);
            }
            let result = futex_wake_oneshot(ctx, uaddr, val, wake_mask, wake_hint, private);
            if trace_wake {
                emit_futex_result(b"debug.futex.wake.result", &result);
            }
            Some(result)
        }
        _ => None,
    }
}

fn futex_wake_oneshot(
    ctx: &SyscallCtx<'_>,
    uaddr: u64,
    n: u32,
    wake_mask: u64,
    wake_hint: MailboxSchedulerHint,
    private: bool,
) -> SyscallResult {
    match futex_wake_count_masked_with_hint(ctx, uaddr, n, wake_mask, wake_hint, private) {
        Ok(woken) => SyscallResult::Return(woken as i64),
        Err(result) => result,
    }
}

fn futex_wake_count(
    ctx: &SyscallCtx<'_>,
    uaddr: u64,
    n: u32,
    private: bool,
) -> Result<u32, SyscallResult> {
    futex_wake_count_masked(
        ctx,
        uaddr,
        n,
        tx_subsystems::futex::FUTEX_WAKE_MASK,
        private,
    )
}

fn futex_wake_count_masked(
    ctx: &SyscallCtx<'_>,
    uaddr: u64,
    n: u32,
    wake_mask: u64,
    private: bool,
) -> Result<u32, SyscallResult> {
    futex_wake_count_masked_with_hint(
        ctx,
        uaddr,
        n,
        wake_mask,
        MailboxSchedulerHint::WakeHandoff,
        private,
    )
}

fn futex_wake_count_masked_with_hint(
    ctx: &SyscallCtx<'_>,
    uaddr: u64,
    n: u32,
    wake_mask: u64,
    wake_hint: MailboxSchedulerHint,
    private: bool,
) -> Result<u32, SyscallResult> {
    let guard = step_engine::guard();
    let outcome = tx_subsystems::futex::step_futex_wake_masked_with_hint_scoped_and_post_in(
        &ctx.aspace,
        uaddr,
        n,
        wake_mask,
        wake_hint,
        private,
        &guard,
        |mailbox, event, hint| ctx.post_mailbox_ref_event_with_hint(mailbox, event, hint),
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

fn futex_pi_unlock(ctx: &SyscallCtx<'_>, uaddr: u64, private: bool) -> SyscallResult {
    let owner = futex_owner_tid(ctx);
    let guard = step_engine::guard();
    let outcome = tx_subsystems::futex::step_futex_unlock_pi_scoped_in(
        &ctx.aspace,
        uaddr,
        owner,
        private,
        &guard,
    );
    drop(guard);
    match outcome {
        StepOutcome::Done(_) => SyscallResult::Return(0),
        StepOutcome::Err(errno) => SyscallResult::error_from(Errno::from(errno)),
        StepOutcome::Yield { .. } | StepOutcome::Continue { .. } => {
            SyscallResult::error_from(Errno::EIO)
        }
    }
}
