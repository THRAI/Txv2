//! Substrate / reactor adapter for tty.
//!
//! Tty is the largest single-subsystem phase. ~14 production files
//! across `tty/structure/`, `tty/execution/{register_hardware,
//! step_*}.rs`, `tty/checks/`, and `tty/project.rs` share this one
//! adapter. Two domains:
//!
//! * `step_engine` — substrate. Bundles step-v3 types used by the
//!   ten tty `step_*` files, zone role types (Cap, Weak, PayloadCap,
//!   Entity, Dead, ZoneAllocated, ZoneError, Zone), EBR Guard +
//!   guard(), bus primitives (RawPort, RawQueue) used by
//!   TtyIdentity's mailbox path, AtomicSlot + SpinMutex, plus the
//!   `sign` (re-exported from `zone::sign`).
//!
//! * `wait_routing` — stacked substrate + reactor. Same shape as
//!   pipe / process / vfs (TtyIdentity exposes wait sources for
//!   readers / writers, plus step_ingest uses reactor::wait::Mask).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "bus"],
    reason = "expose substrate step engine (StepOp/StepOutcome and ten step_* file types), zone role types, EBR guard, and bus primitives (RawPort/RawQueue) used by TtyIdentity / TtyPayload across the tty subsystem"
)]
pub mod step_engine {
    pub use tx_substrate::bus::{RawPort, RawQueue};
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        drive_oneshot, ByteProgress, Errno, InterestMask, NoProgress, OneShotStepOp, ScriptCtx,
        StepOp, StepOutcome, StepProgress, SubjectIdentity, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::{AtomicSlot, SpinMutex};
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake"],
    reason = "wrap WaitSource registration for tty reader/writer wakeup paths"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as tty legacy wakeup primitives (D2 coexistence)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::{
        MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
    };

    /// Delegates to `tx_substrate::wake::new_source`.
    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    /// Delegates to `tx_substrate::wake::unregister_source`.
    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    /// Delegates to `tx_reactor::wait::fire_legacy`.
    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) -> usize {
        tx_reactor::wait::fire_legacy(channel, mask_bits)
    }

    /// Delegates to `tx_substrate::wake::notify`.
    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        tx_substrate::wake::notify(source, mask_bits)
    }
}
