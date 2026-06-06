//! Substrate / reactor adapter for timerfd.
//!
//! Two `#[platform_adapter]`-marked modules: step_engine (zone
//! allocation, step outcomes, WaitSource) and wait_routing (Channel
//! for legacy coexistence).  Mirrors the pipe / signalfd / eventfd
//! adapters.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "wake"],
    reason = "expose substrate step engine outcome/error types, zone allocation, EBR guard, and WaitSource for timerfd step ops"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{borrow_current_guard, guard, Guard};
    pub use tx_substrate::step::{
        drive_oneshot, ByteProgress, Errno as V3Errno, InterestMask, NoProgress, OneShotStepOp,
        ProcessIdentity, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
        WaitSourceId, YieldShape,
    };
    pub use tx_substrate::wake::WaitSource;
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, CoLocatedEntity, Dead, Entity, IdentRef,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, Weak, Zone,
        ZoneAllocated, ZoneError,
    };

    pub type ByteOutcome = StepOutcome<usize, ByteProgress>;

    pub fn done_bytes(n: usize) -> ByteOutcome {
        StepOutcome::done(n)
    }

    pub fn eagain() -> ByteOutcome {
        StepOutcome::err(V3Errno::EAGAIN)
    }

    pub fn einval() -> ByteOutcome {
        StepOutcome::err(V3Errno::EINVAL)
    }

    pub fn yield_until_readable(wait_source_id: u64, mask: u64) -> ByteOutcome {
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, wait_source_id, mask)
    }

    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        tx_substrate::zone::register_zone_for::<T>().map(|_| ())
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake"],
    reason = "wrap WaitSource registration and v3 mailbox notify for timerfd"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as timerfd legacy wakeup verbs (D2 coexistence)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::WaitSource;

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) -> usize {
        tx_reactor::wait::fire_legacy(channel, mask_bits)
    }

    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        tx_substrate::wake::notify(source, mask_bits)
    }
}
