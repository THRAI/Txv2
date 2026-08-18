//! Typed L4 service ownership for page-backed I/O.
//!
//! PageBacked retains PageSlot and retained data-lease semantics. This manager
//! owns only request scheduling, completion routing, and its service wake
//! endpoint, so no PageContainer state lock is needed to drive the L4 queue.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

#[cfg(test)]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::fs_iface::{IoDataSource, IoDataTarget};
use crate::io_manager::runtime::{IoServiceKind, ServiceKick, ServiceWakeSource};
use crate::page_backed::OwnedFileIoRequest;
use crate::sync::SpinMutex;
use crate::{
    io_manager::backend::{BlockPageCompletion, PageFrameRef},
    io_manager::block::BlockCompletion,
};

#[cfg(test)]
use super::service::PageWaitError;
use super::service::PageWaiter;
use super::service::{
    PageService, PageServiceBackendOutcome, PageServiceBackendPrepared,
    PageServiceBackendSubmitError, PageServiceBlockCompletionPrepared,
    PageServiceDiagnosticSnapshot, PageServiceL6Applied, PageServiceTaggedBlockCompletionError,
    PageServiceWake,
};
#[cfg(test)]
use super::PageIoCompletion;
use super::{
    PageContainerKey, PageGeneration, PageIoFlags, PageIoOp, PageIoPriority, PageIoRange,
    PageIoRequest, PageIoRequestId, PageL6Receipt, PageQueueError,
};
#[cfg(test)]
use crate::io_manager::backend::PageCompletion;

#[derive(Debug)]
pub(crate) struct PageIoSubmissionManager {
    state: SpinMutex<PageIoSubmissionState>,
    /// Level-triggered lifetime edge for the reactor-owned service task.
    ///
    /// A wake notification alone is insufficient because PageContainer
    /// retirement can race the task's wait-source subscription. Keeping the
    /// retirement state latched lets the waiter re-check it after installing
    /// the subscription and close that lost-wake window.
    owner_retired: AtomicBool,
}

