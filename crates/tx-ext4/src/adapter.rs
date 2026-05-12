//! Substrate adapter for tx-ext4.
//!
//! tx-ext4's substrate surface is dominated by step_v3 outcome types
//! in the `FsOps` impl (~67 lines), plus a small page_allocator and
//! epoch::guard footprint. Single `step_engine` domain.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and page-allocator primitives used by tx-ext4 FsOps implementation"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::page_allocator;
    pub use tx_substrate::step::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::SpinMutex;
}
