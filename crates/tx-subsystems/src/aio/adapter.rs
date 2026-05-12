use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "wake"],
    reason = "expose substrate step engine on-behalf-of framework (with_on_behalf_of, AbortSignal, OnBehalfOfAbort, SubjectIdentity, SubjectContext, ScriptCtx, CancelReason, InterestMask, WaitSourceId), WaitSource, zone allocation, and SpinMutex for AIO context and worker future"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::step::{
        with_on_behalf_of, AbortSignal, CancelReason, InterestMask, OnBehalfOfAbort, ScriptCtx,
        SubjectContext, SubjectIdentity, WaitSourceId,
    };
    pub use tx_substrate::wake::WaitSource;
    pub use tx_substrate::zone::{Cap, Zone, ZoneAllocated, ZoneError};
    pub use tx_substrate::SpinMutex;

    pub fn sign_zone_for<T: ZoneAllocated>(value: T) -> Result<Cap<T>, ZoneError> {
        let reservation = zone::reserve_for::<T>()?;
        Ok(zone::sign_for(reservation, value))
    }

    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        zone::register_zone_for::<T>().map(|_| ())
    }
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap reactor Channel/Mask as AIO iocb-arrived and events-available legacy wake channels"
)]
pub mod wait_routing {
    pub use tx_reactor::wait::Channel;
}
