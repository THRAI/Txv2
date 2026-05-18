//! Flow-ID computation via SipHash-1-3.
//!
//! Per §9 of `08_OBSERVATION_HOST_v0.md`:
//!   flow_id = siphash13(task_id || wait_gen || flow_kind || boot_id)
//!
//! The per-boot key prevents adversarial collisions in shared traces.
//! Accidental 64-bit collisions are statistically negligible.
//!
//! OBS-6 note: Neither `WaitSourceNotify` nor `Resume` records are currently
//! emitted by the kernel (OBS-3b/OBS-4 not yet wired).  This module is
//! structurally complete and exercised via unit tests with synthetic input.

use std::hash::{Hash, Hasher};
use siphasher::sip::SipHasher13;

/// Flow kind discriminant — matches §9 of the host spec.
///
/// `AgentReply`, `TimerExpire`, and `AbortDelivery` are structurally complete
/// for OBS-6 but unused until OBS-3b/OBS-4 kernel wiring lands.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowKind {
    SourceWake    = 1,
    AgentReply    = 2,
    TimerExpire   = 3,
    AbortDelivery = 4,
}

/// Compute the Perfetto flow_id for a producer/consumer pair.
///
/// `boot_id` seeds the SipHash key (half-key + bit-flip of half-key pattern
/// from the spec: `key0 = boot_id, key1 = !boot_id`).
pub fn compute_flow_id(task_id: u32, wait_gen: u64, kind: FlowKind, boot_id: u64) -> u64 {
    let mut h = SipHasher13::new_with_keys(boot_id, !boot_id);
    task_id.hash(&mut h);
    wait_gen.hash(&mut h);
    (kind as u8).hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_inputs() {
        let id = compute_flow_id(42, 7, FlowKind::SourceWake, 0xdeadbeef_cafebabe);
        let id2 = compute_flow_id(42, 7, FlowKind::SourceWake, 0xdeadbeef_cafebabe);
        assert_eq!(id, id2);
    }

    #[test]
    fn different_kind_produces_different_id() {
        let a = compute_flow_id(1, 1, FlowKind::SourceWake,    0);
        let b = compute_flow_id(1, 1, FlowKind::AgentReply,    0);
        let c = compute_flow_id(1, 1, FlowKind::TimerExpire,   0);
        let d = compute_flow_id(1, 1, FlowKind::AbortDelivery, 0);
        // All four must be distinct.
        let set = std::collections::HashSet::from([a, b, c, d]);
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn different_task_id_produces_different_id() {
        let a = compute_flow_id(1, 1, FlowKind::SourceWake, 0);
        let b = compute_flow_id(2, 1, FlowKind::SourceWake, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn different_boot_id_produces_different_id() {
        let a = compute_flow_id(1, 1, FlowKind::SourceWake, 0);
        let b = compute_flow_id(1, 1, FlowKind::SourceWake, 1);
        assert_ne!(a, b);
    }

    /// Smoke-test uniqueness over a modest input space.
    #[test]
    fn no_collisions_in_small_corpus() {
        let mut ids = std::collections::HashSet::new();
        for task in 0u32..50 {
            for gen in 0u64..20 {
                for kind in [
                    FlowKind::SourceWake,
                    FlowKind::AgentReply,
                    FlowKind::TimerExpire,
                    FlowKind::AbortDelivery,
                ] {
                    let id = compute_flow_id(task, gen, kind, 0xabcd_1234_5678_ef01);
                    assert!(ids.insert(id), "collision at task={task} gen={gen} kind={kind:?}");
                }
            }
        }
    }
}
