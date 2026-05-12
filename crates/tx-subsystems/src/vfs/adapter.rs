//! Substrate / reactor adapter for vfs.
//!
//! VFS is another multi-file subsystem; sibling files
//! (`structure.rs`, `execution.rs`, `walker.rs`, `checks.rs`) all
//! consume this single adapter. Two domains:
//!
//! * `step_engine` — substrate. Bundles step-v3 outcome types and
//!   zone role types (Cap, Weak, Zone, ZoneAllocated, ZoneError),
//!   plus EBR `Guard` / `guard()` used by structure.rs's
//!   payload-walk helpers. Provides `sign_zone_for` plus
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
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        ByteProgress, Errno, InterestMask, NoProgress, ProcessIdentity, ScriptCtx, StepOp,
        StepOutcome, StepProgress, SubjectIdentity, WaitSourceId,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign_for, Cap, PayloadCap, Weak, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::SpinMutex;

    /// Reserve + sign in one step: mint a `Cap<T>` from `T`'s zone.
    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
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
    use tx_substrate::step::{InterestMask, WaitSourceId};

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::{MailboxEvent, TaskMailbox, WaitGeneration, WaitRegistrationGuard, WaitSource};

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        Arc::new(WaitSource::new(WaitSourceId::new(source_id)))
    }

    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) -> usize {
        channel.fire(Mask::from_bits(mask_bits))
    }

    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        source.notify(InterestMask::new(mask_bits));
    }
}
