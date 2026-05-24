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
    reason = "wrap reactor Channel/Mask as AIO iocb-arrived and events-available legacy wake channels"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_reactor::wait::Channel;
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
