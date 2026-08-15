//! L6 submission state owned independently from [`super::PageContainerState`].
//!
//! The handle intentionally keeps queue mutation and completion routing behind
//! a manager lock. PageContainer retains file-page and direct-I/O semantic
//! ownership; callers must finish direct leases after this manager has returned
//! the remove-first completion routes.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

#[cfg(test)]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::execution::Errno;
use crate::fs_iface::IoDataLeaseId;
#[cfg(test)]
use crate::io_manager::backend::PageFrameRef;
use crate::io_manager::backend::{BlockPageCompletion, BlockPageRequestTracker};
use crate::io_manager::block::{
    BioPlan, BlockCompletion, BlockDeviceCompletion, BlockDispatch, BlockQueue, BlockRequestId,
    BlockServiceDriver, BlockServiceNext, BlockTagTable, QueueError, SubmitOutcome,
};
#[cfg(test)]
use crate::io_manager::page::service::{PageService, PageServiceBlockCompletionOutcome};
use crate::io_manager::page::{
    service::{
        PageServiceBackendSubmitOutcome, PageServiceBlockCompletionError,
        PageServiceTaggedBlockCompletionError,
    },
    PageL6Action, PageL6Receipt, PageL6SubmitFailure,
};
use crate::io_manager::runtime::{QueueDepth, ServiceBudget, ServiceKick};
use crate::sync::SpinMutex;

#[derive(Debug)]
pub(crate) struct BlockSubmissionManager {
    namespace: u64,
    state: SpinMutex<BlockSubmissionState>,
}

static NEXT_BLOCK_SUBMISSION_NAMESPACE: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
static BLOCK_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn reset_block_submission_manager_lock_acquisitions_for_test() {
    BLOCK_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST.store(0, Ordering::Release);
}

#[cfg(test)]
pub(crate) fn block_submission_manager_lock_acquisitions_for_test() -> usize {
    BLOCK_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST.load(Ordering::Acquire)
}

impl BlockSubmissionManager {
    fn lock_state(&self) -> tx_substrate::SpinMutexGuard<'_, BlockSubmissionState> {
        #[cfg(test)]
        BLOCK_SUBMISSION_MANAGER_LOCK_ACQUISITIONS_FOR_TEST.fetch_add(1, Ordering::AcqRel);
        self.state.lock()
    }
}

#[derive(Debug)]
struct BlockSubmissionState {
    queue: BlockQueue,
    tracker: BlockPageRequestTracker,
    direct_tracker: DirectIoBlockTracker,
    depth: QueueDepth,
    tags: BlockTagTable,
}

/// Typed L6 ownership token retained by a PageContainer.
#[derive(Clone, Debug)]
pub(crate) struct BlockSubmissionHandle(Arc<BlockSubmissionManager>);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BlockSubmissionDiagnosticSnapshot {
    pub(crate) queued: usize,
    pub(crate) dispatch_blocked: usize,
    pub(crate) front_dispatch_blocked: bool,
    pub(crate) page_routes: usize,
    pub(crate) direct_routes: usize,
    pub(crate) depth_limit: usize,
    pub(crate) depth_in_flight: usize,
    pub(crate) tags_in_flight: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BlockManagerDriven {
    pub(crate) dispatches: Vec<BlockDispatch>,
    pub(crate) next: crate::io_manager::block::BlockServiceNext,
    pub(crate) kicks: usize,
}

#[cfg(test)]
pub(crate) struct BlockManagerCompletion {
    pub(crate) page: PageServiceBlockCompletionOutcome,
    pub(crate) direct: Vec<(IoDataLeaseId, Result<(), Errno>)>,
}

/// Remove-first L6 completion facts. PageBacked must consume `direct` only
/// after the L6 manager lock has been released.
pub(crate) struct BlockManagerCompletionReceipt {
    pub(crate) block: BlockCompletion,
    pub(crate) page: Vec<BlockPageCompletion>,
    pub(crate) direct: Vec<(IoDataLeaseId, Result<(), Errno>)>,
}

#[derive(Debug, Default)]
struct DirectIoBlockTracker {
    pending: BTreeMap<BlockRequestId, Vec<IoDataLeaseId>>,
}

impl DirectIoBlockTracker {
    fn record(&mut self, lease: IoDataLeaseId, outcome: SubmitOutcome) {
        let id = match outcome {
            SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
        };
        let leases = self.pending.entry(id).or_default();
        if !leases.contains(&lease) {
            leases.push(lease);
        }
    }

