//! Substrate adapter for I/O manager service wake routing.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "service_wake",
    apis = ["step", "wake"],
    reason = "wrap WaitSource registration and TaskMailbox notification for io_manager service wake sources"
)]
pub mod service_wake {
    use alloc::sync::Arc;

    pub use tx_substrate::step::InterestMask;
    pub use tx_substrate::wake::{
        MailboxEvent, MailboxSchedulerHint, SubscriberId, TaskMailbox, WaitGeneration, WaitSource,
    };

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    #[cfg(test)]
    pub fn source_id(source: &Arc<WaitSource>) -> u64 {
        tx_substrate::wake::WaitEndpoint::source_id(source).raw()
    }
}
