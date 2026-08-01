//! User-space memory copy primitives for syscall argument decoding.
//!
//! Each function bridges through the v3 step-engine guard API, borrowing
//! an active epoch guard when the caller already holds one and otherwise
//! taking a fresh `step_engine::guard()` inside the call site per the
//! two-site discipline (`txdoc:VM-3-6-CROSS-ASYNC-WAIT-DISCIPLINE`).
//!
//! ## Bootstrap convention
//!
//! `bootstrap_*` functions fall back to a kernel-pointer scan on `EFAULT`
//! when the address space is identity-mapped (the bootstrap phase before
//! full page-table isolation). Once mmap-based user mappings land,
//! the fallback lane becomes dead code.

extern crate alloc;

use crate::adapter::step_engine;
use crate::adapter::step_engine::StepOutcome;
use alloc::vec::Vec;
use tx_hal::UserPtr;
use tx_subsystems::execution::Errno;
use tx_subsystems::vm::{
    AddressSpace, UserAccessKind, UserPage, UserRange, FULL_USER_V1_TOP, USER_PAGE_SIZE,
};

/// Outcome of `read_user_cstr` — distinguishes "no NUL within budget"
/// from a successful copy. The successful arm yields the bytes up to
/// (not including) the NUL terminator, allocated as a kernel-owned
/// `Vec<u8>`.
pub(super) enum ReadCStrError {
    /// No NUL within `max_len` — surface as `-ENAMETOOLONG`.
    TooLong,
    /// Canonical user-copy error, including `ENOMEM` when the kernel-side
    /// destination cannot grow.
    Fault(Errno),
}

/// Outcome of `read_user_cstr_vec`. `TooBig` covers both
/// pointer-array overflow and aggregate-byte overflow; both surface
/// as `-E2BIG` per the Phase 6 plan.
pub(super) enum ReadVecError {
    TooBig,
    OutOfMemory,
    Fault(Errno),
}

#[cfg(test)]
static USER_COPY_ALLOCATION_FAIL_AFTER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

#[cfg(test)]
pub(super) struct UserCopyAllocationFailureGuard;

