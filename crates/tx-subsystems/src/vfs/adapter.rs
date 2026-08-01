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
    reason = "expose substrate step engine outcome types (StepOutcome, ByteProgress, NoProgress, Errno), zone role types (Cap, PayloadCap, Weak, Zone, ZoneAllocated), and EBR guard used by vfs trait surface and walker step ops"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{borrow_current_guard, guard, Guard};
    pub use tx_substrate::step::{
        drive_oneshot, ByteProgress, Deadline, Errno, InterestMask, NoProgress, OneShotStepOp,
        ProcessIdentity, ResumeOutcome, ScriptCtx, StepOp, StepOutcome, StepProgress,
        SubjectIdentity, TimerId, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, Dead, Entity, IdentRef, PayloadCap,
        Weak, Zone, ZoneAllocated, ZoneError,
    };
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step"],
    reason = "wrap WaitSource registration and v3 mailbox notify for vfs RNode open-file wakeup paths"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::wake::{
        MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitEndpoint, WaitSource,
    };

    /// Delegates to `tx_substrate::wake::new_source`. Also registers
    /// the source in the global registry so the driver can look it up
    /// by [`WaitSourceId`] during yield resolution.
    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    /// Notify the v3 `WaitSource` through a caller-provided mailbox post route.
    ///
    /// Scheduler-context callers inject the owner-aware reactor route here;
    /// no-context callers pass direct mailbox posting through the same helper.
    pub fn notify_v3_source_with_post<F>(source: &Arc<WaitSource>, mask_bits: u64, mut post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        source.notify_with_owner_post(
            tx_substrate::step::InterestMask::new(mask_bits),
            MailboxSchedulerHint::Normal,
            |mailbox, event, hint| post(mailbox, event, hint),
        );
    }
}
