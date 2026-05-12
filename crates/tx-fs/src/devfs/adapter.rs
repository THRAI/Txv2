//! Substrate adapter for devfs.
//!
//! Same shape as tmpfs's adapter — single `step_engine` domain
//! covering step_v3, zone role types, epoch Guard, and SpinMutex.
//! No reactor surface in production.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, EBR guard, and SpinMutex used by devfs FsOps implementation"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step_v3::{
        ByteProgress, Errno, NoProgress, ScriptCtx, StepOp, StepOutcome, SubjectIdentity,
    };
    pub use tx_substrate::zone::{
        reserve_for, sign_for, Cap, Zone, ZoneAllocated, ZoneError,
    };
    pub use tx_substrate::SpinMutex;

    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}
