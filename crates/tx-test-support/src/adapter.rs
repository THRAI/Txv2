//! Substrate adapter for tx-test-support: routes the substrate
//! test-bootstrap verbs (`init_host_for_test_once`,
//! `drain_with_budget`) through `#[platform_adapter]` so workspace
//! test code doesn't have direct `tx_substrate::*` references.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["testing", "epoch"],
    reason = "wrap substrate test-bootstrap (init_host_for_test_once) and EBR drain (drain_with_budget) as named test-support verbs so workspace test files reach platform state through a sanctioned adapter rather than direct tx_substrate imports"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::DrainStats as EpochSummary;

    /// Bootstrap the substrate host state (zones, EBR thread state,
    /// page allocator). Idempotent — safe to call from every test
    /// `setup()` helper.
    pub fn init_host() {
        tx_substrate::testing::init_host_for_test_once();
    }

    /// Drain EBR to quiescence — loops `drain_with_budget(usize::MAX)`
    /// until two consecutive drains reclaim zero nodes. Tests that
    /// observe lifecycle bookkeeping (drop counts, weak upgrades)
    /// invoke this after deferred-drop work to flush retirement
    /// queues.
    pub fn drain_to_quiescence() {
        let mut quiet = 0u32;
        while quiet < 2 {
            let stats = tx_substrate::epoch::drain_with_budget(usize::MAX);
            if stats.bag_reclaimed == 0 && stats.publication_dropped == 0 {
                quiet += 1;
            } else {
                quiet = 0;
            }
        }
    }

    /// Single-shot drain returning the stats. For tests that want
    /// finer-grained control than `drain_to_quiescence`.
    pub fn drain_once_unbounded() -> EpochSummary {
        tx_substrate::epoch::drain_with_budget(usize::MAX)
    }
}
