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
        sign, Cap, OperationalCapExt, Weak, Zone, ZoneAllocated, ZoneError,
    };

    pub fn register_zone_for<T: ZoneAllocated>() -> Result<(), ZoneError> {
        zone::register_zone_for::<T>().map(|_| ())
    }
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake"],
    reason = "wrap WaitSource registration and mailbox notify for signalfd"
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

    pub fn notify_source_with_post<F>(source: &Arc<WaitSource>, mask_bits: u64, mut post: F)
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
