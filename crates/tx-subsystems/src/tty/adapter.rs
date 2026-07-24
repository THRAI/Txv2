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
//!   readers / writers, and step_ingest posts v3 mailbox wakeups).

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "bus"],
    reason = "expose substrate step engine (StepOp/StepOutcome and ten step_* file types), zone role types, EBR guard, and bus primitives (RawPort/RawQueue) used by TtyIdentity / TtyPayload across the tty subsystem"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::bus::{RawPort, RawQueue};
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        drive_oneshot, ByteProgress, Errno, InterestMask, NoProgress, OneShotStepOp, ScriptCtx,
        StepOp, StepOutcome, StepProgress, SubjectIdentity, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, Dead, Entity, IdentRef,
        OperationalCapExt, PayloadCap, PayloadPolicy, Weak, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::AtomicSlot;
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake"],
    reason = "wrap WaitSource registration for tty reader/writer wakeup paths"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::wake::{MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource};

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

    /// Notify the v3 `WaitSource` using a caller-provided mailbox post route.
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
