//! Reactor priority-donation seam.
//!
//! Futex PI lives below `tx-kernel`, while the boot reactor and its
//! scheduler live in `tx-kernel`. This module keeps that dependency
//! direction clean by exposing a small function-pointer seam over
//! reactor task keys.

use core::sync::atomic::{AtomicPtr, Ordering};

pub use tx_reactor::{PiLockToken, PriorityBoostError, PriorityBoostToken, PriorityKey, TaskKey};

pub type DonatePriorityFn = fn(
    owner: TaskKey,
    donor: TaskKey,
    rt_priority: u8,
) -> Result<PriorityBoostToken, PriorityBoostError>;
pub type DropPriorityDonationFn = fn(token: PriorityBoostToken) -> Result<(), PriorityBoostError>;
pub type EffectiveRtPriorityFn = fn(task: TaskKey) -> Result<u8, PriorityBoostError>;
pub type UpsertPiWaiterFn = fn(
    owner: TaskKey,
    lock: PiLockToken,
    waiter: TaskKey,
    priority: PriorityKey,
) -> Result<(), PriorityBoostError>;
pub type RemovePiWaiterFn = fn(owner: TaskKey, lock: PiLockToken) -> Result<(), PriorityBoostError>;
pub type EffectivePriorityKeyFn = fn(task: TaskKey) -> Result<PriorityKey, PriorityBoostError>;

static DONATE_PRIORITY_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static DROP_PRIORITY_DONATION_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static EFFECTIVE_RT_PRIORITY_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static UPSERT_PI_WAITER_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static REMOVE_PI_WAITER_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static EFFECTIVE_PRIORITY_KEY_FN: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn install_priority_donation(
    donate: DonatePriorityFn,
    drop: DropPriorityDonationFn,
    effective: EffectiveRtPriorityFn,
    upsert_pi_waiter: UpsertPiWaiterFn,
    remove_pi_waiter: RemovePiWaiterFn,
    effective_priority_key: EffectivePriorityKeyFn,
) {
    DONATE_PRIORITY_FN.store(donate as *mut (), Ordering::Release);
    DROP_PRIORITY_DONATION_FN.store(drop as *mut (), Ordering::Release);
    EFFECTIVE_RT_PRIORITY_FN.store(effective as *mut (), Ordering::Release);
    UPSERT_PI_WAITER_FN.store(upsert_pi_waiter as *mut (), Ordering::Release);
    REMOVE_PI_WAITER_FN.store(remove_pi_waiter as *mut (), Ordering::Release);
    EFFECTIVE_PRIORITY_KEY_FN.store(effective_priority_key as *mut (), Ordering::Release);
}

#[cfg(any(test, feature = "test-support"))]
pub fn install_priority_donation_for_test(
    donate: DonatePriorityFn,
    drop: DropPriorityDonationFn,
    effective: EffectiveRtPriorityFn,
    upsert_pi_waiter: UpsertPiWaiterFn,
    remove_pi_waiter: RemovePiWaiterFn,
    effective_priority_key: EffectivePriorityKeyFn,
) {
    install_priority_donation(
        donate,
        drop,
        effective,
        upsert_pi_waiter,
        remove_pi_waiter,
        effective_priority_key,
    );
}

pub fn donate_priority(
    owner: TaskKey,
    donor: TaskKey,
    rt_priority: u8,
) -> Result<PriorityBoostToken, PriorityBoostError> {
    let raw = DONATE_PRIORITY_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(PriorityBoostError::UnknownTask);
    }
    // SAFETY: the only writer stores a `DonatePriorityFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), DonatePriorityFn>(raw) };
    f(owner, donor, rt_priority)
}

pub fn drop_priority_donation(token: PriorityBoostToken) -> Result<(), PriorityBoostError> {
    let raw = DROP_PRIORITY_DONATION_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(PriorityBoostError::UnknownTask);
    }
    // SAFETY: the only writer stores a `DropPriorityDonationFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), DropPriorityDonationFn>(raw) };
    f(token)
}

pub fn task_effective_rt_priority(task: TaskKey) -> Result<u8, PriorityBoostError> {
    let raw = EFFECTIVE_RT_PRIORITY_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(PriorityBoostError::UnknownTask);
    }
    // SAFETY: the only writer stores an `EffectiveRtPriorityFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), EffectiveRtPriorityFn>(raw) };
    f(task)
}

