use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "wake"],
    reason = "expose substrate step engine outcome/error types, delegate registry, WaitSource, TaskMailbox, zone allocation, and SpinMutex for userfaultfd pending-fault queue and step_ufd_read"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub(crate) use crate::sync::SpinMutex;
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
    use alloc::sync::Arc;

    pub use tx_reactor::wait::{Channel, Mask};
    pub use tx_substrate::wake::WaitSource;

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }
}