#[cfg(test)]
impl Drop for UserCopyAllocationFailureGuard {
    fn drop(&mut self) {
        USER_COPY_ALLOCATION_FAIL_AFTER.store(usize::MAX, core::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
pub(super) fn fail_user_copy_allocation_after_for_test(
    successful_reservations: usize,
) -> UserCopyAllocationFailureGuard {
    USER_COPY_ALLOCATION_FAIL_AFTER.store(
        successful_reservations,
        core::sync::atomic::Ordering::Release,
    );
    UserCopyAllocationFailureGuard
}

pub(super) fn try_reserve_user_copy_items<T>(
    items: &mut Vec<T>,
    additional: usize,
) -> Result<(), ()> {
    if additional == 0 {
        return Ok(());
    }
    #[cfg(test)]
    {
        use core::sync::atomic::Ordering;
        let remaining = USER_COPY_ALLOCATION_FAIL_AFTER.load(Ordering::Acquire);
        if remaining != usize::MAX {
            if remaining == 0 {
                return Err(());
            }
            USER_COPY_ALLOCATION_FAIL_AFTER.fetch_sub(1, Ordering::AcqRel);
        }
    }
    items.try_reserve_exact(additional).map_err(|_| ())
}

fn user_access_guard() -> step_engine::Guard<'static> {
    step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard)
}

/// Bounded copy of a NUL-terminated user string into a kernel-owned
/// `Vec<u8>` (NUL terminator stripped).
///
/// Normal pathname-bearing syscalls expect Linux semantics here:
/// `uaddr == 0` is `-EFAULT`, a missing NUL within `max_len` is
/// `-ENAMETOOLONG`, allocation failure is `-ENOMEM`, and other
/// canonical user-copy failures preserve their original errno.
pub(super) fn read_user_cstr(
    aspace: &AddressSpace,
    uaddr: u64,
    max_len: usize,
) -> Result<Vec<u8>, ReadCStrError> {
    if uaddr == 0 {
        return Err(ReadCStrError::Fault(Errno::EFAULT));
    }
    if max_len == 0 {
        return Err(ReadCStrError::TooLong);
    }
    match bootstrap_read_user_cstr(aspace, uaddr, max_len) {
        Ok(v) => Ok(v),
        Err(Errno::ENAMETOOLONG) => Err(ReadCStrError::TooLong),
        Err(errno) => Err(ReadCStrError::Fault(errno)),
    }
}

/// Wait-capable runtime counterpart of [`read_user_cstr`].
///
/// `AddressSpace::read_user_cstr` may yield while another hart materialises
/// the page containing the string.  A runtime syscall must yield and retry;
/// translating that normal SMP race to `EIO` makes process launch fail before
/// `execve` is reached.
pub(super) async fn read_user_cstr_wait(
    aspace: &AddressSpace,
    uaddr: u64,
    max_len: usize,
) -> Result<Vec<u8>, ReadCStrError> {
    use step_engine::{Errno as V3Errno, StepOutcome as V3};

    if uaddr == 0 {
        return Err(ReadCStrError::Fault(Errno::EFAULT));
    }
    if max_len == 0 {
        return Err(ReadCStrError::TooLong);
    }

    #[cfg(not(target_os = "none"))]
    {
        return read_user_cstr(aspace, uaddr, max_len);
    }

    #[cfg(target_os = "none")]
    loop {
        let outcome = {
            let guard = user_access_guard();
            aspace.read_user_cstr(UserPtr::<u8>::new(uaddr as usize), max_len, &guard)
        };
        match outcome {
            V3::Done(value) => return Ok(value),
            V3::Err(V3Errno::ENAMETOOLONG) => return Err(ReadCStrError::TooLong),
            V3::Err(error) => return Err(ReadCStrError::Fault(error.into())),
            V3::Continue { .. } | V3::Yield { .. } => {
                // The competing materializer owns the wait token. Yielding the
                // task lets that transaction publish before we retry the
                // bounded scan; no epoch guard crosses this await.
                tx_reactor::yield_now().await;
            }
        }
    }
}

/// Bounded copy of a NULL-terminated array of `*const u8` user
/// pointers into a kernel-owned `Vec<Vec<u8>>`. Each non-NULL entry
/// resolves to its own NUL-terminated string. The aggregate-byte
/// budget shared across argv + envp is passed in through
/// `byte_budget` (decremented in place).
///
/// `uaddr == 0` produces an empty vector — matches Linux's lenience
/// for `execve(path, NULL, NULL)` per the Phase 6 plan.
///
/// Each pointer slot and each string read bridges through the
/// canonical user-VA lane (`bootstrap_read_user` /
/// `bootstrap_read_user_cstr`), falling back to the kernel-pointer
/// dance on EFAULT for test scaffolding.
pub(super) fn read_user_cstr_vec(
    aspace: &AddressSpace,
    uaddr: u64,
    max_slots: usize,
    byte_budget: &mut usize,
) -> Result<Vec<Vec<u8>>, ReadVecError> {
    if uaddr == 0 {
        return Ok(Vec::new());
    }
    if max_slots > (u64::MAX as usize) / core::mem::size_of::<u64>() {
        return Err(ReadVecError::TooBig);
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    for slot in 0..max_slots {
        let slot_offset = slot
            .checked_mul(core::mem::size_of::<u64>())
            .and_then(|offset| u64::try_from(offset).ok())
            .ok_or(ReadVecError::TooBig)?;
        let slot_addr = uaddr
            .checked_add(slot_offset)
            .ok_or(ReadVecError::Fault(Errno::EFAULT))?;
        let ptr = match bootstrap_read_user::<u64>(aspace, slot_addr) {
            Ok(p) => p,
            Err(errno) => return Err(ReadVecError::Fault(errno)),
        };
        if ptr == 0 {
            return Ok(out);
        }
        // Read the string at `ptr`, capped at the remaining byte
        // budget. We need at least one byte for the NUL terminator;
        // when `*byte_budget == 0` any non-empty string is `TooBig`.
        let cap = *byte_budget;
        let s = read_user_exec_cstr(aspace, ptr, cap)?;
        // Account `s.len() + 1` for the implicit NUL byte we read but
        // did not store, matching Linux's `ARG_MAX` accounting.
        let charged = s.len().saturating_add(1);
        if charged > *byte_budget {
            return Err(ReadVecError::TooBig);
        }
        *byte_budget -= charged;
        try_reserve_user_copy_items(&mut out, 1).map_err(|_| ReadVecError::OutOfMemory)?;
        out.push(s);
    }
    // Hit the slot cap without observing a NULL terminator — treat
    // as oversized argv per the plan.
    Err(ReadVecError::TooBig)
}

fn read_user_exec_cstr(
    aspace: &AddressSpace,
    uaddr: u64,
    max_len: usize,
) -> Result<Vec<u8>, ReadVecError> {
    if uaddr == 0 {
        return Err(ReadVecError::Fault(Errno::EFAULT));
    }
    if max_len == 0 {
        return Err(ReadVecError::TooBig);
    }

    let mut out = Vec::new();
    for offset in 0..max_len {
        let offset = u64::try_from(offset).map_err(|_| ReadVecError::TooBig)?;
        let byte_addr = uaddr
            .checked_add(offset)
            .ok_or(ReadVecError::Fault(Errno::EFAULT))?;
        let byte = bootstrap_read_user::<u8>(aspace, byte_addr).map_err(ReadVecError::Fault)?;
        if byte == 0 {
            return Ok(out);
        }
        if out.len() == out.capacity() {
            let additional = if out.capacity() == 0 {
                core::cmp::min(max_len, 256)
            } else {
                core::cmp::min(out.capacity(), max_len - out.len())
            };
            try_reserve_user_copy_items(&mut out, additional)
                .map_err(|_| ReadVecError::OutOfMemory)?;
        }
        out.push(byte);
    }
    Err(ReadVecError::TooBig)
}

// =====================================================================
// User-VA bridging helpers.
//
// Phase userva-sweep: every syscall arm that previously dereferenced a
// userspace pointer through the bootstrap `core::ptr::read_volatile` /
// `write_volatile` exemption now routes through one of the bridging
// helpers below. Each helper:
//
// 1. Calls the canonical `aspace.copy_*_user` / `read_user` /
//    `write_user` / `read_user_cstr` lane which walks the AddressSpace's
//    recipes, materialises every covered page through its
//    `VmBacking`, publishes the page to pmap (so subsequent calls see
//    the same frame — see `vm/user_access.rs` module header), and
//    copies through the kernel direct-map view. This is the "real"
//    user-VA path that exec'd processes (and the bake-in fixture
//    after exec) follow.
// 2. On `Errno::EFAULT` (no recipe covers the address — typical for
//    unit-test scaffolding that passes kernel stack/heap pointers
//    directly), falls back to the bootstrap kernel-pointer dance
//    (`core::ptr::read_volatile` / `write_volatile`) the previous
//    user-VA-deferred sites used inline before the userva sweep.
//
// The fallback exists because the existing dispatch tests pass kernel
// pointers (e.g. `buf.as_ptr() as u64`, `&mut set as *mut u64 as u64`)
// directly: a fresh `AddressSpace` has no recipes covering them, so a
// pure `aspace.copy_*_user` call would EFAULT. The fallback is a
// bridge until those tests migrate to user-VA-shaped fixtures
// (`map_user_buffer + seed`); for the bake-in `init` fixture (which
// runs through `exec_script`) the user-VA path always succeeds and
// the fallback is never exercised.
//
// `Blocked` outcomes from the canonical path are awaited inside the
// bridge for sync helpers; async-context helpers surface the token to
// the caller. Today no in-tree backend produces `Blocked` from a
// user-buffer copy on the synchronous path (anon page-cache
// materialisation is sync, file-backed reads await up at the file's
// `step_read` lane), so the awaiting code is a defensive scaffold for
// future async-aware backings.
// =====================================================================

/// Read a `T: Copy` value from `uaddr` through the canonical
/// `aspace.read_user` lane, falling back to the bootstrap
/// kernel-pointer dance on `EFAULT`.
pub(super) fn bootstrap_read_user<T: Copy>(aspace: &AddressSpace, uaddr: u64) -> Result<T, Errno> {
    use step_engine::{Errno as V3Errno, StepOutcome as V3};
    let guard = user_access_guard();
    match aspace.read_user(UserPtr::<T>::new(uaddr as usize), &guard) {
        V3::Done(v) => Ok(v),
        V3::Err(V3Errno::EFAULT) => {
            drop(guard);
            #[cfg(target_os = "none")]
            {
                Err(Errno::EFAULT)
            }
            #[cfg(not(target_os = "none"))]
            {
                let limit = if cfg!(any(test, feature = "test-support")) {
                    0x1000
                } else {
                    FULL_USER_V1_TOP as u64
                };
                if uaddr < limit {
                    return Err(Errno::EFAULT);
                }
                Ok(unsafe { core::ptr::read_volatile(uaddr as *const T) })
            }
        }
        V3::Err(e) => Err(e.into()),
        V3::Yield { .. } | V3::Continue { .. } => Err(Errno::EIO),
    }
}

/// Write a `T: Copy` value to `uaddr` through the canonical
/// `aspace.write_user` lane, falling back to the bootstrap
/// kernel-pointer dance on `EFAULT`.
pub(super) fn bootstrap_write_user<T: Copy>(
    aspace: &AddressSpace,
    uaddr: u64,
    value: T,
) -> Result<(), Errno> {
    use StepOutcome as V3;
    let guard = user_access_guard();
    match aspace.write_user(UserPtr::<T>::new(uaddr as usize), value, &guard) {
        V3::Done(()) | V3::Continue { .. } => Ok(()),
        V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
            drop(guard);
            #[cfg(target_os = "none")]
            {
                Err(Errno::EFAULT)
            }
            #[cfg(not(target_os = "none"))]
            {
                let limit = if cfg!(any(test, feature = "test-support")) {
                    0x1000
                } else {
                    FULL_USER_V1_TOP as u64
                };
                if uaddr < limit {
                    return Err(Errno::EFAULT);
                }
                unsafe {
                    core::ptr::write_volatile(uaddr as *mut T, value);
                }
                Ok(())
            }
        }
        V3::Err(e) => Err(Errno::from(e)),
        V3::Yield { .. } => Err(Errno::EIO),
    }
}

/// Compute the page-aligned `UserRange` covering the byte range
/// `[uaddr, uaddr + len)`. Returns `None` when `len == 0` or when
/// `uaddr` addition overflows.
pub(super) fn covering_user_range(uaddr: u64, len: usize) -> Option<UserRange> {
    if len == 0 {
        return None;
    }
    let uaddr = uaddr as usize;
    let start_page_addr = uaddr & !(USER_PAGE_SIZE - 1);
    let end = uaddr.checked_add(len)?;
    let end_page_addr = (end + USER_PAGE_SIZE - 1) & !(USER_PAGE_SIZE - 1);
    let page_count = (end_page_addr - start_page_addr) / USER_PAGE_SIZE;
    UserRange::from_pages(UserPage(start_page_addr / USER_PAGE_SIZE), page_count).ok()
}

/// Copy `dst.len()` bytes from user-space `uaddr` into the kernel-side
/// buffer `dst`. Bridges through `aspace.copy_from_user`, falling back
/// to a kernel-pointer memcpy on `EFAULT`.
///
/// Before copying, eagerly prefaults the entire user range through
/// `reserve_user_range_for_access` (per PAGE_BACKED_v1 §3 prefault
/// discipline). If any page is unmapped or has a protection mismatch,
/// EFAULT is surfaced before any bytes are transferred.
pub(super) fn bootstrap_copy_from_user(
    aspace: &AddressSpace,
    dst: &mut [u8],
    uaddr: u64,
) -> Result<(), Errno> {
    use StepOutcome as V3;
    if dst.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "none")]
    {
        if let Some(range) = covering_user_range(uaddr, dst.len()) {
            match aspace.reserve_user_range_for_access(range, UserAccessKind::Read) {
                V3::Done(()) => {}
                V3::Err(e) => return Err(Errno::from(e)),
                V3::Yield { .. } | V3::Continue { .. } => return Err(Errno::EIO),
            }
        }

        let guard = user_access_guard();
        match aspace.copy_from_user(dst, UserPtr::<u8>::new(uaddr as usize), &guard) {
            V3::Done(_) | V3::Continue { .. } => Ok(()),
            V3::Err(e) => Err(Errno::from(e)),
            V3::Yield { .. } => Err(Errno::EIO),
        }
    }

    #[cfg(not(target_os = "none"))]
    {
        let mut prefault_failed_with_efault = false;
        if let Some(range) = covering_user_range(uaddr, dst.len()) {
            match aspace.reserve_user_range_for_access(range, UserAccessKind::Read) {
                V3::Done(()) => {}
                V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
                    let limit = if cfg!(any(test, feature = "test-support")) {
                        0x1000
                    } else {
                        FULL_USER_V1_TOP as u64
                    };
                    if uaddr < limit {
                        return Err(Errno::EFAULT);
                    }
                    prefault_failed_with_efault = true;
                }
                V3::Err(e) => return Err(Errno::from(e)),
                V3::Yield { .. } | V3::Continue { .. } => return Err(Errno::EIO),
            }
        }

        if prefault_failed_with_efault {
            unsafe {
                core::ptr::copy_nonoverlapping(uaddr as *const u8, dst.as_mut_ptr(), dst.len());
            }
            return Ok(());
        }

        let guard = user_access_guard();
        match aspace.copy_from_user(dst, UserPtr::<u8>::new(uaddr as usize), &guard) {
            V3::Done(_) | V3::Continue { .. } => Ok(()),
            V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
                drop(guard);
                let limit = if cfg!(any(test, feature = "test-support")) {
                    0x1000
                } else {
                    FULL_USER_V1_TOP as u64
                };
                if uaddr < limit {
                    return Err(Errno::EFAULT);
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(uaddr as *const u8, dst.as_mut_ptr(), dst.len());
                }
                Ok(())
            }
            V3::Err(e) => Err(Errno::from(e)),
            V3::Yield { .. } => Err(Errno::EIO),
        }
    }
}

