//! Epoch-based reclamation substrate.
//!
//! This is the first executable EBR slice. It preserves the public shape the
//! zone layer needs (`guard`, delayed raw retirement, bounded drain), while the
//! internals intentionally stay single-hart friendly until the scheduler,
//! CpuLocal, and migration-pin machinery exist.

mod domain;
mod guard;
mod local;
mod retired;

pub use domain::{init_on_ap, init_on_bsp, try_drain, DrainStats, EpochError};
pub use guard::Guard;

#[doc(hidden)]
pub mod testing {
    pub use super::domain::RETIRED_NODE_POOL_CAPACITY;

    pub unsafe fn reset_for_test() {
        super::domain::reset_for_test();
    }

    pub fn init_for_test() {
        super::domain::init_for_test();
    }

    pub unsafe fn retire_raw_for_test(
        ptr: *mut u8,
        reclaim_fn: unsafe fn(*mut u8),
    ) -> Result<(), super::EpochError> {
        super::retire_raw(ptr, reclaim_fn)
    }
}

/// Enter an epoch.
///
/// While the returned guard is alive, raw observations created by zone/Weak
/// paths are protected from physical slot reuse.
pub fn guard() -> Guard<'static> {
    domain::guard()
}

/// Try to reclaim at most `budget` expired retired nodes.
pub fn drain_with_budget(budget: usize) -> DrainStats {
    try_drain(budget)
}

/// Schedule `ptr` for delayed reclamation through the EBR domain.
///
/// Used by the zone layer when retiring a slot, and by upper-layer
/// publication mechanisms (for example VM recipe publication) that swap an
/// `AtomicPtr` to a new owner and need to defer the old owner's free until
/// every reader past the swap has dropped its guard.
///
/// # Safety
///
/// `ptr` must name storage whose physical reuse must be delayed until all
/// active guards have quiesced. `reclaim_fn` must be valid for exactly one
/// call with `ptr` after the delay window.
pub unsafe fn retire_raw(ptr: *mut u8, reclaim_fn: unsafe fn(*mut u8)) -> Result<(), EpochError> {
    unsafe { domain::retire_raw(ptr, reclaim_fn) }
}
