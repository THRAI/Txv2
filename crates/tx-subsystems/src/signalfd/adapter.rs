use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "wake"],
    reason = "expose substrate step engine outcome/error types, zone allocation, EBR guard, and WaitSource for signalfd pending-queue and read step"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::epoch::{borrow_current_guard, guard};
    pub use tx_substrate::step::{
        ByteProgress, Errno as V3Errno, InterestMask, StepOutcome, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::wake::WaitSource;
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
    reason = "wrap reactor Channel/Mask as signalfd legacy wake verbs (D2/D4 coexistence)"
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