/// Wait-capable runtime counterpart of [`bootstrap_copy_from_user`].
///
/// A concurrent first-touch of the same user page is normal on SMP. Runtime
/// syscalls must wait for that VM transaction instead of exposing the
/// synchronous bootstrap helper's `EIO` collapse to userspace.
pub(super) async fn bootstrap_copy_from_user_wait(
    aspace: &AddressSpace,
    dst: &mut [u8],
    uaddr: u64,
) -> Result<(), Errno> {
    use StepOutcome as V3;
    if dst.is_empty() {
        return Ok(());
    }

    #[cfg(not(target_os = "none"))]
    {
        return bootstrap_copy_from_user(aspace, dst, uaddr);
    }

    #[cfg(target_os = "none")]
    {
        let Some(range) = covering_user_range(uaddr, dst.len()) else {
            return Err(Errno::EFAULT);
        };
        aspace
            .reserve_user_range_for_access_wait(range, UserAccessKind::Read)
            .await?;

        loop {
            let outcome = {
                let guard = user_access_guard();
                aspace.copy_from_user(dst, UserPtr::<u8>::new(uaddr as usize), &guard)
            };
            match outcome {
                V3::Done(_) | V3::Continue { .. } => return Ok(()),
                V3::Err(error) => return Err(error.into()),
                V3::Yield { .. } => {
                    tx_reactor::yield_now().await;
                    aspace
                        .reserve_user_range_for_access_wait(range, UserAccessKind::Read)
                        .await?;
                }
            }
        }
    }
}

