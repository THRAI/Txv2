//! Epoch-based reclamation substrate.
//!
//! This executable EBR substrate preserves the public shape the zone layer
//! needs (`guard`, delayed raw retirement, bounded drain) and already supports
//! explicit BSP/AP initialization, per-CPU retired pools, and SMP-aware guard
//! publication. Runtime trigger wiring such as timer/idle/OOM hooks still
//! belongs to higher layers rather than this module.

mod domain;
mod guard;
mod local;
mod retired;

pub use domain::{
    cpu_summary, init_on_ap, init_on_bsp, summary, try_drain, CpuEpochSummary, DrainStats,
    EpochError, EpochSummary,
};
pub use guard::Guard;
pub use retired::RETIRED_NODE_POOL_CAPACITY;

#[doc(hidden)]
pub mod testing {
    pub use super::retired::RETIRED_NODE_POOL_CAPACITY;

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

/// Called by the future zone layer after it has installed the no-upgrade
/// barrier and transitioned a slot to Retiring.
///
/// # Safety
///
/// `ptr` must name storage whose physical reuse must be delayed until all
/// active guards have quiesced. `reclaim_fn` must be valid for exactly one
/// call with `ptr` after the delay window.
pub(crate) unsafe fn retire_raw(
    ptr: *mut u8,
    reclaim_fn: unsafe fn(*mut u8),
) -> Result<(), EpochError> {
    domain::retire_raw(ptr, reclaim_fn)
}
