//! Substrate adapter for tx-fat.
//!
//! Mirrors `tx-ext4::adapter` — exposes step engine outcome types,
//! zone role types, EBR guard, and page-allocator primitives.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and page-allocator primitives used by tx-fat FsOps implementation"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::page_allocator;
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity,
        Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;
}