pub fn upsert_pi_waiter(
    owner: TaskKey,
    lock: PiLockToken,
    waiter: TaskKey,
    priority: PriorityKey,
) -> Result<(), PriorityBoostError> {
    let raw = UPSERT_PI_WAITER_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(PriorityBoostError::UnknownTask);
    }
    // SAFETY: the only writer stores an `UpsertPiWaiterFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), UpsertPiWaiterFn>(raw) };
    f(owner, lock, waiter, priority)
}

pub fn remove_pi_waiter(owner: TaskKey, lock: PiLockToken) -> Result<(), PriorityBoostError> {
    let raw = REMOVE_PI_WAITER_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(PriorityBoostError::UnknownTask);
    }
    // SAFETY: the only writer stores a `RemovePiWaiterFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), RemovePiWaiterFn>(raw) };
    f(owner, lock)
}

pub fn task_effective_priority_key(task: TaskKey) -> Result<PriorityKey, PriorityBoostError> {
    let raw = EFFECTIVE_PRIORITY_KEY_FN.load(Ordering::Acquire);
    if raw.is_null() {
        return Err(PriorityBoostError::UnknownTask);
    }
    // SAFETY: the only writer stores an `EffectivePriorityKeyFn` cast to `*mut ()`.
    let f = unsafe { core::mem::transmute::<*mut (), EffectivePriorityKeyFn>(raw) };
    f(task)
}

pub fn donate_priority_fn() -> Option<DonatePriorityFn> {
    let raw = DONATE_PRIORITY_FN.load(Ordering::Acquire);
    if raw.is_null() {
        None
    } else {
        // SAFETY: inverse of the `install_priority_donation` cast.
        Some(unsafe { core::mem::transmute::<*mut (), DonatePriorityFn>(raw) })
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn reset_for_test() {
    DONATE_PRIORITY_FN.store(core::ptr::null_mut(), Ordering::Release);
    DROP_PRIORITY_DONATION_FN.store(core::ptr::null_mut(), Ordering::Release);
    EFFECTIVE_RT_PRIORITY_FN.store(core::ptr::null_mut(), Ordering::Release);
    UPSERT_PI_WAITER_FN.store(core::ptr::null_mut(), Ordering::Release);
    REMOVE_PI_WAITER_FN.store(core::ptr::null_mut(), Ordering::Release);
    EFFECTIVE_PRIORITY_KEY_FN.store(core::ptr::null_mut(), Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn donate_err(
        _owner: TaskKey,
        _donor: TaskKey,
        _rt_priority: u8,
    ) -> Result<PriorityBoostToken, PriorityBoostError> {
        Err(PriorityBoostError::InvalidPriority)
    }

    fn drop_err(_token: PriorityBoostToken) -> Result<(), PriorityBoostError> {
        Err(PriorityBoostError::UnknownToken)
    }

    fn effective_err(_task: TaskKey) -> Result<u8, PriorityBoostError> {
        Err(PriorityBoostError::UnknownTask)
    }

    fn upsert_err(
        _owner: TaskKey,
        _lock: PiLockToken,
        _waiter: TaskKey,
        _priority: PriorityKey,
    ) -> Result<(), PriorityBoostError> {
        Err(PriorityBoostError::InvalidPriority)
    }

    fn remove_err(_owner: TaskKey, _lock: PiLockToken) -> Result<(), PriorityBoostError> {
        Err(PriorityBoostError::UnknownToken)
    }

    fn priority_key_err(_task: TaskKey) -> Result<PriorityKey, PriorityBoostError> {
        Err(PriorityBoostError::UnknownTask)
    }

    #[test]
    fn priority_donation_seam_installs_function_pointer() {
        reset_for_test();
        assert!(donate_priority_fn().is_none());
        install_priority_donation(
            donate_err,
            drop_err,
            effective_err,
            upsert_err,
            remove_err,
            priority_key_err,
        );
        let installed = donate_priority_fn().expect("donation hook installed");
        assert_eq!(
            installed as *const () as usize,
            donate_err as *const () as usize
        );
        reset_for_test();
    }
}
