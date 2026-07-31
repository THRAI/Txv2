//! Substrate / reactor adapter for eventfd.
//!
//! Two `#[platform_adapter]`-marked modules: step_engine (zone allocation,
//! step outcomes, WaitSource) and wait_routing (mailbox notify helpers).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "wake"],
    reason = "expose substrate step engine outcome/error types, zone allocation, EBR guard, and WaitSource for eventfd read/write step ops"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        drive_oneshot, ByteProgress, Errno as V3Errno, InterestMask, NoProgress, OneShotStepOp,
        ProcessIdentity, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
        WaitSourceId, YieldShape,
    };
    pub use tx_substrate::wake::WaitSource;
    pub use tx_substrate::zone::{sign, Cap, Zone, ZoneAllocated, ZoneError};
    pub type ByteOutcome = StepOutcome<usize, ByteProgress>;

    pub fn done_bytes(n: usize) -> ByteOutcome {
        StepOutcome::done(n)
    }

    pub fn eagain() -> ByteOutcome {
        StepOutcome::err(V3Errno::EAGAIN)
    }

    pub fn eagain_no_progress() -> StepOutcome<(), NoProgress> {
        StepOutcome::err(V3Errno::EAGAIN)
    }

    pub fn yield_until_readable(wait_source_id: u64, mask: u64) -> ByteOutcome {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, wait_source_id, mask)
    }

    pub fn yield_until_writable(wait_source_id: u64, mask: u64) -> StepOutcome<(), NoProgress> {
        StepOutcome::yield_on_wait_source(NoProgress, wait_source_id, mask)
    }

    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        tx_substrate::zone::register_zone_for::<T>().map(|_| ())
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake"],
    reason = "wrap WaitSource registration and v3 mailbox notify for eventfd"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::wake::{MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource};

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    pub fn notify_source_with_post<F>(source: &Arc<WaitSource>, mask_bits: u64, mut post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        source.notify_with_owner_post(
            tx_substrate::step::InterestMask::new(mask_bits),
            MailboxSchedulerHint::Normal,
            |mailbox, event, _hint| post(mailbox, event),
        );
    }
}
