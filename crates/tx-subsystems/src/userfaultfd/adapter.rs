use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "wake"],
    reason = "expose substrate step engine outcome/error types, delegate registry, WaitSource, TaskMailbox, zone allocation, and SpinMutex for userfaultfd pending-fault queue and step_ufd_read"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub use tx_substrate::epoch::{guard, Guard};
    pub use tx_substrate::step::{
        ByteProgress, DelegateRegistry, DelegateReply, DelegateState, DelegateTokenId, Errno,
        Errno as V3Errno, InterestMask, StepOutcome, TransitionOutcome, UfdReply, WaitSourceId,
    };
    pub use tx_substrate::wake::{TaskMailbox, WaitSource};
    pub use tx_substrate::zone::{
        reserve_for, sign, sign_for, Cap, CapProducingPolicy, CoLocatedEntity, Dead, Entity,
        IdentRef, IdentitySlot, IsPayloadPolicy, ObserverNodePolicy, OperationalCapExt,
        OperationalRefExt, PayloadBinding, PayloadCap, PayloadPolicy, RetainedEntityPolicy, Weak,
        Zone, ZoneAllocated, ZoneError, ZonePolicy,
    };
    pub use tx_substrate::SpinMutex;

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