#[cfg(test)]
static PAGE_IO_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_page_io_submission_manager_lock_acquisitions_for_test() {
    PAGE_IO_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST.store(0, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn page_io_submission_manager_lock_acquisitions_for_test() -> usize {
    PAGE_IO_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST.load(Ordering::Acquire)
}

impl PageIoSubmissionManager {
    fn lock_state(&self) -> tx_substrate::SpinMutexGuard<'_, PageIoSubmissionState> {
        #[cfg(test)]
        PAGE_IO_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST.fetch_add(1, Ordering::AcqRel);
        self.state.lock()
    }
}

#[derive(Debug)]
struct PageIoSubmissionState {
    service: PageService,
    admitted_file_requests: BTreeMap<PageIoRequestId, AdmittedFileRequest>,
    background_graphs: BTreeSet<PageIoRequestId>,
    wake_source: Option<Arc<ServiceWakeSource>>,
}

/// The retained page-cache evidence was previously guarded by
/// `PageContainerState`. L4 now provides that same mutex-protected custody;
/// access is only by immutable projection or one terminal removal.
#[derive(Debug)]
struct AdmittedFileRequest(OwnedFileIoRequest);

// SAFETY: the wrapped CachePin is never dereferenced or cloned through this
// wrapper. Every access is serialized by PageIoSubmissionManager.state, and a
// terminal path moves it back to PageBacked for the PageSlot transition.
unsafe impl Send for AdmittedFileRequest {}

/// Typed L4 ownership token retained by a page-backed object.
#[derive(Clone, Debug)]
pub(crate) struct PageIoSubmissionHandle(Arc<PageIoSubmissionManager>);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PageIoSubmissionDiagnosticSnapshot {
    pub(crate) service: PageServiceDiagnosticSnapshot,
    pub(crate) admitted_file_requests: usize,
    pub(crate) background_graphs: usize,
    pub(crate) wake_source_id: u64,
    pub(crate) wake_pending_mask: u64,
    pub(crate) wake_subscribers: usize,
}

impl PageIoSubmissionHandle {
    pub(crate) fn has_immediate_work(&self, include_submissions: bool) -> bool {
        self.0
            .lock_state()
            .service
            .has_immediate_work(include_submissions)
    }

    pub(crate) fn new(max_pending: usize) -> Self {
        Self(Arc::new(PageIoSubmissionManager {
            state: SpinMutex::new(PageIoSubmissionState {
                service: PageService::new(max_pending),
                admitted_file_requests: BTreeMap::new(),
                background_graphs: BTreeSet::new(),
                wake_source: None,
            }),
            owner_retired: AtomicBool::new(false),
        }))
    }

    pub(crate) fn with_service<R>(&self, f: impl FnOnce(&mut PageService) -> R) -> R {
        f(&mut self.0.lock_state().service)
    }

    pub(crate) fn diagnostic_snapshot(&self) -> PageIoSubmissionDiagnosticSnapshot {
        let state = self.0.lock_state();
        let (wake_source_id, wake_pending_mask, wake_subscribers) = state
            .wake_source
            .as_ref()
            .map(|source| {
                let endpoint = source.wake_endpoint();
                (
                    source.source_id(),
                    endpoint.pending_mask_snapshot(),
                    endpoint.subscriber_count(),
                )
            })
            .unwrap_or((0, 0, 0));
        PageIoSubmissionDiagnosticSnapshot {
            service: state.service.diagnostic_snapshot(),
            admitted_file_requests: state.admitted_file_requests.len(),
            background_graphs: state.background_graphs.len(),
            wake_source_id,
            wake_pending_mask,
            wake_subscribers,
        }
    }

    /// Publish a queued L4 request and its retained data owner together.
    ///
    /// The service can become runnable as soon as `PageService::submit`
    /// returns. Keeping both mutations under the manager lock prevents a
    /// concurrent service turn from observing a request before its source or
    /// target lease has been installed.
    pub(crate) fn submit_owned_file_request(
        &self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
        priority: PageIoPriority,
        flags: PageIoFlags,
        generation_hint: Option<PageGeneration>,
        make_owner: impl FnOnce(PageIoRequest) -> OwnedFileIoRequest,
    ) -> Option<PageIoRequestId> {
        let mut state = self.0.lock_state();
        let id = state
            .service
            .submit(pc, range, op, priority, flags, generation_hint)
            .ok()?;
        let request = PageIoRequest::new(id, pc, range, op, priority, flags, generation_hint);
        let replaced = state
            .admitted_file_requests
            .insert(id, AdmittedFileRequest(make_owner(request)));
        debug_assert!(replaced.is_none(), "L4 request identifiers are unique");
        Some(id)
    }

    /// L4 retains the page-cache source/target bundle from admission until its
    /// terminal completion. PageBacked only consumes the bundle to apply the
    /// corresponding PageSlot transition.
    pub(crate) fn admit_file_request(&self, owner: OwnedFileIoRequest) -> bool {
        let request_id = owner.request().id;
        self.0
            .state
            .lock()
            .admitted_file_requests
            .insert(request_id, AdmittedFileRequest(owner))
            .is_none()
    }

    pub(crate) fn file_request_data(
        &self,
        request_id: PageIoRequestId,
    ) -> (IoDataSource, IoDataTarget) {
        let state = self.0.lock_state();
        state
            .admitted_file_requests
            .get(&request_id)
            .map(|owner| (owner.0.source(), owner.0.target()))
            .unwrap_or((IoDataSource::None, IoDataTarget::None))
    }

    pub(crate) fn file_request(&self, request_id: PageIoRequestId) -> Option<PageIoRequest> {
        self.0
            .state
            .lock()
            .admitted_file_requests
            .get(&request_id)
            .map(|owner| owner.0.request().clone())
    }

    pub(crate) fn take_file_request(
        &self,
        request_id: PageIoRequestId,
    ) -> (Option<OwnedFileIoRequest>, Vec<PageWaiter>) {
        let mut state = self.0.lock_state();
        let owner = state
            .admitted_file_requests
            .remove(&request_id)
            .map(|owner| owner.0);
        let waiters = state.service.retire_submission(request_id);
        (owner, waiters)
    }

    /// Requeue a popped initial submission only while its retained file owner
    /// is still live. Cancellation removes the owner and any queued row under
    /// this same lock, so `Ok(None)` is the cancel-before-requeue
    /// linearization point.
    pub(crate) fn requeue_submission_if_file_owner_present(
        &self,
        request: PageIoRequest,
    ) -> Result<Option<PageServiceWake>, PageQueueError> {
        let mut state = self.0.lock_state();
        if !state.admitted_file_requests.contains_key(&request.id) {
            return Ok(None);
        }
        state.service.requeue_submission(request).map(Some)
    }

    /// Register a waiter only while the matching request still has an L4
    /// lifetime owner. Completion removes the owner and queue row under this
    /// same manager lock before mutating PageContainer state, so `false` is
    /// the completion-before-wait linearization point.
    pub(crate) fn register_waiter(&self, request_id: PageIoRequestId, waiter: PageWaiter) -> bool {
        let mut state = self.0.lock_state();
        if !state.admitted_file_requests.contains_key(&request_id)
            && !state.service.has_queued_submission(request_id)
        {
            return false;
        }
        // Re-registering the same page source is idempotent: one live waiter
        // row is sufficient for every joiner on that PageReady endpoint.
        let _ = state.service.wait_on(request_id, waiter);
        true
    }

    pub(crate) fn mark_background_graph(&self, request_id: PageIoRequestId) {
        self.0.lock_state().background_graphs.insert(request_id);
    }

    pub(crate) fn take_background_graph(&self, request_id: PageIoRequestId) -> bool {
        self.0.lock_state().background_graphs.remove(&request_id)
    }

    #[cfg(test)]
    pub(crate) fn admitted_file_request_count(&self) -> usize {
        self.0.lock_state().admitted_file_requests.len()
    }

    #[cfg(test)]
    pub(crate) fn admitted_file_writeback_count(&self) -> usize {
        self.0
            .state
            .lock()
            .admitted_file_requests
            .values()
            .filter(|owner| owner.0.request().op == PageIoOp::Writeback)
            .count()
    }

    #[cfg(test)]
    pub(crate) fn admitted_file_read_count(&self) -> usize {
        self.0
            .state
            .lock()
            .admitted_file_requests
            .values()
            .filter(|owner| owner.0.request().op == PageIoOp::Read)
            .count()
    }

    pub(crate) fn prepare_backend_outcome(
        &self,
        outcome: PageServiceBackendOutcome,
        request: PageIoRequest,
    ) -> Result<PageServiceBackendPrepared, PageServiceBackendSubmitError> {
        self.with_service(|service| service.prepare_backend_outcome(outcome, request))
    }

    pub(crate) fn apply_l6_receipt(
        &self,
        receipt: PageL6Receipt,
    ) -> Result<PageServiceL6Applied, PageServiceBackendSubmitError> {
        self.with_service(|service| service.apply_l6_receipt(receipt))
    }

    pub(crate) fn prepare_block_completion_routes<F>(
        &self,
        completion: BlockCompletion,
        page_routes: Vec<BlockPageCompletion>,
        allow_external_completion: bool,
        frame_for: F,
    ) -> Result<PageServiceBlockCompletionPrepared, PageServiceTaggedBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        self.with_service(|service| {
            service.prepare_block_completion_routes(
                completion,
                page_routes,
                allow_external_completion,
                frame_for,
            )
        })
    }

    pub(crate) fn attach_wake_source(&self, wake_source: Arc<ServiceWakeSource>) -> bool {
        let mut state = self.0.lock_state();
        if self.0.owner_retired.load(Ordering::Acquire) || state.wake_source.is_some() {
            return false;
        }
        state.wake_source = Some(wake_source);
        true
    }

    /// Publish PageContainer owner retirement and wake a parked service task.
    ///
    /// The atomic flag is the level predicate; the wake-source notification is
    /// only the scheduling edge. A waiter installs its subscription before
    /// re-checking this flag, so retirement cannot be lost between observation
    /// and parking. Repeated retirement is intentionally idempotent.
    pub(crate) fn retire_owner(&self) -> bool {
        if self.0.owner_retired.swap(true, Ordering::AcqRel) {
            return false;
        }

        let wake_source = self.0.lock_state().wake_source.clone();
        if let Some(wake_source) = wake_source {
            let _ = wake_source
                .kick_with_post(ServiceKick::new(IoServiceKind::Page), |mailbox, event| {
                    mailbox.post(event)
                });
        }
        true
    }

    pub(crate) fn owner_retired(&self) -> bool {
        self.0.owner_retired.load(Ordering::Acquire)
    }

    pub(crate) fn kick(&self, service: IoServiceKind) {
        let wake_source = self.0.lock_state().wake_source.clone();
        if let Some(wake_source) = wake_source {
            let _ = wake_source.kick_with_post(ServiceKick::new(service), |mailbox, event| {
                mailbox.post(event)
            });
        }
    }

    pub(crate) fn has_wake_source(&self) -> bool {
        self.0.lock_state().wake_source.is_some()
    }

    /// Bounded, value-only queue state used by kernel stall diagnostics.
    pub(crate) fn diagnostic_counts(&self) -> (usize, usize, bool) {
        let state = self.0.lock_state();
        (
            state.service.submission_len(),
            state.admitted_file_requests.len(),
            state.wake_source.is_some(),
        )
    }

    #[cfg(test)]
    pub(crate) fn find_submission(
        &self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
    ) -> Option<PageIoRequest> {
        self.with_service(|service| service.find_submission(pc, range, op).cloned())
    }

    #[cfg(test)]
    pub(crate) fn submit(
        &self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
        priority: PageIoPriority,
        flags: PageIoFlags,
        generation_hint: Option<PageGeneration>,
    ) -> Result<PageIoRequestId, PageQueueError> {
        self.with_service(|service| service.submit(pc, range, op, priority, flags, generation_hint))
    }

    #[cfg(test)]
    pub(crate) fn push_completion(&self, completion: PageIoCompletion) {
        self.with_service(|service| service.push_completion(completion));
    }

    #[cfg(test)]
    pub(crate) fn push_page_completion(&self, completion: PageCompletion) {
        self.with_service(|service| service.push_page_completion(completion));
    }

    #[cfg(test)]
    pub(crate) fn wait_on(
        &self,
        request_id: PageIoRequestId,
        waiter: PageWaiter,
    ) -> Result<(), PageWaitError> {
        self.with_service(|service| service.wait_on(request_id, waiter))
    }

    #[cfg(test)]
    pub(crate) fn submission_len(&self) -> usize {
        self.with_service(|service| service.submission_len())
    }

    #[cfg(test)]
    pub(crate) fn waiter_count(&self, request_id: PageIoRequestId) -> usize {
        self.with_service(|service| service.waiter_count(request_id))
    }

    #[cfg(test)]
    pub(crate) fn reserve_background_request(
        &self,
        pc: PageContainerKey,
        range: PageIoRange,
    ) -> Result<PageIoRequest, PageQueueError> {
        self.with_service(|service| service.reserve_background_request(pc, range))
    }
}
