//! Substrate adapter for devfs.
//!
//! Same shape as tmpfs's adapter — single `step_engine` domain
//! covering step_v3, zone role types, epoch Guard, and SpinMutex.
//! No reactor surface in production.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, page-allocator primitives, and SpinMutex used by devfs FsOps and ext4 bridge"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{self as epoch, guard, Guard};
    pub use tx_substrate::page_allocator::{self, ZeroPolicy};
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::SpinMutex;
}
