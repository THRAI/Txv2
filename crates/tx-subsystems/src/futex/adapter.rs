//! Substrate / reactor adapter for futex.
//!
//! Same two-domain shape as the pipe pilot:
//!
//! * `step_engine` — substrate. Re-exports `step_v3` types futex's
//!   step ops use and provides the futex-side wait verb
//!   `yield_until_wake(source_id, mask)` that wraps the explicit
//!   `Yield { progress: NoProgress, shape: OnWaitSource { source,
//!   interests } }` enum constructor in named-domain form.
//!
//! * `wait_routing` — stacked substrate + reactor. Wraps WaitSource
//!   registration (`new_wait_source`) and the legacy/v3 wakeup
//!   primitives (`fire_legacy_channel`, `notify_v3_source`) used
//!   by `step_futex_wake` when it fires both D2 paths.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone"],
    reason = "wrap futex step outcomes (einval, eagain, yield-until-wake, done(n)) as named verbs over the substrate step engine"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        Errno, InterestMask, NoProgress, OneShotStepOp, ProcessIdentity, ScriptCtx, StepOp,
        StepOutcome, StepProgress, SubjectIdentity, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;

    /// `futex(uaddr, FUTEX_WAIT, val, ...)` matched the value: park on
    /// the bucket's wait source. Wraps `StepOutcome::Yield { progress
    /// = NoProgress, shape = OnWaitSource { ... } }` in a futex-side
    /// verb so the call site reads as the futex contract.
    pub fn yield_until_wake(source_id: u64, mask: u64) -> StepOutcome<(), NoProgress> {
        StepOutcome::Yield {
            progress: NoProgress,
            shape: YieldShape::OnWaitSource {
                source: WaitSourceId::new(source_id),
                interests: InterestMask::new(mask),
            },
        }
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step"],
    reason = "wrap WaitSource registration and v3 mailbox notify in futex-side bucket wakeup verbs (D2 coexistence path)"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as futex-side legacy bucket wakeup verbs (PR-3D-2 D2 coexistence path)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::{
        MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
    };

    /// Mint a `WaitSource` for one futex bucket, keyed by the
    /// bucket's `source_id` so the legacy `Channel` resolver and the
    /// v3 mailbox path share the id namespace (PR-3D-2 / D2
    /// coexistence).
    ///
    /// Delegates to `tx_substrate::wake::new_source`.
    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    /// Fire the legacy `Channel` for one futex bucket — D2
    /// coexistence wake path.
    ///
    /// Delegates to `tx_reactor::wait::fire_legacy`.
    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) {
        tx_reactor::wait::fire_legacy(channel, mask_bits);
    }

    /// Notify the v3 `WaitSource` for one futex bucket — D2
    /// coexistence wake path (mailbox).
    ///
    /// Delegates to `tx_substrate::wake::notify`.
    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        tx_substrate::wake::notify(source, mask_bits)
    }
}
