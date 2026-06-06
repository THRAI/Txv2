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
    /// Canonical user copy fault (for example, `NULL`/bad pathname).
    Fault(Errno),
}

/// Outcome of `read_user_cstr_vec`. `TooBig` covers both
/// pointer-array overflow and aggregate-byte overflow; both surface
/// as `-E2BIG` per the Phase 6 plan.
pub(super) enum ReadVecError {
    TooBig,
}

fn user_access_guard() -> step_engine::Guard<'static> {
    step_engine::borrow_current_guard().unwrap_or_else(step_engine::guard)
}

/// Bounded copy of a NUL-terminated user string into a kernel-owned
/// `Vec<u8>` (NUL terminator stripped).
///
/// Normal pathname-bearing syscalls expect Linux semantics here:
/// `uaddr == 0` is `-EFAULT`, a missing NUL within `max_len` is
/// `-ENAMETOOLONG`, and other canonical user-copy failures preserve
/// their original errno.
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
    let mut out: Vec<Vec<u8>> = Vec::new();
    for slot in 0..max_slots {
        let slot_addr = uaddr.wrapping_add((slot * core::mem::size_of::<u64>()) as u64);
        let ptr = match bootstrap_read_user::<u64>(aspace, slot_addr) {
            Ok(p) => p,
            Err(_) => return Err(ReadVecError::TooBig),
        };
        if ptr == 0 {
            return Ok(out);
        }
        // Read the string at `ptr`, capped at the remaining byte
        // budget. We need at least one byte for the NUL terminator;
        // when `*byte_budget == 0` any non-empty string is `TooBig`.
        let cap = *byte_budget;
        let s = match read_user_cstr(aspace, ptr, cap) {
            Ok(s) => s,
            Err(ReadCStrError::TooLong | ReadCStrError::Fault(_)) => {
                return Err(ReadVecError::TooBig);
            }
        };
        // Account `s.len() + 1` for the implicit NUL byte we read but
        // did not store, matching Linux's `ARG_MAX` accounting.
        let charged = s.len().saturating_add(1);
        if charged > *byte_budget {
            return Err(ReadVecError::TooBig);
        }
        *byte_budget -= charged;
        out.push(s);
    }
    // Hit the slot cap without observing a NULL terminator — treat
    // as oversized argv per the plan.
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
                let mut out: Vec<u8> = Vec::with_capacity(core::cmp::min(max_len, 256));
                for offset in 0..max_len {
                    // SAFETY: see `bootstrap_read_user`.
                    let byte =
                        unsafe { core::ptr::read_volatile((uaddr as usize + offset) as *const u8) };
                    if byte == 0 {
                        return Ok(out);
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
