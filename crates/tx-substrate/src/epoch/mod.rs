//! Epoch-based reclamation substrate.
//!
//! This executable EBR substrate provides guarded observation, three intrusive
//! per-CPU bags, bounded draining, and SMP-aware guard publication. Runtime
//! trigger wiring such as timer/idle/OOM hooks still belongs to higher layers.

use core::ptr;

mod bag;
mod domain;
mod guard;
mod local;

pub use domain::{
    cpu_summary, init_on_ap, init_on_bsp, offline_cpu, service_local_drain_request, summary,
    try_drain, CpuEpochSummary, DrainStats, EpochError, EpochSummary,
};
pub(crate) use domain::{LocalRetireGuard, MAX_EPOCH_CPUS};
pub use guard::Guard;

#[repr(C)]
pub(crate) struct RcuHead {
    pub(crate) next: *mut RcuHead,
    pub(crate) reclaim: unsafe fn(*mut RcuHead, &mut LocalRetireGuard),
}

impl RcuHead {
    pub(crate) const fn new(reclaim: unsafe fn(*mut RcuHead, &mut LocalRetireGuard)) -> Self {
        Self {
            next: ptr::null_mut(),
            reclaim,
        }
    }

    pub(crate) fn is_queued(&self) -> bool {
        !self.next.is_null()
    }
}

pub(crate) fn with_local_retire_guard<R>(
    action: impl FnOnce(&mut domain::LocalRetireGuard) -> R,
) -> Result<R, EpochError> {
    domain::with_local_retire_guard(action)
}

#[doc(hidden)]
pub mod testing {
    use core::ptr::NonNull;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum CpuMembershipForTest {
        Offline,
        Admitting,
        Online,
        Draining,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct AdvanceTestResult {
        pub advanced: bool,
        pub scan_attempts: usize,
        pub version_before: u64,
        pub version_after: u64,
    }

    #[repr(C)]
    pub struct IntrusiveTestNode {
        head: super::RcuHead,
        ptr: *mut u8,
        reclaim_fn: unsafe fn(*mut u8),
    }

    impl IntrusiveTestNode {
        pub const fn new(ptr: *mut u8, reclaim_fn: unsafe fn(*mut u8)) -> Self {
            Self {
                head: super::RcuHead::new(reclaim_intrusive_test_node),
                ptr,
                reclaim_fn,
            }
        }
    }

    unsafe fn reclaim_intrusive_test_node(
        head: *mut super::RcuHead,
        _guard: &mut super::LocalRetireGuard,
    ) {
        let node = head.cast::<IntrusiveTestNode>();
        unsafe {
            ((*node).reclaim_fn)((*node).ptr);
        }
    }

    pub unsafe fn reset_for_test() {
        super::domain::reset_for_test();
    }

    pub fn init_for_test() {
        super::domain::init_for_test();
    }

    pub fn with_local_retire_guard_for_test<R>(
        action: impl FnOnce() -> R,
    ) -> Result<R, super::EpochError> {
        super::domain::with_local_retire_guard_for_test(action)
    }

    pub fn local_retire_active_for_test() -> bool {
        super::domain::local_retire_active_for_test()
    }

    pub fn try_advance_with_membership_change_for_test(
        membership_change: impl FnOnce(),
    ) -> AdvanceTestResult {
        let result = super::domain::try_advance_with_membership_change_for_test(membership_change);
        AdvanceTestResult {
            advanced: result.advanced,
            scan_attempts: result.scan_attempts,
            version_before: result.version_before,
            version_after: result.version_after,
        }
    }

    pub fn drain_requested_for_test(cpu: tx_hal::CpuId) -> bool {
        super::domain::drain_requested_for_test(cpu)
    }

    pub fn cpu_membership_for_test(cpu: tx_hal::CpuId) -> CpuMembershipForTest {
        match super::domain::membership_for_test(cpu) {
            Some(super::local::CpuMembership::Admitting) => CpuMembershipForTest::Admitting,
            Some(super::local::CpuMembership::Online) => CpuMembershipForTest::Online,
            Some(super::local::CpuMembership::Draining) => CpuMembershipForTest::Draining,
            Some(super::local::CpuMembership::Offline) | None => CpuMembershipForTest::Offline,
        }
    }

    pub fn membership_version_for_test() -> u64 {
        super::domain::membership_version_for_test()
    }

    pub fn init_on_bsp_with_admission_hook_for_test<P>(
        admission_hook: impl FnOnce(),
    ) -> Result<(), super::EpochError>
    where
        P: tx_hal::PercpuIf + tx_hal::SmpIf + tx_hal::IrqIf,
    {
        super::domain::init_on_bsp_with_admission_hook_for_test::<P>(admission_hook)
    }

    pub unsafe fn retire_intrusive_for_test(
        node: &mut IntrusiveTestNode,
    ) -> Result<(), super::EpochError> {
        let head = NonNull::from(&mut node.head);
        unsafe { super::domain::retire_intrusive(head) }
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

/// Borrow the current CPU's active epoch guard without modifying epoch counters.
///
/// Returns `Some(guard)` when the current CPU already holds a guard
/// (`local_epoch != 0`).  The returned guard has a no-op Drop: it does not
/// call `local.leave()` or decrement `active_guards`.
///
/// Returns `None` if no guard is currently held; the caller should fall back
/// to `epoch::guard()`.
///
/// Use this when code must satisfy an `&Guard` API but is called from within
/// an existing epoch window and creating a nested guard would violate the
/// EBR no-nesting invariant.
pub fn borrow_current_guard() -> Option<Guard<'static>> {
    domain::borrow_guard()
}

/// Try to reclaim at most `budget` expired retired nodes.
pub fn drain_with_budget(budget: usize) -> DrainStats {
    try_drain(budget)
}
