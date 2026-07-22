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
pub use retired::RETIRED_BAG_CAPACITY;

#[doc(hidden)]
pub mod testing {
    pub use super::retired::RETIRED_BAG_CAPACITY;

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
#[track_caller]
pub fn guard() -> Guard<'static> {
    domain::guard()
}

/// Enter a real nested guard when the current CPU is already epoch-pinned.
///
/// Returns `Some(guard)` when the current CPU already holds a guard. The
/// returned guard increments the CPU-local pin depth and decrements it on Drop.
/// It reuses the outermost guard's published epoch and does not count as a new
/// periodic-collection pinning.
///
/// Returns `None` if no guard is currently held; the caller should fall back
/// to `epoch::guard()`.
///
/// This compatibility spelling remains for callers that want to distinguish
/// "already guarded" from "create an outermost guard". New code may simply
/// call `epoch::guard()`, which also supports balanced nesting.
pub fn borrow_current_guard() -> Option<Guard<'static>> {
    domain::borrow_guard()
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
