//! Substrate / reactor adapter for eventfd.
//!
//! Two `#[platform_adapter]`-marked modules: step_engine (zone
//! allocation, step outcomes, WaitSource) and wait_routing (Channel
//! for legacy coexistence).  Mirrors the pipe / signalfd adapters.

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
        ByteProgress, Errno as V3Errno, InterestMask, NoProgress, OneShotStepOp,
        ProcessIdentity, ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity,
        WaitSourceId, YieldShape, drive_oneshot,
    };
    pub use tx_substrate::wake::WaitSource;
    pub use tx_substrate::zone::{
        sign, sign_for, reserve_for,
        Cap, PayloadCap, Weak, IdentRef,
        Dead, ZoneError,
        Entity, CoLocatedEntity, OperationalCapExt, OperationalRefExt, PayloadBinding,
        Zone, ZoneAllocated,
        IdentitySlot, IsPayloadPolicy, CapProducingPolicy, ObserverNodePolicy, PayloadPolicy, RetainedEntityPolicy, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;

    pub type ByteOutcome = StepOutcome<usize, ByteProgress>;

    pub fn done_bytes(n: usize) -> ByteOutcome {
        StepOutcome::done(n)
    }

    pub fn eagain() -> ByteOutcome {
        StepOutcome::err(V3Errno::EAGAIN)
    }

    /// `write` would overflow the counter → `EAGAIN` (non-blocking).
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
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as eventfd legacy wakeup verbs (D2 coexistence)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::WaitSource;

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        tx_substrate::wake::new_source(source_id)
    }

    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) -> usize {
        tx_reactor::wait::fire_legacy(channel, mask_bits)
    }

    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        tx_substrate::wake::notify(source, mask_bits)
    }
}