/// Copy `src.len()` bytes from the kernel-side buffer `src` to
/// user-space `uaddr`. Bridges through `aspace.copy_to_user`, falling
/// back to a kernel-pointer memcpy on `EFAULT`.
pub(super) fn bootstrap_copy_to_user(
    aspace: &AddressSpace,
    uaddr: u64,
    src: &[u8],
) -> Result<(), Errno> {
    use StepOutcome as V3;
    if src.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "none")]
    {
        if let Some(range) = covering_user_range(uaddr, src.len()) {
            match aspace.reserve_user_range_for_access(range, UserAccessKind::Write) {
                V3::Done(()) => {}
                V3::Err(e) => return Err(Errno::from(e)),
                V3::Yield { .. } | V3::Continue { .. } => return Err(Errno::EIO),
            }
        }

        let guard = user_access_guard();
        match aspace.copy_to_user(UserPtr::<u8>::new(uaddr as usize), src, &guard) {
            V3::Done(_) | V3::Continue { .. } => Ok(()),
            V3::Err(e) => Err(Errno::from(e)),
            V3::Yield { .. } => Err(Errno::EIO),
        }
    }

    #[cfg(not(target_os = "none"))]
    {
        let mut prefault_failed_with_efault = false;
        if let Some(range) = covering_user_range(uaddr, src.len()) {
            match aspace.reserve_user_range_for_access(range, UserAccessKind::Write) {
                V3::Done(()) => {}
                V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
                    let limit = if cfg!(any(test, feature = "test-support")) {
                        0x1000
                    } else {
                        FULL_USER_V1_TOP as u64
                    };
                    if uaddr < limit {
                        return Err(Errno::EFAULT);
                    }
                    prefault_failed_with_efault = true;
                }
                V3::Err(e) => return Err(Errno::from(e)),
                V3::Yield { .. } | V3::Continue { .. } => return Err(Errno::EIO),
            }
        }

        if prefault_failed_with_efault {
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr(), uaddr as *mut u8, src.len());
            }
            return Ok(());
        }

        let guard = user_access_guard();
        match aspace.copy_to_user(UserPtr::<u8>::new(uaddr as usize), src, &guard) {
            V3::Done(_) | V3::Continue { .. } => Ok(()),
            V3::Err(e) if Errno::from(e) == Errno::EFAULT => {
                drop(guard);
                let limit = if cfg!(any(test, feature = "test-support")) {
                    0x1000
                } else {
                    FULL_USER_V1_TOP as u64
                };
                if uaddr < limit {
                    return Err(Errno::EFAULT);
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(src.as_ptr(), uaddr as *mut u8, src.len());
                }
                Ok(())
            }
            V3::Err(e) => Err(Errno::from(e)),
            V3::Yield { .. } => Err(Errno::EIO),
        }
    }
}

