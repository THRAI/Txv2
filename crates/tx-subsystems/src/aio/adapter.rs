use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "wake"],
    reason = "expose substrate step engine on-behalf-of framework (with_on_behalf_of, AbortSignal, OnBehalfOfAbort, SubjectIdentity, SubjectContext, ScriptCtx, CancelReason, InterestMask, WaitSourceId), WaitSource, zone allocation, and SpinMutex for AIO context and worker future"
)]
pub mod step_engine {
    use tx_substrate::zone;

    pub(crate) use crate::sync::SpinMutex;
    pub use tx_substrate::step::{
        with_on_behalf_of, AbortSignal, CancelReason, InterestMask, OnBehalfOfAbort, ScriptCtx,
        SubjectContext, SubjectIdentity, WaitSourceId,
    };
    pub use tx_substrate::wake::WaitSource;
    pub use tx_substrate::zone::{sign, Cap, Zone, ZoneAllocated, ZoneError};

    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        zone::register_zone_for::<T>().map(|_| ())
    }
}

#[platform_adapter(
    platform = "reactor",
    domain = "wait_routing",
    reason = "wrap AIO wait-source registration and mailbox notify verbs"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::wake::{MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitSource};

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    pub fn notify_v3_source_with_post<F>(source: &Arc<WaitSource>, mask_bits: u64, mut post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        source.notify_with_owner_post(
            tx_substrate::step::InterestMask::new(mask_bits),
            MailboxSchedulerHint::Normal,
            |mailbox, event, _hint| post(mailbox, event),
        );
    }
}
