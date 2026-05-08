//! VM-resolved user-buffer copying.
//!
//! User virtual addresses are resolved through `AddressSpace` recipes and
//! pmap materializations before bytes are copied through direct-map kernel
//! pointers. Architecture fixup paths remain a HAL fallback; this module is
//! the normal copyin/copyout path.

use core::cmp;

use tx_hal::{KernelPtr, UserPtr};
use tx_substrate::page_allocator;

use crate::execution::{Errno, Guard, StepOutcome};

use super::{
    AccessMode, AddressSpace, LockMode, PmapMappingSnapshot, UserRange, UserRangeError,
    UserVirtAddr, VmBacking, VmEntry, VmFaultError, VmFaultMaterialization,
    VmFaultMaterializationBacking, VmFaultOutcome, USER_PAGE_SIZE,
};

#[derive(Clone, Copy)]
enum CopyDirection {
    FromUser,
    ToUser,
}

impl CopyDirection {
    const fn access(self) -> AccessMode {
        match self {
            Self::FromUser => AccessMode::Read,
            Self::ToUser => AccessMode::Write,
        }
    }
}

/// Copy `len` bytes from `src` in `aspace` into the kernel buffer `dst`.
///
/// # Safety
///
/// `dst` must be valid writable kernel memory for `len` bytes.
pub unsafe fn copy_from_user(
    aspace: &AddressSpace,
    dst: KernelPtr<u8>,
    src: UserPtr<u8>,
    len: usize,
) -> StepOutcome<usize> {
    let guard = tx_substrate::epoch::guard();
    unsafe { copy_from_user_with_guard(aspace, &guard, dst, src, len) }
}

/// Copy `len` bytes from `src` in `aspace` into the kernel buffer `dst`,
/// reusing the caller's epoch guard.
///
/// # Safety
///
/// `dst` must be valid writable kernel memory for `len` bytes.
pub unsafe fn copy_from_user_with_guard(
    aspace: &AddressSpace,
    guard: &Guard<'_>,
    dst: KernelPtr<u8>,
    src: UserPtr<u8>,
    len: usize,
) -> StepOutcome<usize> {
    unsafe {
        copy_user_bytes(
            aspace,
            guard,
            dst.as_ptr(),
            src.addr(),
            len,
            CopyDirection::FromUser,
        )
    }
}

/// Copy `len` bytes from kernel buffer `src` into `dst` in `aspace`.
///
/// # Safety
///
/// `src` must be valid readable kernel memory for `len` bytes.
pub unsafe fn copy_to_user(
    aspace: &AddressSpace,
    dst: UserPtr<u8>,
    src: KernelPtr<u8>,
    len: usize,
) -> StepOutcome<usize> {
    let guard = tx_substrate::epoch::guard();
    unsafe { copy_to_user_with_guard(aspace, &guard, dst, src, len) }
}

/// Copy `len` bytes from kernel buffer `src` into `dst` in `aspace`, reusing
/// the caller's epoch guard.
///
/// # Safety
///
/// `src` must be valid readable kernel memory for `len` bytes.
pub unsafe fn copy_to_user_with_guard(
    aspace: &AddressSpace,
    guard: &Guard<'_>,
    dst: UserPtr<u8>,
    src: KernelPtr<u8>,
    len: usize,
) -> StepOutcome<usize> {
    unsafe {
        copy_user_bytes(
            aspace,
            guard,
            src.as_ptr(),
            dst.addr(),
            len,
            CopyDirection::ToUser,
        )
    }
}

