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
//!   and `SpinMutex`. Provides `sign_zone_for`.
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
    apis = ["step_v3", "zone", "epoch"],
    reason = "expose substrate step engine (StepOp/StepOutcome, RestrictionStackHandle, SubjectIdentity), EBR guard, zone role types (Cap/PayloadCap/Weak/IdentRef/Entity), and lock primitives (SpinMutex/AtomicSlot) used by process identity, payload, group, session, and the seven fork/exit/wait/chdir/getcwd/setpgid/setsid step ops"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step_v3::ProcessIdentity as PlaceholderProcessSubject;
    pub use tx_substrate::step_v3::{
        NoProgress, RestrictionStackHandle, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
        WaitSourceId,
    };
    pub use tx_substrate::zone::{
        Cap, Dead, Entity, IdentRef, OperationalCapExt, PayloadCap, Weak, Zone, ZoneAllocated,
        ZoneError,
    };
    pub use tx_substrate::{AtomicSlot, SpinMutex};

    /// Reserve + sign in one step: mint a `Cap<T>` from `T`'s zone.
    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step_v3"],
    reason = "wrap process exit-source WaitSource registration and v3 mailbox notify"
)]
#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as process exit-source legacy wake verbs (D2 coexistence)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;
    use tx_substrate::step_v3::{InterestMask, WaitSourceId};

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::WaitSource;

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        Arc::new(WaitSource::new(WaitSourceId::new(source_id)))
    }

    pub fn fire_legacy_channel(channel: &Channel, mask_bits: u64) {
        channel.fire(Mask::from_bits(mask_bits));
    }

    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        source.notify(InterestMask::new(mask_bits));
    }
}
