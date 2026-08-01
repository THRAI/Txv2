//! Substrate wake adapter for net/socket readiness (P3-S1).
//!
//! Same `wait_routing` shape as the eventfd/futex adapters: wraps
//! substrate `WaitSource` registration and the v3 notify verb so socket
//! readiness carriers can be dual-registered — the subsystems registry
//! (consumed by `wait_on_token`, i.e. ppoll/pselect and the legacy socket
//! park paths) AND the substrate registry (consumed by
//! `await_wait_source`, i.e. epoll). Closing audit R4a: before this,
//! epoll looked sockets up in a registry they were never in and
//! `epoll_wait` on a pure-socket set returned 0 immediately.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake", "step"],
    reason = "wrap substrate WaitSource registration + v3 notify so socket readiness queues are visible to await_wait_source/epoll (R4a dual-registry fix)"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::wake::{MailboxEvent, TaskMailbox, WaitSource};

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
    }

    pub fn notify_v3_source(source: &Arc<WaitSource>, mask_bits: u64) {
        // `wake::notify` reports how many waiters it woke; the mirror path
        // has no use for the count.
        let _woken = tx_substrate::wake::notify(source, mask_bits);
    }

    pub fn notify_v3_source_with_post(
        source: &Arc<WaitSource>,
        mask_bits: u64,
        post: &mut dyn FnMut(&TaskMailbox, MailboxEvent) -> bool,
    ) {
        source.notify_with_owner_post(
            tx_substrate::step::InterestMask::new(mask_bits),
            tx_substrate::wake::MailboxSchedulerHint::Normal,
            |mailbox, event, _hint| post(mailbox, event),
        );
    }
}