unsafe fn copy_user_bytes(
    aspace: &AddressSpace,
    guard: &Guard<'_>,
    kernel_ptr: *mut u8,
    user_addr: usize,
    len: usize,
    direction: CopyDirection,
) -> StepOutcome<usize> {
    if len == 0 {
        return StepOutcome::Done(0);
    }

    let range = match covering_range(user_addr, len) {
        Ok(range) => range,
        Err(errno) => return StepOutcome::Err(errno),
    };
    let _guard = match aspace
        .range_lock()
        .acquire_step(range, LockMode::Materializer)
    {
        StepOutcome::Done(guard) => guard,
        StepOutcome::Blocked(token) => return StepOutcome::Blocked(token),
        StepOutcome::Advanced(_) | StepOutcome::AdvancedThenBlocked(_, _) => {
            unreachable!("RangeLock acquire_step cannot advance")
        }
        StepOutcome::Err(errno) => return StepOutcome::Err(errno),
    };

    let mut advanced = 0usize;
    while advanced < len {
        let current_user = user_addr + advanced;
        let run = match resolve_copy_run(
            aspace,
            guard,
            current_user,
            len - advanced,
            direction.access(),
        ) {
            Ok(run) => run,
            Err(errno) => {
                return if advanced == 0 {
                    StepOutcome::Err(errno)
                } else {
                    StepOutcome::Done(advanced)
                };
            }
        };

        unsafe {
            let kernel_run = kernel_ptr.add(advanced);
            let user_run = run.ptr;
            match direction {
                CopyDirection::FromUser => {
                    core::ptr::copy_nonoverlapping(user_run.cast_const(), kernel_run, run.len);
                }
                CopyDirection::ToUser => {
                    core::ptr::copy_nonoverlapping(kernel_run.cast_const(), user_run, run.len);
                }
            }
        }
        advanced += run.len;
    }

    StepOutcome::Done(advanced)
}

struct CopyRun {
    ptr: *mut u8,
    len: usize,
}

fn resolve_copy_run(
    aspace: &AddressSpace,
    guard: &Guard<'_>,
    user_addr: usize,
    remaining: usize,
    access: AccessMode,
) -> Result<CopyRun, Errno> {
    let entry = lookup_entry(aspace, guard, UserVirtAddr(user_addr)).ok_or(Errno::EFAULT)?;
    if !entry.prot.permits(access) {
        return Err(Errno::EFAULT);
    }

    let entry_remaining = entry
        .range
        .end()
        .as_usize()
        .checked_sub(user_addr)
        .ok_or(Errno::EFAULT)?;
    let max_len = cmp::min(remaining, entry_remaining);
    let first = resolve_page(aspace, guard, user_addr, access)?;
    let page_offset = user_addr % USER_PAGE_SIZE;
    let mut run_len = cmp::min(max_len, USER_PAGE_SIZE - page_offset);
    let mut expected_next_ppn = first.ppn.0 + 1;

    while run_len < max_len {
        let next_addr = user_addr + run_len;
        if !next_addr.is_multiple_of(USER_PAGE_SIZE) {
            break;
        }
        let next = resolve_page(aspace, guard, next_addr, access)?;
        if next.ppn.0 != expected_next_ppn || !next.prot.permits(access) {
            break;
        }
        let chunk = cmp::min(max_len - run_len, USER_PAGE_SIZE);
        run_len += chunk;
        expected_next_ppn += 1;
    }

    let frame_base = page_allocator::frame_kernel_addr(first.ppn).map_err(|_| Errno::EIO)?;
    Ok(CopyRun {
        ptr: unsafe { frame_base.add(page_offset) },
        len: run_len,
    })
}

fn resolve_page(
    aspace: &AddressSpace,
    guard: &Guard<'_>,
    user_addr: usize,
    access: AccessMode,
) -> Result<PmapMappingSnapshot, Errno> {
    let va = UserVirtAddr(user_addr);
    let page = va.containing_page();
    if let Some(snapshot) = aspace.pmap().lookup(page) {
        if snapshot.prot.permits(access) {
            return Ok(snapshot);
        }
    }

    let outcome = resolve_fault_with_guard(aspace, guard, va, access).map_err(errno_from_fault)?;
    let materialization = outcome.materialize_pagebacked().map_err(errno_from_fault)?;
    publish_fault_materialization_with_guard(aspace, guard, &outcome, &materialization)
        .map_err(errno_from_fault)?;
    aspace
        .pmap()
        .publish_page_with_replacement(
            outcome.page_range.start().containing_page(),
            materialization.page.ppn,
            materialization.publish_prot,
            materialization.page.map_pin,
            materialization.replace_existing,
        )
        .map_err(|error| errno_from_fault(VmFaultError::Pmap(error)))?;

    aspace
        .pmap()
        .lookup(page)
        .filter(|snapshot| snapshot.prot.permits(access))
        .ok_or(Errno::EFAULT)
}

