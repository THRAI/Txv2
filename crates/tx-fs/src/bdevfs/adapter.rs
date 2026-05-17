//! Substrate adapter for bdev-fs.
//!
//! Same shape as devfs's adapter — single `step_engine` domain
//! covering step_v3, zone role types, epoch Guard, page-allocator
//! primitives, and SpinMutex. No reactor surface in production.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, page-allocator primitives, and SpinMutex used by bdev-fs FsOps and FsPageBacking"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{self as epoch, guard, Guard};
    pub use tx_substrate::page_allocator::{self, ZeroPolicy};
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ProcessIdentity, ScriptCtx, StepOp, StepOutcome,
        SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;
}
