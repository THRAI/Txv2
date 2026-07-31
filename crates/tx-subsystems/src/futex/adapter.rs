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
//! * `wait_routing` — substrate wake routing. Wraps WaitSource registration
//!   (`new_wait_source`) and mailbox wake publication used by
//!   `step_futex_wake`.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone"],
    reason = "wrap futex step outcomes (einval, eagain, yield-until-wake, done(n)) as named verbs over the substrate step engine"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        AbortReason, Errno, InterestMask, NoProgress, OneShotStepOp, ProcessIdentity,
        ResumeOutcome, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity, WaitSourceId,
        YieldShape,
    };
    pub use tx_substrate::zone::ZoneError;

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
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::wake::{
        MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitGeneration, WaitRegistrationGuard,
        WaitSource,
    };

    /// Mint a `WaitSource` for one futex bucket, keyed by the bucket's
    /// `source_id`.
    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    /// Remove a waiter-owned v3 `WaitSource` from the global registry.
    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    /// Notify the v3 `WaitSource` through a caller-provided mailbox post.
    ///
    /// This is the futex exact-waiter hook for syscall/reactor contexts that
    /// can publish `SourceFired` and immediately route the already-upgraded
    /// mailbox through owner-aware scheduler placement.
    pub fn notify_v3_source_limit_emit_with_post<F>(
        source: &Arc<WaitSource>,
        mask_bits: u64,
        limit: usize,
        hint: MailboxSchedulerHint,
        post: F,
    ) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        source.notify_limit_emit_with_owner_post(
            tx_substrate::step::InterestMask::new(mask_bits),
            limit,
            hint,
            post,
        )
    }
}