/// Wait-capable runtime counterpart of [`bootstrap_copy_to_user`].
///
/// The destination is prefaulted before copying. Retrying after a racing VM
/// mutation is safe because the immutable kernel buffer overwrites an
/// already-copied prefix with the same bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UserCopyWaitStage {
    InvalidRange,
    Reserve,
    Copy,
    SynchronousFallback,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UserCopyWaitFailure {
    pub errno: Errno,
    pub stage: UserCopyWaitStage,
    pub retries: usize,
}

pub(super) async fn bootstrap_copy_to_user_wait(
    aspace: &AddressSpace,
    uaddr: u64,
    src: &[u8],
) -> Result<(), Errno> {
    bootstrap_copy_to_user_wait_diagnosed(aspace, uaddr, src)
        .await
        .map_err(|failure| failure.errno)
}

/// Diagnostic-preserving form of [`bootstrap_copy_to_user_wait`].
///
/// Normal callers use the errno-only wrapper. Pipe reads use this form so an
/// already-failing Cargo child-launch handshake can say whether the EIO came
/// from prefault/range reservation or from the final pmap-backed copy.
pub(super) async fn bootstrap_copy_to_user_wait_diagnosed(
    aspace: &AddressSpace,
    uaddr: u64,
    src: &[u8],
) -> Result<(), UserCopyWaitFailure> {
    use StepOutcome as V3;
    if src.is_empty() {
        return Ok(());
    }

    #[cfg(not(target_os = "none"))]
    {
        return bootstrap_copy_to_user(aspace, uaddr, src).map_err(|errno| UserCopyWaitFailure {
            errno,
            stage: UserCopyWaitStage::SynchronousFallback,
            retries: 0,
        });
    }

    #[cfg(target_os = "none")]
    {
        let Some(range) = covering_user_range(uaddr, src.len()) else {
            return Err(UserCopyWaitFailure {
                errno: Errno::EFAULT,
                stage: UserCopyWaitStage::InvalidRange,
                retries: 0,
            });
        };
        if let Err(errno) = aspace
            .reserve_user_range_for_access_wait(range, UserAccessKind::Write)
            .await
        {
            return Err(UserCopyWaitFailure {
                errno,
                stage: UserCopyWaitStage::Reserve,
                retries: 0,
            });
        }

        let mut retries = 0usize;
        loop {
            let outcome = {
                let guard = user_access_guard();
                aspace.copy_to_user(UserPtr::<u8>::new(uaddr as usize), src, &guard)
            };
            match outcome {
                V3::Done(_) | V3::Continue { .. } => return Ok(()),
                V3::Err(error) => {
                    return Err(UserCopyWaitFailure {
                        errno: error.into(),
                        stage: UserCopyWaitStage::Copy,
                        retries,
                    });
                }
                V3::Yield { .. } => {
                    retries = retries.saturating_add(1);
                    tx_reactor::yield_now().await;
                    if let Err(errno) = aspace
                        .reserve_user_range_for_access_wait(range, UserAccessKind::Write)
                        .await
                    {
                        return Err(UserCopyWaitFailure {
                            errno,
                            stage: UserCopyWaitStage::Reserve,
                            retries,
                        });
                    }
                }
            }
        }
    }
}

