//! Substrate adapter for tmpfs.
//!
//! tmpfs is the largest single-file substrate consumer in the
//! workspace (136 lines outside-adapter on entry, dominated by
//! step_v3 trait signatures). Single `step_engine` domain — no
//! reactor surface in production. Re-exports the step_v3 outcome
//! types, zone role types, epoch Guard, and SpinMutex used by the
//! `FsOps` impl, plus pass-through `reserve_for` / `sign_for` and
//! `sign`.
//!
//! Crosses crate boundaries: `tx-fs` (this crate) depends on
//! `tx-platform-adapter` so the attribute resolves.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and SpinMutex used by tmpfs FsOps implementation (the largest single-file substrate consumer in the workspace)"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
        YieldShape,
    };
    pub use tx_substrate::zone::{
        sign, sign_for, reserve_for, register_zone_for,
        Cap, PayloadCap, Weak, IdentRef,
        Dead, ZoneError,
        Entity, CoLocatedEntity, OperationalCapExt, OperationalRefExt, PayloadBinding,
        Zone, ZoneAllocated, IdentitySlot,
        IsPayloadPolicy, CapProducingPolicy, ObserverNodePolicy, PayloadPolicy, RetainedEntityPolicy, ZonePolicy,
    };
    pub use tx_substrate::{page_allocator, SpinMutex};
}
