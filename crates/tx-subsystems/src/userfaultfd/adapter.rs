use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step_v3", "zone", "wake"],
    reason = "expose substrate step engine outcome/error types, delegate registry, WaitSource, TaskMailbox, zone allocation, and SpinMutex for userfaultfd pending-fault queue and step_ufd_read"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step_v3::{
        ByteProgress, DelegateRegistry, DelegateReply, DelegateState, DelegateTokenId, Errno,
        Errno as V3Errno, InterestMask, StepOutcome, TransitionOutcome, UfdReply, WaitSourceId,
    };
    pub use tx_substrate::wake::{TaskMailbox, WaitSource};
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
    reason = "wrap reactor Channel/Mask as userfaultfd legacy read-readiness wake channel"
)]
pub mod wait_routing {
    pub use tx_reactor::wait::{Channel, Mask};
}