    fn complete(
        &mut self,
        completion: &BlockCompletion,
    ) -> Vec<(IoDataLeaseId, Result<(), Errno>)> {
        self.pending
            .remove(&completion.id)
            .unwrap_or_default()
            .into_iter()
            .map(|lease| (lease, completion.result))
            .collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }
}

impl BlockSubmissionHandle {
    pub(crate) fn new(max_pending: usize, queue_depth: usize) -> Self {
        Self(Arc::new(BlockSubmissionManager {
            namespace: NEXT_BLOCK_SUBMISSION_NAMESPACE.fetch_add(1, Ordering::AcqRel),
            state: SpinMutex::new(BlockSubmissionState {
                queue: BlockQueue::new(max_pending),
                tracker: BlockPageRequestTracker::new(),
                direct_tracker: DirectIoBlockTracker::default(),
                depth: QueueDepth::new(queue_depth),
                tags: BlockTagTable::new(),
            }),
        }))
    }

    pub(crate) fn namespace(&self) -> u64 {
        self.0.namespace
    }

    pub(crate) fn has_in_flight(&self) -> bool {
        !self.0.lock_state().tags.is_empty()
    }

    pub(crate) fn has_immediate_work(&self) -> bool {
        let state = self.0.lock_state();
        state
            .queue
            .can_dispatch_next_tagged(&state.depth, &state.tags)
            || !state.tags.is_empty()
    }

    /// Re-observe L6 after dispatch completions have returned their tags.
    ///
    /// `BlockServiceDriver::drive_once` decides `next` before the concrete
    /// device is polled.  A synchronous device can complete every dispatched
    /// request in the same service turn, reopening queue depth that was full
    /// when that preliminary decision was made.  Callers must use this value
    /// after completion processing instead of carrying the stale pre-poll
    /// decision into the task's sleep protocol.
    pub(crate) fn service_next(&self) -> BlockServiceNext {
        let state = self.0.lock_state();
        if state
            .queue
            .can_dispatch_next_tagged(&state.depth, &state.tags)
        {
            BlockServiceNext::Runnable
        } else if state.queue.is_empty() && state.tags.is_empty() {
            BlockServiceNext::Sleeping
        } else {
            BlockServiceNext::WaitingForCompletion
        }
    }

    pub(crate) fn diagnostic_snapshot(&self) -> BlockSubmissionDiagnosticSnapshot {
        let state = self.0.lock_state();
        BlockSubmissionDiagnosticSnapshot {
            queued: state.queue.len(),
            dispatch_blocked: state.queue.dispatch_blocked_len(),
            front_dispatch_blocked: state.queue.front_dispatch_blocked(),
            page_routes: state.tracker.len(),
            direct_routes: state.direct_tracker.pending.len(),
            depth_limit: state.depth.limit(),
            depth_in_flight: state.depth.in_flight(),
            tags_in_flight: state.tags.len(),
        }
    }

    pub(crate) fn submit_direct(
        &self,
        lease: IoDataLeaseId,
        plan: BioPlan,
    ) -> Result<SubmitOutcome, QueueError> {
        let mut state = self.0.lock_state();
        let outcome = state.queue.submit(plan)?;
        state.direct_tracker.record(lease, outcome);
        Ok(outcome)
    }

    /// Submit a value-only L4 action. L6 never receives a PageService borrow,
    /// retained page lease, or graph/metadata semantic state.
    pub(crate) fn submit_page_action(&self, action: PageL6Action) -> PageL6Receipt {
        let id = action.id();
        let mut state = self.0.lock_state();
        let mut submitted = Vec::new();
        let mut failure = None;
        for (failed_index, plan) in action.into_bios().into_vec().into_iter().enumerate() {
            match state.queue.submit(plan) {
                Ok(outcome) => {
                    state.queue.block_dispatch(block_request_id(outcome));
                    submitted.push(outcome);
                }
                Err(error) => {
                    failure = Some(PageL6SubmitFailure {
                        failed_index,
                        error,
                    });
                    break;
                }
            }
        }
        PageL6Receipt {
            id,
            submitted,
            failure,
        }
    }