fn resolve_fault_with_guard(
    aspace: &AddressSpace,
    guard: &Guard<'_>,
    va: UserVirtAddr,
    access: AccessMode,
) -> Result<VmFaultOutcome, VmFaultError> {
    let page_range = UserRange::containing_page(va).map_err(VmFaultError::Range)?;
    let entry = lookup_entry(aspace, guard, va).ok_or(VmFaultError::NoRecipe)?;
    if !entry.prot.permits(access) {
        return Err(VmFaultError::ProtectionViolation);
    }
    Ok(VmFaultOutcome {
        page_range,
        entry,
        access,
        pmap_materialization_deferred: aspace.pmap().materialization_deferred(),
    })
}

fn publish_fault_materialization_with_guard(
    aspace: &AddressSpace,
    guard: &Guard<'_>,
    outcome: &VmFaultOutcome,
    materialization: &VmFaultMaterialization,
) -> Result<VmEntry, VmFaultError> {
    let entry =
        lookup_entry(aspace, guard, outcome.page_range.start()).ok_or(VmFaultError::StaleRecipe)?;
    if entry != outcome.entry || !entry.prot.permits(outcome.access) {
        return Err(VmFaultError::StaleRecipe);
    }
    match (&entry.backing, materialization.backing) {
        (VmBacking::Page { .. }, VmFaultMaterializationBacking::PageBacked) => {
            if outcome.backing_page_index()? != materialization.page_index {
                return Err(VmFaultError::StaleRecipe);
            }
        }
        (VmBacking::PrivateAnon, VmFaultMaterializationBacking::PrivateAnon) => {
            if outcome.private_anon_page_index()? != materialization.page_index {
                return Err(VmFaultError::StaleRecipe);
            }
        }
        _ => return Err(VmFaultError::StaleRecipe),
    }
    Ok(entry)
}

fn lookup_entry(aspace: &AddressSpace, guard: &Guard<'_>, addr: UserVirtAddr) -> Option<VmEntry> {
    aspace.recipes.lookup(addr, guard)
}

fn covering_range(start: usize, len: usize) -> Result<UserRange, Errno> {
    let end = start.checked_add(len).ok_or(Errno::EFAULT)?;
    let top = UserRange::full_user_v1().end().as_usize();
    if start >= top || end > top {
        return Err(Errno::EFAULT);
    }

    let aligned_start = start - (start % USER_PAGE_SIZE);
    let aligned_end = align_up(end, USER_PAGE_SIZE).ok_or(Errno::EFAULT)?;
    UserRange::new_aligned(UserVirtAddr(aligned_start), aligned_end - aligned_start)
        .map_err(errno_from_range)
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

const fn errno_from_range(error: UserRangeError) -> Errno {
    match error {
        UserRangeError::ZeroLength | UserRangeError::Unaligned | UserRangeError::Overflow => {
            Errno::EFAULT
        }
    }
}

const fn errno_from_fault(error: VmFaultError) -> Errno {
    match error {
        VmFaultError::WouldBlock => Errno::EBUSY,
        VmFaultError::Pmap(_) => Errno::EIO,
        VmFaultError::PageCache(_) => Errno::ENOMEM,
        VmFaultError::Range(_)
        | VmFaultError::NoRecipe
        | VmFaultError::ProtectionViolation
        | VmFaultError::BackingMismatch
        | VmFaultError::BackingOffsetOverflow
        | VmFaultError::PageBeyondSize
        | VmFaultError::StaleRecipe => Errno::EFAULT,
    }
}
