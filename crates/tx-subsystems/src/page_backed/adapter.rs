//! Substrate adapter for page_backed.
//!
//! Page_backed has no reactor surface in production (the two
//! tx_reactor refs in the directory are inside test files). Single
//! domain `step_engine` covers everything: step_v3 types used by the
//! per-variant fetch / write step ops, zone role types for the
//! `PageContainer` zone, the `page_allocator` surface (allocator,
//! cache pins, device frames, gift pins, map pins, zero policy), EBR Guard +
//! guard, and SpinMutex.

use tx_platform_adapter::platform_adapter;

#[platform_adapter(
    platform = "substrate",
    domain = "step_engine",
    apis = ["step", "zone", "epoch", "page_allocator"],
    reason = "expose substrate step engine outcome types, zone role types, page-allocator primitives (BitmapPageAllocator, CachePin, DeviceFrame, GiftPin, MapPin, ZeroPolicy), EBR guard, and SpinMutex used by the page_backed subsystem's per-variant fetch/write step ops"
)]
pub mod step_engine {
    pub use tx_substrate::epoch::{borrow_current_guard, guard, Guard};
    pub use tx_substrate::page_allocator::{
        self, AllocError, BitmapPageAllocator, CachePin, DeviceFrame, GiftPin, MapPin, ZeroPolicy,
    };
    pub use tx_substrate::step::{
        ByteProgress, Errno, InterestMask, NoProgress, PageProgress,
        ProcessIdentity as PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome, StepProgress,
        SubjectIdentity, WaitSourceId, YieldShape,
    };
    pub use tx_substrate::zone::{
        register_zone_for, reserve_for, sign, sign_for, Cap, Dead, Entity, IdentRef, PayloadCap,
        Weak, Zone, ZoneAllocated, ZoneError,
    };
}

#[platform_adapter(
    platform = "substrate",
    domain = "wait_routing",
    apis = ["wake"],
    reason = "wrap WaitSource construction, registration, notification, and unregistration for PageBacked page-fetch in-flight wait sources"
)]
pub mod wait_routing {
    use alloc::sync::Arc;

    pub use tx_substrate::bus::RawQueue;
    use tx_substrate::step::WaitSourceId;
    pub use tx_substrate::wake::{MailboxEvent, TaskMailbox, WaitSource};

    pub fn new_wait_source(source_id: u64) -> Arc<WaitSource> {
        let source = tx_substrate::wake::new_source(source_id);
        tx_substrate::wake::register_source(Arc::clone(&source));
        source
    }

    pub fn notify_source_with_post<F>(source: &Arc<WaitSource>, mask_bits: u64, mut post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        source.notify_with_owner_post(
            tx_substrate::step::InterestMask::new(mask_bits),
            tx_substrate::wake::MailboxSchedulerHint::Normal,
            |mailbox, event, _hint| post(mailbox, event),
        );
    }

    pub fn new_readiness_queue(source_id: u64) -> RawQueue {
        let queue = RawQueue::with_source_id(WaitSourceId::new(source_id));
        crate::wait_source::register_wait_queue_with_id(source_id, queue.clone());
        queue
    }

    pub fn notify_readiness(queue: &RawQueue, mask_bits: u64) {
        queue.fire(mask_bits);
    }

    pub fn clear_readiness(queue: &RawQueue, mask_bits: u64) {
        queue.clear(mask_bits);
    }

    pub fn unregister_source(source_id: u64) {
        tx_substrate::wake::unregister_source(tx_substrate::step::WaitSourceId::new(source_id));
        crate::wait_source::release_wait_source(source_id);
    }
}
