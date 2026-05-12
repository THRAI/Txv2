//! Substrate adapter for tmpfs.
//!
//! tmpfs is the largest single-file substrate consumer in the
//! workspace (136 lines outside-adapter on entry, dominated by
//! step_v3 trait signatures). Single `step_engine` domain — no
//! reactor surface in production. Re-exports the step_v3 outcome
//! types, zone role types, epoch Guard, and SpinMutex used by the
//! `FsOps` impl, plus pass-through `reserve_for` / `sign_for` and
//! `sign_zone_for`.
//!
//! Crosses crate boundaries: `tx-fs` (this crate) depends on
//! `tx-platform-adapter` so the attribute resolves.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and SpinMutex used by tmpfs FsOps implementation (the largest single-file substrate consumer in the workspace)"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step_v3::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
        YieldShape,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign_for, Cap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::{page_allocator, SpinMutex};

    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}