    /// Atomically publish every L6 page route and make its accepted BIOs
    /// dispatchable. This must run only after L4 accepts the matching receipt.
    pub(crate) fn record_page_outcome(&self, outcome: &PageServiceBackendSubmitOutcome) {
        let mut state = self.0.lock_state();
        record_page_submissions(&mut state.tracker, outcome);
        for outcome in page_action_submit_outcomes(outcome) {
            state.queue.unblock_dispatch(block_request_id(*outcome));
        }
    }

    /// Consume only L6 tag/depth/tracker state. The returned values are then
    /// routed through L4 after the manager lock is released.
    pub(crate) fn complete_receipt(
        &self,
        completion: BlockDeviceCompletion,
    ) -> Result<BlockManagerCompletionReceipt, PageServiceTaggedBlockCompletionError> {
        let mut state = self.0.lock_state();
        let BlockSubmissionState {
            tracker,
            direct_tracker,
            depth,
            tags,
            ..
        } = &mut *state;
        let block = tags.complete(depth, completion.tag, completion.result)?;
        let direct = direct_tracker.complete(&block);
        let page = if tracker.contains(block.id) {
            tracker
                .complete(block.clone())
                .map_err(PageServiceBlockCompletionError::Tracker)?
        } else {
            Vec::new()
        };
        Ok(BlockManagerCompletionReceipt {
            block,
            page,
            direct,
        })
    }

    pub(crate) fn drive<F>(&self, budget: ServiceBudget, kick: F) -> BlockManagerDriven
    where
        F: FnMut(ServiceKick) -> bool,
    {
        let mut driver = BlockServiceDriver::new(budget);
        let mut state = self.0.lock_state();
        let BlockSubmissionState {
            queue, depth, tags, ..
        } = &mut *state;
        let driven = driver.drive_once(queue, depth, tags, kick);
        BlockManagerDriven {
            dispatches: driven.step.dispatches,
            next: driven.step.next,
            kicks: driven.kicks,
        }
    }

    #[cfg(test)]
    pub(crate) fn complete<F>(
        &self,
        service: &mut PageService,
        completion: BlockDeviceCompletion,
        frame_for: F,
    ) -> Result<BlockManagerCompletion, PageServiceTaggedBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let mut state = self.0.lock_state();
        let BlockSubmissionState {
            queue,
            tracker,
            direct_tracker,
            depth,
            tags,
        } = &mut *state;
        let block_completion = tags.complete(depth, completion.tag, completion.result)?;
        let direct = direct_tracker.complete(&block_completion);
        let page = service.push_completed_block_completion_with_graphs(
            tracker,
            queue,
            block_completion,
            !direct.is_empty(),
            frame_for,
        )?;
        Ok(BlockManagerCompletion { page, direct })
    }

    #[cfg(test)]
    pub(crate) fn queue_len_for_test(&self) -> usize {
        self.0.lock_state().queue.len()
    }

    #[cfg(test)]
    pub(crate) fn tracker_len_for_test(&self) -> usize {
        self.0.lock_state().tracker.len()
    }

    #[cfg(test)]
    pub(crate) fn direct_tracker_len_for_test(&self) -> usize {
        self.0.lock_state().direct_tracker.len()
    }

    #[cfg(test)]
    pub(crate) fn submit_untracked_for_test(
        &self,
        plan: BioPlan,
    ) -> Result<SubmitOutcome, QueueError> {
        self.0.lock_state().queue.submit(plan)
    }
}

fn record_page_submissions(
    tracker: &mut BlockPageRequestTracker,
    outcome: &PageServiceBackendSubmitOutcome,
) {
    if let PageServiceBackendSubmitOutcome::BlockBiosQueued { request, submitted } = outcome {
        tracker.record_submit_outcomes(request.clone(), submitted);
    }
}

fn page_action_submit_outcomes(outcome: &PageServiceBackendSubmitOutcome) -> &[SubmitOutcome] {
    match outcome {
        PageServiceBackendSubmitOutcome::BlockBiosQueued { submitted, .. }
        | PageServiceBackendSubmitOutcome::MetadataFirstQueued { submitted, .. }
        | PageServiceBackendSubmitOutcome::BlockGraphQueued { submitted, .. } => submitted,
        PageServiceBackendSubmitOutcome::QueuedPageCompletions { .. }
        | PageServiceBackendSubmitOutcome::Yield(_)
        | PageServiceBackendSubmitOutcome::Err { .. } => &[],
    }
}

const fn block_request_id(outcome: SubmitOutcome) -> BlockRequestId {
    match outcome {
        SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
    }
}
