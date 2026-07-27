//! Pipe notification meanings.
//!
//! This module owns the pipe-side readiness codes and semantic wake verbs.
//! The underlying `WaitSource` primitive lives in `adapter.rs`; pipe execution
//! code calls the names here.

pub(crate) use readiness::{
    new_wait_points, notify_readable, notify_readable_with_post, notify_writable,
    notify_writable_with_post, release_wait_points, wait_until_readable, wait_until_writable,
    yield_until_readable, yield_until_writable,
};
pub use readiness::{PIPE_READABLE, PIPE_WRITABLE};

use tx_platform_adapter::notification_adapter;

#[notification_adapter(
    subsystem = "pipe",
    domain = "readiness",
    reason = "pipe notification.rs owns readable and writable wait codes and wake verbs"
)]
mod readiness {
    use alloc::sync::Arc;

    use crate::pipe::adapter::step_engine::{self, ByteProgress, StepOutcome};
    use crate::pipe::adapter::wait_routing::{self, MailboxEvent, TaskMailbox, WaitSource};

    /// Bytes are available to read, or writer close made EOF observable.
    pub const PIPE_READABLE: u64 = 0x1;
    /// Space is available to write, or reader close made EPIPE observable.
    pub const PIPE_WRITABLE: u64 = 0x2;

    pub(crate) struct PipeWaitPoints {
        reader_source_id: u64,
        reader_source: Arc<WaitSource>,
        writer_source_id: u64,
        writer_source: Arc<WaitSource>,
    }

    impl PipeWaitPoints {
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

    pub(crate) fn new_wait_points() -> PipeWaitPoints {
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

        PipeWaitPoints {
            reader_source_id,
            reader_source,
            writer_source_id,
            writer_source,
        }
    }

    pub(crate) fn release_wait_points(reader_source_id: u64, writer_source_id: u64) {
        crate::wait_source::release_wait_source(reader_source_id);
        crate::wait_source::release_wait_source(writer_source_id);
        wait_routing::unregister_source(reader_source_id);
        wait_routing::unregister_source(writer_source_id);
    }

    pub(crate) fn notify_readable(source: &Arc<WaitSource>) {
        notify_readable_with_post(source, |mailbox, event| mailbox.post(event));
    }

    pub(crate) fn notify_readable_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_source_with_post(source, PIPE_READABLE, post);
    }

    pub(crate) fn notify_writable(source: &Arc<WaitSource>) {
        notify_writable_with_post(source, |mailbox, event| mailbox.post(event));
    }

    pub(crate) fn notify_writable_with_post<F>(source: &Arc<WaitSource>, post: F)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        wait_routing::notify_source_with_post(source, PIPE_WRITABLE, post);
    }

    pub(crate) fn wait_until_readable(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<usize, ByteProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        step_engine::yield_until_readable(source_id, PIPE_READABLE)
    }

    pub(crate) fn yield_until_readable<T>(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<T, ByteProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, PIPE_READABLE)
    }

    pub(crate) fn wait_until_writable(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<usize, ByteProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        step_engine::yield_until_writable(source_id, PIPE_WRITABLE)
    }

    pub(crate) fn yield_until_writable<T>(
        endpoint: &(impl tx_substrate::wake::WaitEndpoint + ?Sized),
    ) -> StepOutcome<T, ByteProgress> {
        let source_id = tx_substrate::wake::WaitEndpoint::source_id(endpoint).raw();
        StepOutcome::yield_on_wait_source(ByteProgress::EMPTY, source_id, PIPE_WRITABLE)
    }
}
