//! Eventfd notification meanings.
//!
//! This module owns the eventfd-side readable/writable codes and semantic
//! wake/wait verbs. Raw wait-source primitives stay behind `adapter.rs`.

use tx_platform_adapter::notification_adapter;

pub(crate) use readiness::{
    new_wait_points, notify_readable, notify_readable_with_post, notify_writable,
    notify_writable_with_post, wait_until_readable, wait_until_writable,
};

#[notification_adapter(
    subsystem = "eventfd",
    domain = "readiness",
    reason = "eventfd notification.rs owns readable and writable wait codes and wake verbs"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::eventfd::adapter::step_engine::{
        self, ByteOutcome, NoProgress, StepOutcome, WaitSource,
    };
    use crate::eventfd::adapter::wait_routing::{self, MailboxEvent, TaskMailbox};

    /// Counter is nonzero and readable.
    pub const EVENTFD_READABLE: u64 = 0x1;
    /// Counter can accept another write without overflowing.
    pub const EVENTFD_WRITABLE: u64 = 0x2;

    pub(crate) struct EventfdWaitPoints {
        reader_source_id: u64,
        reader_source: Arc<WaitSource>,
        writer_source_id: u64,
        writer_source: Arc<WaitSource>,
    }

    impl EventfdWaitPoints {
        pub(crate) fn reader_endpoint(&self) -> &Arc<WaitSource> {
            &self.reader_source
        }

        pub(crate) fn writer_endpoint(&self) -> &Arc<WaitSource> {
            &self.writer_source
        }

        pub(crate) fn into_parts(self) -> (u64, Arc<WaitSource>, u64, Arc<WaitSource>) {
            (
                self.reader_source_id,
                self.reader_source,
                self.writer_source_id,
                self.writer_source,
            )
        }
    }

    pub(crate) fn new_wait_points() -> EventfdWaitPoints {
        let reader_source_id = crate::allocate_notification_source_id();
        let writer_source_id = crate::allocate_notification_source_id();
        let reader_source = wait_routing::new_wait_source(reader_source_id);
        let writer_source = wait_routing::new_wait_source(writer_source_id);
        crate::wait_source::register_wait_source_with_id(
            reader_source_id,
            Arc::clone(&reader_source),
        );
        crate::wait_source::register_wait_source_with_id(
            writer_source_id,
            Arc::clone(&writer_source),
        );
        EventfdWaitPoints {
            reader_source_id,
            reader_source,
            writer_source_id,
            writer_source,
        }
    }

    pub(crate) fn notify_readable(source: &Arc<WaitSource>) {
        notify_readable_with_post(source, |mailbox, event| mailbox.post(event));
    }

    pub(crate) fn notify_readable_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_source_with_post(source, EVENTFD_READABLE, post);
    }

    pub(crate) fn notify_writable(source: &Arc<WaitSource>) {
        notify_writable_with_post(source, |mailbox, event| mailbox.post(event));
    }

    pub(crate) fn notify_writable_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_source_with_post(source, EVENTFD_WRITABLE, post);
    }

    pub(crate) fn wait_until_readable(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> ByteOutcome {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        step_engine::yield_until_readable(source_id, EVENTFD_READABLE)
    }

    pub(crate) fn wait_until_writable(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<(), NoProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        step_engine::yield_until_writable(source_id, EVENTFD_WRITABLE)
    }
}
