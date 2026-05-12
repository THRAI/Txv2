//! Substrate adapter for tx-scripts.
//!
//! Small surface — the exec script uses step_v3 outcome types, EBR
//! guard, and the zone Cap role type. Single `step_engine` domain.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "zone", "epoch"],
    reason = "expose substrate step engine outcome types, zone role types, and EBR guard used by the tx-scripts exec script"
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
    pub use tx_substrate::{page_allocator, SpinMutex};

    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }
}