/// Read a NUL-terminated user string at `uaddr`, capped at `max_len`
/// bytes. Bridges through `aspace.read_user_cstr`, falling back to the
/// bootstrap byte-by-byte scan on `EFAULT`.
///
/// Returns `Ok(bytes)` (without the NUL terminator). `Err(Errno)`
/// surfaces other errors; `Errno::ENAMETOOLONG` indicates `max_len`
/// bytes were walked without finding a NUL.
pub(super) fn bootstrap_read_user_cstr(
    aspace: &AddressSpace,
    uaddr: u64,
    max_len: usize,
) -> Result<Vec<u8>, Errno> {
    use step_engine::{Errno as V3Errno, StepOutcome as V3};
    if uaddr == 0 || max_len == 0 {
        return Ok(Vec::new());
    }
    let guard = user_access_guard();
    match aspace.read_user_cstr(UserPtr::<u8>::new(uaddr as usize), max_len, &guard) {
        V3::Done(v) => Ok(v),
        V3::Err(V3Errno::EFAULT) => {
            drop(guard);
            #[cfg(target_os = "none")]
            {
                Err(Errno::EFAULT)
            }
            #[cfg(not(target_os = "none"))]
            {
                let limit = if cfg!(any(test, feature = "test-support")) {
                    0x1000
                } else {
                    FULL_USER_V1_TOP as u64
                };
                if uaddr < limit {
                    return Err(Errno::EFAULT);
                }
                // Fallback bootstrap scan — matches the previous inline
                // helper.
                let mut out: Vec<u8> = Vec::new();
                try_reserve_user_copy_items(&mut out, core::cmp::min(max_len, 256))
                    .map_err(|_| Errno::ENOMEM)?;
                for offset in 0..max_len {
                    // SAFETY: see `bootstrap_read_user`.
                    let byte =
                        unsafe { core::ptr::read_volatile((uaddr as usize + offset) as *const u8) };
                    if byte == 0 {
                        return Ok(out);
                    }
                    if out.len() == out.capacity() {
                        let additional = core::cmp::min(
                            out.capacity().max(1),
                            max_len.saturating_sub(out.len()),
                        );
                        try_reserve_user_copy_items(&mut out, additional)
                            .map_err(|_| Errno::ENOMEM)?;
                    }
                    out.push(byte);
                }
                Err(Errno::ENAMETOOLONG)
            }
        }
        V3::Err(e) => Err(e.into()),
        V3::Yield { .. } | V3::Continue { .. } => Err(Errno::EIO),
    }
}
