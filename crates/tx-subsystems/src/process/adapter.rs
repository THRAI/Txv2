//! Substrate / reactor adapter for process.
//!
//! Process is the first multi-file subsystem migrated. Sibling files
//! (`structure.rs`, `execution.rs`, `exec_prep.rs`) all consume this
//! single adapter via `use super::adapter::*`. Two domains:
//!
//! * `step_engine` — substrate. Bundles step-v3 types (StepOp,
//!   StepOutcome, etc., used by the seven `*Op` impls in
//!   `execution.rs`), zone role types (Cap, PayloadCap, Weak,
//!   IdentRef, Entity, Dead, ZoneAllocated, ZoneError, Zone,
//!   OperationalCapExt) used by ProcessIdentity / ProcessPayload /
//!   ProcessGroup / Session, EBR `Guard` / `guard()`, the
//!   `RestrictionStackHandle` (`SubjectAuthority` plumbing), the
//!   `AtomicSlot` primitive (used for slot-style payload binding),
//!   and `SpinMutex`. Re-exports `zone::sign`.
//!
//! * `wait_routing` — stacked substrate + reactor. The exit-source
//!   path: each process exposes a `WaitSource` (substrate) that
//!   wakes parents blocked in `waitpid`, backed by a reactor
//!   `Channel`/`Mask` legacy coexistence pair. Same verbs as pipe
//!   and futex: `new_wait_source`, `fire_legacy_channel`,
//!   `notify_v3_source`.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "bus"],
    reason = "expose substrate step engine (StepOp/StepOutcome, RestrictionStackHandle, SubjectIdentity), EBR guard, zone role types (Cap/PayloadCap/Weak/IdentRef/Entity), bus primitives (RawPort/RawQueue for signal_port/exit_source wires), and lock primitives (SpinMutex/AtomicSlot) used by process identity, payload, group, session, and the seven fork/exit/wait/chdir/getcwd/setpgid/setsid step ops"
)]
pub mod step_engine {
    pub use tx_substrate::bus::{RawPort, RawQueue};
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step::{
        Errno, InterestMask, NoProgress, OneShotStepOp, RestrictionStackHandle, ScriptCtx,
        StepOp, StepOutcome, SubjectIdentity, WaitSourceId, drive_oneshot,
    };
    pub use tx_substrate::zone::{
        sign, sign_for, reserve_for, register_zone_for,
        Cap, PayloadCap, Weak, IdentRef,
        Dead, ZoneError,
        Entity, CoLocatedEntity, OperationalCapExt, OperationalRefExt, PayloadBinding,
        Zone, ZoneAllocated,
        IdentitySlot, IsPayloadPolicy, CapProducingPolicy, ObserverNodePolicy, PayloadPolicy, RetainedEntityPolicy, ZonePolicy,
    };
    pub use tx_substrate::{AtomicSlot, SpinMutex};
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step"],
    reason = "wrap process exit-source WaitSource registration and v3 mailbox notify"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as process exit-source legacy wake verbs (D2 coexistence)"
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
    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) {
        tx_reactor::wait::fire_legacy(channel, mask_bits);
    }

    /// Delegates to `tx_substrate::wake::notify`.
    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        tx_substrate::wake::notify(source, mask_bits)
    }
}