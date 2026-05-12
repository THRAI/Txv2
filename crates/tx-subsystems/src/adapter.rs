//! Crate-root substrate adapter for tx-subsystems shared infrastructure.
//!
//! Covers the crate-root utility files (execution.rs, device.rs,
//! wait_source.rs, zones.rs, lib.rs, initramfs/mod.rs) that span
//! multiple subsystems and don't belong to a single per-subsystem
//! adapter. Mirrors the tx-shims crate-root adapter pattern.
//!
//! Two domains:
//! * `step_engine` — substrate: step_v3 outcome/error/progress types,
//!   zone Cap, EBR Guard/guard, page_allocator, and SpinMutex. Used by
//!   device.rs, execution.rs, and initramfs/mod.rs.
//! * `wait_routing` — reactor: Channel, Mask, WaitFuture. Used by
//!   wait_source.rs.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome/error/progress types, zone Cap, EBR Guard/guard, and page_allocator frame_kernel_addr used by crate-root shared infrastructure files (execution.rs, device.rs, initramfs/mod.rs, zones.rs)"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{self as epoch, guard, EpochSummary, Guard};
    pub use tx_substrate::page_allocator;
    pub use tx_substrate::step::{
        ByteProgress, Errno as V3Errno, NoProgress, RestrictionStackHandle, StepOutcome,
    };
    pub use tx_substrate::zone::{
        self as zone, register_zone_for, reserve_for, sign, sign_for, Cap, CapProducingPolicy,
        CoLocatedEntity, Dead, Entity, IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy,
        OperationalCapExt, OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy,
        RetainedEntityPolicy, Weak, Zone, ZoneAllocated, ZoneError, ZoneInfo, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask/WaitFuture used by wait_source.rs registry"
)]
pub mod wait_routing {
    pub use tx_reactor::wait::{Channel, Mask, WaitFuture, WaitOutcome};
}
