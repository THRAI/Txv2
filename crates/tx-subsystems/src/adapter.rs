//! Crate-root substrate adapter for tx-subsystems shared infrastructure.
//!
//! Covers the crate-root utility files (execution.rs, device.rs,
//! wait_source.rs, zones.rs, lib.rs, initramfs/mod.rs) that span
//! multiple subsystems and don't belong to a single per-subsystem
//! adapter. Mirrors the tx-shims crate-root adapter pattern.
//!
//! Two domains:
//! * `step_engine` — substrate: step_v3 outcome/error/progress types,
//!   zone Cap, EBR Guard/guard, page_allocator, wake diagnostics, and the
//!   subsystem lock facade. Used by device.rs, execution.rs, initramfs/mod.rs,
//!   and zones.rs.
//! * `wait_routing` — reactor wait outcome/mask compatibility used by
//!   wait_source.rs.
//! * `wait_mailbox` — substrate wake mailbox types used by wait_source.rs.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator", "wake"],
    reason = "expose substrate step engine outcome/error/progress types, zone Cap, EBR Guard/guard, page_allocator frame_kernel_addr, and wake registry diagnostics used by crate-root shared infrastructure files (execution.rs, device.rs, initramfs/mod.rs, zones.rs)"
)]
pub mod step_engine {
    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{drain_with_budget, guard, summary, EpochSummary, Guard};
    pub use tx_substrate::page_allocator;
    pub use tx_substrate::step::{
        ByteProgress, Errno as V3Errno, NoProgress, RestrictionStackHandle, ScriptCtx, StepOp,
        StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::wake::registry_summary as wake_registry_summary;
    pub use tx_substrate::zone::{
        self as zone, register_zone_for, reserve_for, sign, sign_for, Cap, Dead, Entity, IdentRef,
        OperationalCapExt, PayloadCap, Weak, Zone, ZoneAllocated, ZoneError, ZoneInfo,
    };
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Mask/WaitOutcome used by wait_source.rs registry"
)]
pub mod wait_routing {
    pub use tx_reactor::wait::{Mask, WaitOutcome};
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["step", "wake"],
    reason = "wrap substrate wake mailbox primitives and InterestMask used by wait_source.rs RawQueue/RawPort futures"
)]
pub mod wait_mailbox {
    pub use tx_substrate::step::InterestMask;
    pub use tx_substrate::wake::mailbox::{ActiveWait, TaskMailbox, WaitGeneration};
}
