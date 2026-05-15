//! Substrate / reactor adapter for vfs.
//!
//! VFS is another multi-file subsystem; sibling files
//! (`structure.rs`, `execution.rs`, `walker.rs`, `checks.rs`) all
//! consume this single adapter. Two domains:
//!
//! * `step_engine` — substrate. Bundles step-v3 outcome types and
//!   zone role types (Cap, Weak, Zone, ZoneAllocated, ZoneError),
//!   plus EBR `Guard` / `guard()` used by structure.rs's
//!   payload-walk helpers. Re-exports `zone::sign`; provides
//!   `vfs_err(errno)` (returns `StepOutcome::err(errno)` typed for
//!   the FsOps return shape).
//!
//! * `wait_routing` — stacked substrate + reactor. structure.rs
//!   exposes per-RNode wait sources (open-file notify); same verbs
//!   as pipe/futex/process.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine outcome types (StepOutcome, ByteProgress, NoProgress, Errno), zone role types (Cap, Weak, Zone, ZoneAllocated), and EBR guard used by vfs trait surface and walker step ops"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        ByteProgress, Errno, InterestMask, NoProgress, OneShotStepOp, ProcessIdentity,
        ScriptCtx, StepOp, StepOutcome, StepProgress, SubjectIdentity, WaitSourceId,
        drive_oneshot,
    };
    pub use tx_substrate::zone::{
        sign, sign_for, reserve_for, register_zone_for,
        Cap, PayloadCap, Weak, IdentRef,
        Dead, ZoneError,
        Entity, CoLocatedEntity, OperationalCapExt, OperationalRefExt, PayloadBinding,
        Zone, ZoneAllocated,
        IdentitySlot, IsPayloadPolicy, CapProducingPolicy, ObserverNodePolicy, PayloadPolicy, RetainedEntityPolicy, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step"],
    reason = "wrap WaitSource registration and v3 mailbox notify for vfs RNode open-file wakeup paths"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as vfs RNode legacy wakeup verbs (D2 coexistence)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::{
        MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource,
    };

    /// Delegates to `tx_substrate::wake::new_source`.
    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        tx_substrate::wake::new_source(source_id)
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
