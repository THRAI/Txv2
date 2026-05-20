//! Reactor affinity syscall seam.
//!
//! The boot reactor is owned by `tx-kernel`, while Linux syscall decoding
//! lives in `tx-shims`. This module keeps the dependency direction clean by
//! exposing a small function-pointer seam keyed by user-visible tids.

use core::sync::atomic::{AtomicPtr, Ordering};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReactorAffinityError {
    InvalidMask,
    NoSuchThread,
    NotInstalled,
}

pub type SetThreadAffinityFn = fn(tid: u32, affinity: u64) -> Result<(), ReactorAffinityError>;
pub type GetThreadAffinityFn = fn(tid: u32) -> Result<u64, ReactorAffinityError>;

static SET_THREAD_AFFINITY_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static GET_THREAD_AFFINITY_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn install_thread_affinity(set: SetThreadAffinityFn, get: GetThreadAffinityFn) {
    SET_THREAD_AFFINITY_FN.store(set as *mut (), Ordering::Release);
    GET_THREAD_AFFINITY_FN.store(get as *mut (), Ordering::Release);
}

pub fn set_thread_affinity(tid: u32, affinity: u64) -> Result<(), ReactorAffinityError> {
    let raw = SET_THREAD_AFFINITY_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(ReactorAffinityError::NotInstalled);
    }
    // SAFETY: the only writer stores a `SetThreadAffinityFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), SetThreadAffinityFn>(raw) };
    f(tid, affinity)
}

pub fn get_thread_affinity(tid: u32) -> Result<u64, ReactorAffinityError> {
    let raw = GET_THREAD_AFFINITY_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(ReactorAffinityError::NotInstalled);
    }
    // SAFETY: the only writer stores a `GetThreadAffinityFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), GetThreadAffinityFn>(raw) };
    f(tid)
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    SET_THREAD_AFFINITY_FN.store(core::ptr::null_mut(), Ordering::Release);
    GET_THREAD_AFFINITY_FN.store(core::ptr::null_mut(), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_ok(tid: u32, affinity: u64) -> Result<(), ReactorAffinityError> {
        if tid == 7 && affinity == 0b11 {
            Ok(())
        } else {
            Err(ReactorAffinityError::NoSuchThread)
        }
    }

    fn get_ok(tid: u32) -> Result<u64, ReactorAffinityError> {
        if tid == 7 {
            Ok(0b11)
        } else {
            Err(ReactorAffinityError::NoSuchThread)
        }
    }

    #[test]
    fn affinity_seam_round_trips_installed_functions() {
        reset_for_test();
        assert_eq!(
            set_thread_affinity(7, 0b11),
            Err(ReactorAffinityError::NotInstalled)
        );
        install_thread_affinity(set_ok, get_ok);
        assert_eq!(set_thread_affinity(7, 0b11), Ok(()));
        assert_eq!(get_thread_affinity(7), Ok(0b11));
        assert_eq!(
            get_thread_affinity(8),
            Err(ReactorAffinityError::NoSuchThread)
        );
        reset_for_test();
    }
}
