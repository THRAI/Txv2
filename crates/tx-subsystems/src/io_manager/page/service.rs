//! Budgeted L4 page-service queue ownership.

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use crate::execution::{Errno, Guard};
use crate::fs_iface::{IoDataSource, IoDataTarget};
use crate::io_manager::backend::graph::BackendGraphStage;
use crate::io_manager::backend::{
    dispatch_backend_plan, plan_backend_request, BackendBioCompletion, BackendBioGraph,
    BackendBioNodeId, BackendDispatch, BackendGraphAdvance, BackendGraphScheduler,
    BackendGraphSchedulerError, BackendPageRequest, BackendPlan, BackendPlanResume, BackendPlanner,
    BioPlanList, BlockPageCompletion, BlockPageCompletionError, BlockPageRequestTracker,
    BlockPageRequestTrackerError, FsObjectKey, PageCompletion, PageFrameRef, PageIoCompletionEntry,
    PagerResumeToken, WaitSourceId,
};
use crate::io_manager::block::{
    BlockCompletion, BlockCompletionError, BlockQueue, BlockRequestId, BlockTag, BlockTagTable,
    QueueError, SubmitOutcome,
};
use crate::io_manager::page::{
    PageContainerKey, PageIoCompletion, PageIoFlags, PageIoOp, PageIoPriority, PageIoRange,
    PageIoRequest, PageIoRequestId, PageL6Action, PageL6ActionId, PageL6Receipt,
    PageL6SubmitFailure, PageQueueError, PageRequestQueue,
};
use crate::io_manager::runtime::{IoServiceKind, QueueDepth, ServiceBudget, ServiceKick};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageWaitInterest(u64);

impl PageWaitInterest {
    pub const READY: Self = Self(1 << 0);

    pub const fn bits(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageWaiter {
    pub source_id: u64,
    pub interests: PageWaitInterest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageCompletionRoute {
    pub completion: PageIoCompletion,
    pub frame: Option<PageFrameRef>,
    pub waiters: Vec<PageWaiter>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageWaitError {
    DuplicateWaiter,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageServiceWork {
    Completion(PageCompletionRoute),
    Submission(PageIoRequest),
    BackendResume {
        page_request: PageIoRequest,
        resume: BackendPlanResume,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageServiceBackendOutcome {
    QueuedPageCompletions {
        queued: usize,
        wake: Option<PageServiceWake>,
    },
    BlockBios(BioPlanList),
    BlockGraph(BackendBioGraph),
    MetadataFirst {
        request: BackendPageRequest,
        bios: BioPlanList,
        resume: PagerResumeToken,
    },
    Yield(WaitSourceId),
    Err(Errno),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageServiceBackendSubmitOutcome {
    QueuedPageCompletions {
        queued: usize,
        wake: Option<PageServiceWake>,
    },
    BlockBiosQueued {
        request: PageIoRequest,
        submitted: Vec<SubmitOutcome>,
    },
    BlockGraphQueued {
        request: PageIoRequest,
        submitted: Vec<SubmitOutcome>,
    },
    MetadataFirstQueued {
        request: PageIoRequest,
        backend_request: BackendPageRequest,
        submitted: Vec<SubmitOutcome>,
        resume: PagerResumeToken,
    },
    Yield(WaitSourceId),
    Err {
        request: PageIoRequest,
        errno: Errno,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageServiceBackendSubmitError {
    BlockQueue(QueueError),
    Graph(BackendGraphSchedulerError),
    DuplicateGraph(PageIoRequestId),
    UnknownL6Action(PageL6ActionId),
}

impl From<QueueError> for PageServiceBackendSubmitError {
    fn from(error: QueueError) -> Self {
        Self::BlockQueue(error)
    }
}

impl From<BackendGraphSchedulerError> for PageServiceBackendSubmitError {
    fn from(error: BackendGraphSchedulerError) -> Self {
        Self::Graph(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageServiceTurn {
    Work(Vec<PageServiceWork>),
    Sleep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageServiceNext {
    Runnable,
    Sleeping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageServiceWake {
    Wake,
    AlreadyRunnable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageServiceSubmit {
    pub id: PageIoRequestId,
    pub wake: PageServiceWake,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageServiceBlockCompletionOutcome {
    pub queued: usize,
    pub wake: Option<PageServiceWake>,
    pub block_submitted: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageServiceBlockCompletionPrepared {
    pub(crate) outcome: PageServiceBlockCompletionOutcome,
    pub(crate) actions: Vec<PageL6Action>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PageServiceBackendPrepared {
    Local(PageServiceBackendSubmitOutcome),
    Submit(PageL6Action),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PageServiceL6Applied {
    pub(crate) outcome: PageServiceBackendSubmitOutcome,
    /// A submission failure is synchronous only when L6 accepted no BIO.
    ///
    /// For an accepted prefix, the remaining admission error is retained by
    /// `PageService` (or the metadata/graph continuation) and is reported
    /// after every accepted request reaches a terminal route. Exposing that
    /// sticky error here would make PageBacked roll back the owner while the
    /// accepted prefix still owns it.
    pub(crate) failure: Option<PageL6SubmitFailure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageServiceBlockCompletionError {
    Tracker(BlockPageRequestTrackerError),
    Completion(BlockPageCompletionError),
}

impl From<BlockPageRequestTrackerError> for PageServiceBlockCompletionError {
    fn from(error: BlockPageRequestTrackerError) -> Self {
        Self::Tracker(error)
    }
}

impl From<BlockPageCompletionError> for PageServiceBlockCompletionError {
    fn from(error: BlockPageCompletionError) -> Self {
        Self::Completion(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageServiceTaggedBlockCompletionError {
    Block(BlockCompletionError),
    Page(PageServiceBlockCompletionError),
    Graph(BackendGraphSchedulerError),
    ExternalCompletion,
}

impl From<BlockCompletionError> for PageServiceTaggedBlockCompletionError {
    fn from(error: BlockCompletionError) -> Self {
        Self::Block(error)
    }
}

impl From<PageServiceBlockCompletionError> for PageServiceTaggedBlockCompletionError {
    fn from(error: PageServiceBlockCompletionError) -> Self {
        Self::Page(error)
    }
}

impl From<BackendGraphSchedulerError> for PageServiceTaggedBlockCompletionError {
    fn from(error: BackendGraphSchedulerError) -> Self {
        Self::Graph(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageServiceStep {
    pub turn: PageServiceTurn,
    pub next: PageServiceNext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageServiceDriven {
    pub step: PageServiceStep,
    pub kicks: usize,
}

pub trait PageServiceBackendContext {
    fn plan_submission(&self, request: PageIoRequest) -> Option<BackendPlan>;

    fn prepare_submission_with_source_and_target(
        &self,
        _request: &PageIoRequest,
        _source: &IoDataSource,
        _target: &IoDataTarget,
        _guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        Ok(())
    }

    fn plan_submission_with_source(
        &self,
        request: PageIoRequest,
        _source: IoDataSource,
    ) -> Option<BackendPlan> {
        self.plan_submission(request)
    }

    fn plan_submission_with_source_and_target(
        &self,
        request: PageIoRequest,
        source: IoDataSource,
        _target: IoDataTarget,
    ) -> Option<BackendPlan> {
        self.plan_submission_with_source(request, source)
    }

    fn resume_submission(&self, _resume: BackendPlanResume) -> Option<BackendPlan> {
        None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PageServiceDrivenWork {
    Completion(PageCompletionRoute),
    BackendSubmission(PageServiceBackendSubmitOutcome),
    /// Owned L6 accepted no BIO because its queue/depth was transiently full.
    /// The initial L4 request, owner bundle, and waiter rows remain live so a
    /// later service turn can safely plan that request again.
    BackendAdmissionRetry {
        request: PageIoRequest,
        failure: PageL6SubmitFailure,
    },
    BackendSubmitError(PageServiceBackendSubmitError),
    UnplannedSubmission(PageIoRequest),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageServiceBackendDriven {
    pub work: Vec<PageServiceDrivenWork>,
    pub next: PageServiceNext,
    pub kicks: usize,
    /// Backend readiness that must be observed before retrying a requeued
    /// submission. Keeping this separate from `next` prevents a runnable page
    /// queue from degenerating into a self-kick loop while its backend owner
    /// is contended.
    pub backend_wait: Option<WaitSourceId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageServiceDriver {
    budget: ServiceBudget,
}

impl PageServiceDriver {
    pub const fn new(budget: ServiceBudget) -> Self {
        Self { budget }
    }

    pub fn drive_once<F>(&mut self, service: &mut PageService, mut kick: F) -> PageServiceDriven
    where
        F: FnMut(ServiceKick) -> bool,
    {
        let step = service.drive_turn(self.budget);
        let kicks = if step.next == PageServiceNext::Runnable {
            usize::from(kick(ServiceKick::new(IoServiceKind::Page)))
        } else {
            0
        };
        PageServiceDriven { step, kicks }
    }

    pub fn drive_once_with_backend<C, F>(
        &mut self,
        service: &mut PageService,
        context: &C,
        block_queue: &mut BlockQueue,
        mut kick: F,
    ) -> PageServiceBackendDriven
    where
        C: PageServiceBackendContext + ?Sized,
        F: FnMut(ServiceKick) -> bool,
    {
        let step = service.drive_turn(self.budget);
        let mut work = Vec::new();

        if let PageServiceTurn::Work(items) = step.turn {
            for item in items {
                match item {
                    PageServiceWork::Completion(route) => {
                        work.push(PageServiceDrivenWork::Completion(route));
                    }
                    PageServiceWork::Submission(request) => {
                        let Some(plan) = context.plan_submission(request.clone()) else {
                            work.push(PageServiceDrivenWork::UnplannedSubmission(request));
                            continue;
                        };
                        let dispatch = dispatch_backend_plan(plan);
                        let outcome = service.consume_backend_dispatch(dispatch);
                        match service.queue_backend_outcome(outcome, block_queue, request) {
                            Ok(outcome) => {
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome));
                            }
                            Err(error) => {
                                work.push(PageServiceDrivenWork::BackendSubmitError(error));
                            }
                        }
                    }
                    PageServiceWork::BackendResume {
                        page_request,
                        resume,
                    } => {
                        let Some(plan) = context.resume_submission(resume) else {
                            work.push(PageServiceDrivenWork::UnplannedSubmission(page_request));
                            continue;
                        };
                        let dispatch = dispatch_backend_plan(plan);
                        let outcome = service.consume_backend_dispatch(dispatch);
                        match service.queue_backend_outcome(outcome, block_queue, page_request) {
                            Ok(outcome) => {
                                work.push(PageServiceDrivenWork::BackendSubmission(outcome));
                            }
                            Err(error) => {
                                work.push(PageServiceDrivenWork::BackendSubmitError(error));
                            }
                        }
                    }
                }
            }
        }

        let next = if service.has_work() {
            PageServiceNext::Runnable
        } else {
            PageServiceNext::Sleeping
        };
        service.next = next;
        let kicks = if next == PageServiceNext::Runnable {
            usize::from(kick(ServiceKick::new(IoServiceKind::Page)))
        } else {
            0
        };

        PageServiceBackendDriven {
            work,
            next,
            kicks,
            backend_wait: None,
        }
    }
}

#[derive(Debug)]
pub struct PageService {
    submissions: PageRequestQueue,
    completions: VecDeque<PageIoCompletionEntry>,
    backend_resumes: VecDeque<(PageIoRequest, BackendPlanResume)>,
    metadata: BTreeMap<BlockRequestId, Vec<MetadataContinuation>>,
    graphs: BTreeMap<PageIoRequestId, BackendGraphExecution>,
    pending_l6: BTreeMap<PageL6ActionId, PendingPageL6>,
    partial_l6: BTreeMap<PageIoRequestId, PartialL6Admission>,
    l6_receipt_errors: usize,
    next_l6_action: u64,
    waiters: BTreeMap<PageIoRequestId, Vec<PageWaiter>>,
    next: PageServiceNext,
}

/// Allocation-free counters used by the kernel's one-shot stall dump.
///
/// These are observations only: no correctness path branches on them and the
/// snapshot does not retain requests or page-cache leases.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PageServiceDiagnosticSnapshot {
    pub(crate) submissions: usize,
    pub(crate) completions: usize,
    pub(crate) backend_resumes: usize,
    pub(crate) metadata: usize,
    pub(crate) graphs: usize,
    pub(crate) pending_l6: usize,
    pub(crate) partial_l6: usize,
    pub(crate) l6_receipt_errors: usize,
    pub(crate) waiter_requests: usize,
    pub(crate) waiters: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MetadataContinuation {
    page_request: PageIoRequest,
    request: BackendPageRequest,
    token: PagerResumeToken,
    pending: Vec<BlockRequestId>,
    completions: Vec<BackendBioCompletion>,
    admission_errno: Option<Errno>,
}

#[derive(Debug)]
struct BackendGraphExecution {
    request: PageIoRequest,
    scheduler: BackendGraphScheduler,
}

#[derive(Debug)]
struct PartialL6Admission {
    request: PageIoRequest,
    pending: BTreeSet<BlockRequestId>,
    errno: Errno,
}

#[derive(Debug)]
enum PendingPageL6 {
    PageBios {
        request: PageIoRequest,
        expected: usize,
    },
    MetadataFirst {
        page_request: PageIoRequest,
        backend_request: BackendPageRequest,
        resume: PagerResumeToken,
        expected: usize,
    },
    GraphReady {
        graph: PageIoRequestId,
        nodes: Vec<BackendBioNodeId>,
    },
}

impl PageService {
    pub fn new(max_pending_submissions: usize) -> Self {
        Self {
            submissions: PageRequestQueue::new(max_pending_submissions),
            completions: VecDeque::new(),
            backend_resumes: VecDeque::new(),
            metadata: BTreeMap::new(),
            graphs: BTreeMap::new(),
            pending_l6: BTreeMap::new(),
            partial_l6: BTreeMap::new(),
            l6_receipt_errors: 0,
            next_l6_action: 1,
            waiters: BTreeMap::new(),
            next: PageServiceNext::Sleeping,
        }
    }

    pub(crate) fn has_immediate_work(&self, include_submissions: bool) -> bool {
        !self.completions.is_empty()
            || !self.backend_resumes.is_empty()
            || (include_submissions && !self.submissions.is_empty())
    }

    pub fn submit(
        &mut self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
        priority: PageIoPriority,
        flags: PageIoFlags,
        generation_hint: Option<crate::io_manager::page::PageGeneration>,
    ) -> Result<PageIoRequestId, PageQueueError> {
        self.submit_with_wake(pc, range, op, priority, flags, generation_hint)
            .map(|outcome| outcome.id)
    }

    pub fn submit_with_wake(
        &mut self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
        priority: PageIoPriority,
        flags: PageIoFlags,
        generation_hint: Option<crate::io_manager::page::PageGeneration>,
    ) -> Result<PageServiceSubmit, PageQueueError> {
        let id = self
            .submissions
            .submit(pc, range, op, priority, flags, generation_hint)?;
        let wake = self.note_work_ready();
        Ok(PageServiceSubmit { id, wake })
    }

    pub fn reserve_background_request(
        &mut self,
        pc: PageContainerKey,
        range: PageIoRange,
    ) -> Result<PageIoRequest, PageQueueError> {
        let id = self.submissions.submit(
            pc,
            range,
            PageIoOp::Checkpoint,
            PageIoPriority::BackgroundWriteback,
            PageIoFlags::EMPTY,
            None,
        )?;
        Ok(self
            .submissions
            .remove(id)
            .expect("newly reserved background request"))
    }

    pub fn submit_with_kick<F>(
        &mut self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
        priority: PageIoPriority,
        flags: PageIoFlags,
        generation_hint: Option<crate::io_manager::page::PageGeneration>,
        mut kick: F,
    ) -> Result<PageServiceSubmit, PageQueueError>
    where
        F: FnMut(ServiceKick) -> bool,
    {
        let outcome = self.submit_with_wake(pc, range, op, priority, flags, generation_hint)?;
        self.post_kick_if_needed(outcome.wake, &mut kick);
        Ok(outcome)
    }

    pub fn push_completion(&mut self, completion: PageIoCompletion) {
        let _ = self.push_completion_with_wake(completion);
    }

    pub fn push_completion_with_wake(&mut self, completion: PageIoCompletion) -> PageServiceWake {
        self.completions
            .push_back(PageIoCompletionEntry::new(completion, None));
        self.note_work_ready()
    }

    pub fn push_page_completion(&mut self, completion: PageCompletion) -> PageServiceWake {
        self.completions
            .push_back(PageIoCompletionEntry::from_page_completion(completion));
        self.note_work_ready()
    }

    pub fn push_block_page_completion(
        &mut self,
        completion: BlockPageCompletion,
    ) -> Result<PageServiceWake, BlockPageCompletionError> {
        let completion = completion.into_page_completion()?;
        self.completions.push_back(completion);
        Ok(self.note_work_ready())
    }

    pub fn push_tracked_block_completion<F>(
        &mut self,
        tracker: &mut BlockPageRequestTracker,
        completion: BlockCompletion,
        frame_for: F,
    ) -> Result<PageServiceBlockCompletionOutcome, PageServiceBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let completions = tracker.complete(completion)?;
        let (completions, partial_queued, partial_wake) =
            self.consume_partial_l6_routes(completions);
        let mut outcome = self
            .push_block_page_completions(completions, frame_for)
            .map_err(PageServiceBlockCompletionError::from)?;
        outcome.queued += partial_queued;
        outcome.wake = merge_page_wake(outcome.wake, partial_wake);
        Ok(outcome)
    }

    /// Route page completions already removed from the owning L6 tracker.
    ///
    /// L4 converts these opaque routes into page completion work, but never
    /// retains a second page-request tracker. That makes a consumed L6 route
    /// incapable of being replayed against a later page request.
    fn push_block_page_completions<F>(
        &mut self,
        completions: Vec<BlockPageCompletion>,
        mut frame_for: F,
    ) -> Result<PageServiceBlockCompletionOutcome, BlockPageCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let mut entries = Vec::new();
        for completion in completions {
            let completion = match frame_for(&completion) {
                Some(frame) => completion.with_frame(frame),
                None => completion,
            };
            entries.push(completion.into_page_completion()?);
        }
        let queued = entries.len();
        for entry in entries {
            self.completions.push_back(entry);
        }
        let wake = (queued != 0).then(|| self.note_work_ready());
        Ok(PageServiceBlockCompletionOutcome {
            queued,
            wake,
            block_submitted: 0,
        })
    }

    pub fn push_tagged_block_completion<F>(
        &mut self,
        tags: &mut BlockTagTable,
        depth: &mut QueueDepth,
        tracker: &mut BlockPageRequestTracker,
        tag: BlockTag,
        result: Result<(), Errno>,
        frame_for: F,
    ) -> Result<PageServiceBlockCompletionOutcome, PageServiceTaggedBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let completion = tags.complete(depth, tag, result)?;
        let (metadata_handled, metadata_wake) =
            self.complete_metadata_block(completion.id, completion.result);
        if metadata_handled && !tracker.contains(completion.id) {
            return Ok(PageServiceBlockCompletionOutcome {
                queued: 0,
                wake: metadata_wake,
                block_submitted: 0,
            });
        }
        let mut outcome = self.push_tracked_block_completion(tracker, completion, frame_for)?;
        outcome.wake = metadata_wake.or(outcome.wake);
        Ok(outcome)
    }

    pub fn push_tagged_block_completion_with_graphs<F>(
        &mut self,
        tags: &mut BlockTagTable,
        depth: &mut QueueDepth,
        tracker: &mut BlockPageRequestTracker,
        block_queue: &mut BlockQueue,
        tag: BlockTag,
        result: Result<(), Errno>,
        frame_for: F,
    ) -> Result<PageServiceBlockCompletionOutcome, PageServiceTaggedBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let completion = tags.complete(depth, tag, result)?;
        self.push_completed_block_completion_with_graphs(
            tracker,
            block_queue,
            completion,
            false,
            frame_for,
        )
    }

    /// Route a block completion whose tag/depth ownership was consumed by an
    /// adjacent L4 client. This keeps page, graph, metadata, and direct-I/O
    /// users on one L6 queue without making those users masquerade as pages.
    pub fn push_completed_block_completion_with_graphs<F>(
        &mut self,
        tracker: &mut BlockPageRequestTracker,
        block_queue: &mut BlockQueue,
        completion: BlockCompletion,
        allow_external_completion: bool,
        frame_for: F,
    ) -> Result<PageServiceBlockCompletionOutcome, PageServiceTaggedBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let mut prepared = self.prepare_completed_block_completion_with_graphs(
            tracker,
            completion,
            allow_external_completion,
            frame_for,
        )?;
        for action in prepared.actions {
            let receipt = submit_l6_action_to_queue(action, block_queue);
            prepared.outcome.block_submitted += receipt.submitted.len();
            let applied = self
                .apply_l6_receipt(receipt)
                .map_err(backend_submit_error_to_tagged)?;
            if let PageServiceBackendSubmitOutcome::QueuedPageCompletions { queued, wake } =
                applied.outcome
            {
                prepared.outcome.queued += queued;
                prepared.outcome.wake = merge_page_wake(prepared.outcome.wake, wake);
            }
        }
        Ok(prepared.outcome)
    }

    /// Complete L4 graph and metadata state using routes that L6 removed
    /// together with the completed tag. L4 must not reconstruct those routes
    /// from its own tracker.
    pub(crate) fn prepare_block_completion_routes<F>(
        &mut self,
        completion: BlockCompletion,
        page_routes: Vec<BlockPageCompletion>,
        allow_external_completion: bool,
        frame_for: F,
    ) -> Result<PageServiceBlockCompletionPrepared, PageServiceTaggedBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let graph = self.complete_graph_block(&completion)?;
        let (metadata_handled, metadata_wake) =
            self.complete_metadata_block(completion.id, completion.result);
        let graph_handled = graph.is_some();
        if page_routes.is_empty()
            && !metadata_handled
            && !graph_handled
            && !allow_external_completion
        {
            return Err(PageServiceBlockCompletionError::Tracker(
                BlockPageRequestTrackerError::UnknownBlockRequest(completion.id),
            )
            .into());
        }

        let (page_routes, partial_queued, partial_wake) =
            self.consume_partial_l6_routes(page_routes);
        let mut outcome = self
            .push_block_page_completions(page_routes, frame_for)
            .map_err(PageServiceBlockCompletionError::from)?;
        outcome.queued += partial_queued;
        outcome.wake = merge_page_wake(outcome.wake, partial_wake);
        let actions = if let Some((queued, graph_wake, actions)) = graph {
            outcome.queued += queued;
            outcome.wake =
                merge_page_wake(merge_page_wake(metadata_wake, graph_wake), outcome.wake);
            actions
        } else {
            outcome.wake = merge_page_wake(metadata_wake, outcome.wake);
            Vec::new()
        };
        Ok(PageServiceBlockCompletionPrepared { outcome, actions })
    }

    fn consume_partial_l6_routes(
        &mut self,
        routes: Vec<BlockPageCompletion>,
    ) -> (Vec<BlockPageCompletion>, usize, Option<PageServiceWake>) {
        let mut passthrough = Vec::new();
        let mut completed = alloc::collections::BTreeSet::new();
        let mut queued = 0usize;
        let mut wake = None;
        for route in routes {
            let request_id = route.request().id;
            if completed.contains(&request_id) {
                continue;
            }
            let Some(partial) = self.partial_l6.get_mut(&request_id) else {
                passthrough.push(route);
                continue;
            };
            let block_id = route.block_completion().id;
            partial.pending.remove(&block_id);
            if partial.pending.is_empty() {
                let partial = self
                    .partial_l6
                    .remove(&request_id)
                    .expect("partial admission remains registered");
                wake = merge_page_wake(
                    wake,
                    Some(self.queue_page_error(&partial.request, partial.errno)),
                );
                queued = queued.saturating_add(1);
                completed.insert(request_id);
            }
        }
        (passthrough, queued, wake)
    }

    pub(crate) fn prepare_completed_block_completion_with_graphs<F>(
        &mut self,
        tracker: &mut BlockPageRequestTracker,
        completion: BlockCompletion,
        allow_external_completion: bool,
        frame_for: F,
    ) -> Result<PageServiceBlockCompletionPrepared, PageServiceTaggedBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let graph = self.complete_graph_block(&completion)?;
        let (metadata_handled, metadata_wake) =
            self.complete_metadata_block(completion.id, completion.result);
        let tracked = tracker.contains(completion.id);
        if !tracked {
            if !metadata_handled && graph.is_none() && !allow_external_completion {
                return Err(PageServiceBlockCompletionError::Tracker(
                    BlockPageRequestTrackerError::UnknownBlockRequest(completion.id),
                )
                .into());
            }
            let (queued, graph_wake, actions) = graph.unwrap_or((0, None, Vec::new()));
            return Ok(PageServiceBlockCompletionPrepared {
                outcome: PageServiceBlockCompletionOutcome {
                    queued,
                    wake: merge_page_wake(metadata_wake, graph_wake),
                    block_submitted: 0,
                },
                actions,
            });
        }
        let mut outcome = self.push_tracked_block_completion(tracker, completion, frame_for)?;
        let actions = if let Some((queued, graph_wake, actions)) = graph {
            outcome.queued += queued;
            outcome.wake =
                merge_page_wake(merge_page_wake(metadata_wake, graph_wake), outcome.wake);
            actions
        } else {
            outcome.wake = merge_page_wake(metadata_wake, outcome.wake);
            Vec::new()
        };
        Ok(PageServiceBlockCompletionPrepared { outcome, actions })
    }

    fn complete_graph_block(
        &mut self,
        completion: &BlockCompletion,
    ) -> Result<
        Option<(usize, Option<PageServiceWake>, Vec<PageL6Action>)>,
        BackendGraphSchedulerError,
    > {
        let graph_ids = self
            .graphs
            .iter()
            .filter_map(|(id, execution)| {
                execution
                    .scheduler
                    .handles_request(completion.id)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        if graph_ids.is_empty() {
            return Ok(None);
        }

        let mut queued = 0usize;
        let mut wake = None;
        let mut actions = Vec::new();
        for graph_id in graph_ids {
            let stage = self
                .graphs
                .get_mut(&graph_id)
                .expect("graph id came from the registry")
                .scheduler
                .complete(completion.id, completion.result)?;
            match stage {
                BackendGraphStage::Ready { nodes, bios } => {
                    let id = self.allocate_l6_action();
                    self.pending_l6.insert(
                        id,
                        PendingPageL6::GraphReady {
                            graph: graph_id,
                            nodes,
                        },
                    );
                    actions.push(PageL6Action::GraphReady { id, bios });
                }
                BackendGraphStage::Waiting => {}
                BackendGraphStage::Complete(result) => {
                    let execution = self
                        .graphs
                        .remove(&graph_id)
                        .expect("completed graph remains registered");
                    let graph_wake =
                        self.queue_graph_terminal_completion(execution.request, result);
                    wake = Some(
                        if wake == Some(PageServiceWake::Wake)
                            || graph_wake == PageServiceWake::Wake
                        {
                            PageServiceWake::Wake
                        } else {
                            PageServiceWake::AlreadyRunnable
                        },
                    );
                    queued += 1;
                }
            }
        }
        Ok(Some((queued, wake, actions)))
    }

    pub fn register_metadata_continuation(
        &mut self,
        page_request: PageIoRequest,
        request: BackendPageRequest,
        token: PagerResumeToken,
        submitted: &[SubmitOutcome],
    ) {
        self.register_metadata_continuation_with_error(
            page_request,
            request,
            token,
            submitted,
            None,
        );
    }

    fn register_metadata_continuation_with_error(
        &mut self,
        page_request: PageIoRequest,
        request: BackendPageRequest,
        token: PagerResumeToken,
        submitted: &[SubmitOutcome],
        admission_errno: Option<Errno>,
    ) {
        let mut pending = submitted
            .iter()
            .map(|outcome| match outcome {
                SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => *id,
            })
            .collect::<Vec<_>>();
        pending.sort();
        pending.dedup();
        if pending.is_empty() {
            self.queue_metadata_error(page_request, admission_errno.unwrap_or(Errno::ENOSYS));
            self.note_work_ready();
            return;
        }
        for id in &pending {
            self.metadata
                .entry(*id)
                .or_default()
                .push(MetadataContinuation {
                    page_request: page_request.clone(),
                    request: request.clone(),
                    token,
                    pending: pending.clone(),
                    completions: Vec::new(),
                    admission_errno,
                });
        }
    }

    pub fn register_metadata_submission(
        &mut self,
        page_request: PageIoRequest,
        backend_request: BackendPageRequest,
        resume: PagerResumeToken,
        submitted: &[SubmitOutcome],
    ) {
        self.register_metadata_continuation_with_error(
            page_request,
            backend_request,
            resume,
            submitted,
            None,
        );
    }

    fn register_metadata_submission_with_error(
        &mut self,
        page_request: PageIoRequest,
        backend_request: BackendPageRequest,
        resume: PagerResumeToken,
        submitted: &[SubmitOutcome],
        admission_errno: Option<Errno>,
    ) {
        self.register_metadata_continuation_with_error(
            page_request,
            backend_request,
            resume,
            submitted,
            admission_errno,
        );
    }

    fn complete_metadata_block(
        &mut self,
        id: BlockRequestId,
        result: Result<(), Errno>,
    ) -> (bool, Option<PageServiceWake>) {
        let Some(continuations) = self.metadata.remove(&id) else {
            return (false, None);
        };
        let mut routed = false;
        for mut continuation in continuations {
            continuation.completions.push(BackendBioCompletion::new(
                BackendBioNodeId::new(id.raw()),
                result,
            ));
            continuation.pending.retain(|pending| *pending != id);
            if continuation.pending.is_empty() {
                if let Some(errno) = continuation.admission_errno.or_else(|| {
                    continuation
                        .completions
                        .iter()
                        .find_map(|completion| completion.result.err())
                }) {
                    self.queue_metadata_error(continuation.page_request, errno);
                } else {
                    let resume = BackendPlanResume::with_request(
                        continuation.token,
                        continuation.completions,
                        continuation.request.clone(),
                    );
                    self.backend_resumes
                        .push_back((continuation.page_request, resume));
                }
                routed = true;
            } else {
                for pending in &continuation.pending {
                    self.metadata
                        .entry(*pending)
                        .or_default()
                        .push(continuation.clone());
                }
            }
        }
        let wake = routed.then(|| self.note_work_ready());
        (true, wake)
    }

    fn queue_metadata_error(&mut self, request: PageIoRequest, errno: Errno) {
        let _ = self.queue_page_error(&request, errno);
    }

    fn queue_page_error(&mut self, request: &PageIoRequest, errno: Errno) -> PageServiceWake {
        let kind = match request.op {
            PageIoOp::Read | PageIoOp::Readahead => {
                crate::io_manager::page::PageIoCompletionKind::ReadInstalled
            }
            PageIoOp::Writeback => crate::io_manager::page::PageIoCompletionKind::WritebackFinished,
            PageIoOp::Fsync | PageIoOp::Checkpoint => {
                crate::io_manager::page::PageIoCompletionKind::Noop
            }
        };
        let wake = self.note_work_ready();
        self.completions.push_back(PageIoCompletionEntry::new(
            PageIoCompletion::new(
                request.id,
                request.range,
                crate::io_manager::page::PageIoResult::Err(errno),
                request
                    .generation_hint
                    .unwrap_or(crate::io_manager::page::PageGeneration::new(0)),
                kind,
            ),
            None,
        ));
        wake
    }

    pub fn push_completion_with_kick<F>(
        &mut self,
        completion: PageIoCompletion,
        mut kick: F,
    ) -> PageServiceWake
    where
        F: FnMut(ServiceKick) -> bool,
    {
        let wake = self.push_completion_with_wake(completion);
        self.post_kick_if_needed(wake, &mut kick);
        wake
    }

    pub fn consume_backend_dispatch(
        &mut self,
        dispatch: BackendDispatch,
    ) -> PageServiceBackendOutcome {
        match dispatch {
            BackendDispatch::PageCompletions(completions) => {
                let mut queued = 0usize;
                for completion in completions.into_vec() {
                    self.completions.push_back(completion);
                    queued += 1;
                }
                let wake = (queued != 0).then(|| self.note_work_ready());
                PageServiceBackendOutcome::QueuedPageCompletions { queued, wake }
            }
            BackendDispatch::BlockBios(bios) => PageServiceBackendOutcome::BlockBios(bios),
            BackendDispatch::BlockGraph(graph) => PageServiceBackendOutcome::BlockGraph(graph),
            BackendDispatch::MetadataFirst {
                request,
                bios,
                resume,
            } => PageServiceBackendOutcome::MetadataFirst {
                request,
                bios,
                resume,
            },
            BackendDispatch::Yield(wait) => PageServiceBackendOutcome::Yield(wait),
            BackendDispatch::Err(errno) => PageServiceBackendOutcome::Err(errno),
        }
    }

    pub(crate) fn prepare_backend_outcome(
        &mut self,
        outcome: PageServiceBackendOutcome,
        request: PageIoRequest,
    ) -> Result<PageServiceBackendPrepared, PageServiceBackendSubmitError> {
        match outcome {
            PageServiceBackendOutcome::QueuedPageCompletions { queued, wake } => {
                Ok(PageServiceBackendPrepared::Local(
                    PageServiceBackendSubmitOutcome::QueuedPageCompletions { queued, wake },
                ))
            }
            PageServiceBackendOutcome::BlockBios(bios) => {
                let expected = bios.as_slice().len();
                let id = self.allocate_l6_action();
                self.pending_l6
                    .insert(id, PendingPageL6::PageBios { request, expected });
                Ok(PageServiceBackendPrepared::Submit(PageL6Action::PageBios {
                    id,
                    bios,
                }))
            }
            PageServiceBackendOutcome::MetadataFirst {
                request: backend_request,
                bios,
                resume,
            } => {
                let expected = bios.as_slice().len();
                let id = self.allocate_l6_action();
                self.pending_l6.insert(
                    id,
                    PendingPageL6::MetadataFirst {
                        page_request: request,
                        backend_request,
                        resume,
                        expected,
                    },
                );
                Ok(PageServiceBackendPrepared::Submit(
                    PageL6Action::MetadataFirst { id, bios },
                ))
            }
            PageServiceBackendOutcome::BlockGraph(graph) => {
                if self.graphs.contains_key(&request.id) {
                    return Err(PageServiceBackendSubmitError::DuplicateGraph(request.id));
                }
                let mut scheduler = BackendGraphScheduler::new(graph);
                match scheduler.stage_ready() {
                    BackendGraphStage::Ready { nodes, bios } => {
                        let graph = request.id;
                        self.graphs
                            .insert(graph, BackendGraphExecution { request, scheduler });
                        let id = self.allocate_l6_action();
                        self.pending_l6
                            .insert(id, PendingPageL6::GraphReady { graph, nodes });
                        Ok(PageServiceBackendPrepared::Submit(
                            PageL6Action::GraphReady { id, bios },
                        ))
                    }
                    BackendGraphStage::Waiting => {
                        self.graphs.insert(
                            request.id,
                            BackendGraphExecution {
                                request: request.clone(),
                                scheduler,
                            },
                        );
                        Ok(PageServiceBackendPrepared::Local(
                            PageServiceBackendSubmitOutcome::BlockGraphQueued {
                                request,
                                submitted: Vec::new(),
                            },
                        ))
                    }
                    BackendGraphStage::Complete(result) => {
                        let wake = self.queue_graph_terminal_completion(request, result);
                        Ok(PageServiceBackendPrepared::Local(
                            PageServiceBackendSubmitOutcome::QueuedPageCompletions {
                                queued: 1,
                                wake: Some(wake),
                            },
                        ))
                    }
                }
            }
            PageServiceBackendOutcome::Yield(wait) => Ok(PageServiceBackendPrepared::Local(
                PageServiceBackendSubmitOutcome::Yield(wait),
            )),
            PageServiceBackendOutcome::Err(errno) => Ok(PageServiceBackendPrepared::Local(
                PageServiceBackendSubmitOutcome::Err { request, errno },
            )),
        }
    }

    pub(crate) fn apply_l6_receipt(
        &mut self,
        receipt: PageL6Receipt,
    ) -> Result<PageServiceL6Applied, PageServiceBackendSubmitError> {
        let Some(pending) = self.pending_l6.get(&receipt.id) else {
            self.l6_receipt_errors = self.l6_receipt_errors.saturating_add(1);
            return Err(PageServiceBackendSubmitError::UnknownL6Action(receipt.id));
        };
        let expected = match pending {
            PendingPageL6::PageBios { expected, .. }
            | PendingPageL6::MetadataFirst { expected, .. } => *expected,
            PendingPageL6::GraphReady { nodes, .. } => nodes.len(),
        };
        if receipt.submitted.len() > expected
            || receipt
                .failure
                .is_some_and(|failure| failure.failed_index != receipt.submitted.len())
            || (receipt.failure.is_none() && receipt.submitted.len() != expected)
        {
            self.l6_receipt_errors = self.l6_receipt_errors.saturating_add(1);
            return Err(PageServiceBackendSubmitError::Graph(
                BackendGraphSchedulerError::InvalidSubmissionReceipt,
            ));
        }

        let pending = self
            .pending_l6
            .remove(&receipt.id)
            .expect("pending action validated above");
        let accepted_none = receipt.submitted.is_empty();
        let failure = receipt.failure;
        let outcome = match pending {
            PendingPageL6::PageBios { request, .. } => {
                match (failure, receipt.submitted.is_empty()) {
                    (Some(failure), true) => PageServiceBackendSubmitOutcome::Err {
                        request,
                        errno: queue_error_errno(failure.error),
                    },
                    (Some(failure), false) => {
                        let pending = receipt
                            .submitted
                            .iter()
                            .map(|outcome| match outcome {
                                SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => *id,
                            })
                            .collect();
                        self.partial_l6.insert(
                            request.id,
                            PartialL6Admission {
                                request: request.clone(),
                                pending,
                                errno: queue_error_errno(failure.error),
                            },
                        );
                        PageServiceBackendSubmitOutcome::BlockBiosQueued {
                            request,
                            submitted: receipt.submitted,
                        }
                    }
                    (None, _) => PageServiceBackendSubmitOutcome::BlockBiosQueued {
                        request,
                        submitted: receipt.submitted,
                    },
                }
            }
            PendingPageL6::MetadataFirst {
                page_request,
                backend_request,
                resume,
                ..
            } => {
                if let Some(failure) = failure.filter(|_| receipt.submitted.is_empty()) {
                    return Ok(PageServiceL6Applied {
                        outcome: PageServiceBackendSubmitOutcome::Err {
                            request: page_request,
                            errno: queue_error_errno(failure.error),
                        },
                        failure: Some(failure),
                    });
                }
                self.register_metadata_submission_with_error(
                    page_request.clone(),
                    backend_request.clone(),
                    resume,
                    &receipt.submitted,
                    failure.map(|failure| queue_error_errno(failure.error)),
                );
                PageServiceBackendSubmitOutcome::MetadataFirstQueued {
                    request: page_request,
                    backend_request,
                    submitted: receipt.submitted,
                    resume,
                }
            }
            PendingPageL6::GraphReady { graph, nodes } => {
                if let Some(failure) = failure.filter(|_| receipt.submitted.is_empty()) {
                    let execution = self
                        .graphs
                        .remove(&graph)
                        .expect("pending graph action retains its execution");
                    return Ok(PageServiceL6Applied {
                        outcome: PageServiceBackendSubmitOutcome::Err {
                            request: execution.request,
                            errno: queue_error_errno(failure.error),
                        },
                        failure: Some(failure),
                    });
                }
                let execution = self
                    .graphs
                    .get_mut(&graph)
                    .expect("pending graph action retains its execution");
                let failure_errno = failure.map(|failure| queue_error_errno(failure.error));
                let advance = match execution.scheduler.apply_submission_receipt(
                    &nodes,
                    &receipt.submitted,
                    failure_errno,
                ) {
                    Ok(advance) => advance,
                    Err(error) => {
                        self.l6_receipt_errors = self.l6_receipt_errors.saturating_add(1);
                        return Err(error.into());
                    }
                };
                match advance {
                    BackendGraphAdvance::Pending { submitted } => {
                        PageServiceBackendSubmitOutcome::BlockGraphQueued {
                            request: execution.request.clone(),
                            submitted,
                        }
                    }
                    BackendGraphAdvance::Complete(result) => {
                        let execution = self
                            .graphs
                            .remove(&graph)
                            .expect("terminal graph remains registered");
                        let wake = self.queue_graph_terminal_completion(execution.request, result);
                        PageServiceBackendSubmitOutcome::QueuedPageCompletions {
                            queued: 1,
                            wake: Some(wake),
                        }
                    }
                }
            }
        };
        // A partial receipt is asynchronous from the owner's perspective:
        // accepted BIOs still have live completion routes. Keep the admission
        // error in `partial_l6`/continuations/scheduler and expose it only for
        // the zero-accepted case, where callers may synchronously roll back.
        Ok(PageServiceL6Applied {
            outcome,
            failure: failure.filter(|_| accepted_none),
        })
    }

    fn allocate_l6_action(&mut self) -> PageL6ActionId {
        let id = PageL6ActionId::new(self.next_l6_action);
        self.next_l6_action = self.next_l6_action.wrapping_add(1).max(1);
        id
    }

    pub fn consume_backend_submission<P>(
        &mut self,
        object: FsObjectKey,
        request: PageIoRequest,
        planner: &P,
        block_queue: &mut BlockQueue,
    ) -> Result<PageServiceBackendSubmitOutcome, PageServiceBackendSubmitError>
    where
        P: BackendPlanner + ?Sized,
    {
        let backend_request = BackendPageRequest::from_page_io_request(object, request.clone());
        let plan = plan_backend_request(planner, backend_request);
        let dispatch = dispatch_backend_plan(plan);
        let outcome = self.consume_backend_dispatch(dispatch);
        self.queue_backend_outcome(outcome, block_queue, request)
    }

    pub fn queue_backend_outcome(
        &mut self,
        outcome: PageServiceBackendOutcome,
        block_queue: &mut BlockQueue,
        request: PageIoRequest,
    ) -> Result<PageServiceBackendSubmitOutcome, PageServiceBackendSubmitError> {
        match self.prepare_backend_outcome(outcome, request)? {
            PageServiceBackendPrepared::Local(outcome) => Ok(outcome),
            PageServiceBackendPrepared::Submit(action) => {
                let receipt = submit_l6_action_to_queue(action, block_queue);
                let applied = self.apply_l6_receipt(receipt)?;
                Ok(applied.outcome)
            }
        }
    }

    fn queue_graph_terminal_completion(
        &mut self,
        request: PageIoRequest,
        result: Result<(), Errno>,
    ) -> PageServiceWake {
        let kind = match request.op {
            PageIoOp::Read | PageIoOp::Readahead => {
                crate::io_manager::page::PageIoCompletionKind::ReadInstalled
            }
            PageIoOp::Writeback => crate::io_manager::page::PageIoCompletionKind::WritebackFinished,
            PageIoOp::Fsync | PageIoOp::Checkpoint => {
                crate::io_manager::page::PageIoCompletionKind::Noop
            }
        };
        let result = match result {
            Ok(()) => crate::io_manager::page::PageIoResult::Done,
            Err(errno) => crate::io_manager::page::PageIoResult::Err(errno),
        };
        self.completions.push_back(PageIoCompletionEntry::new(
            PageIoCompletion::new(
                request.id,
                request.range,
                result,
                request
                    .generation_hint
                    .unwrap_or(crate::io_manager::page::PageGeneration::new(0)),
                kind,
            ),
            None,
        ));
        self.note_work_ready()
    }

    pub fn submission_len(&self) -> usize {
        self.submissions.len()
    }

    pub(crate) fn diagnostic_snapshot(&self) -> PageServiceDiagnosticSnapshot {
        PageServiceDiagnosticSnapshot {
            submissions: self.submissions.len(),
            completions: self.completions.len(),
            backend_resumes: self.backend_resumes.len(),
            metadata: self.metadata.len(),
            graphs: self.graphs.len(),
            pending_l6: self.pending_l6.len(),
            partial_l6: self.partial_l6.len(),
            l6_receipt_errors: self.l6_receipt_errors,
            waiter_requests: self.waiters.len(),
            waiters: self.waiters.values().map(Vec::len).sum(),
        }
    }

    pub(crate) fn has_queued_submission(&self, request_id: PageIoRequestId) -> bool {
        self.submissions.contains(request_id)
    }

    pub fn find_submission(
        &self,
        pc: PageContainerKey,
        range: PageIoRange,
        op: PageIoOp,
    ) -> Option<&PageIoRequest> {
        self.submissions.find(pc, range, op)
    }

    pub fn retire_submission(&mut self, request_id: PageIoRequestId) -> Vec<PageWaiter> {
        let _ = self.submissions.remove(request_id);
        self.waiters.remove(&request_id).unwrap_or_default()
    }

    pub(crate) fn requeue_submission(
        &mut self,
        request: PageIoRequest,
    ) -> Result<PageServiceWake, PageQueueError> {
        self.submissions.requeue(request)?;
        Ok(self.note_work_ready())
    }

    pub fn wait_on(
        &mut self,
        request_id: PageIoRequestId,
        waiter: PageWaiter,
    ) -> Result<(), PageWaitError> {
        let waiters = self.waiters.entry(request_id).or_default();
        if waiters.iter().any(|existing| *existing == waiter) {
            return Err(PageWaitError::DuplicateWaiter);
        }
        waiters.push(waiter);
        Ok(())
    }

    pub fn has_waiters(&self, request_id: PageIoRequestId) -> bool {
        self.waiter_count(request_id) != 0
    }

    pub fn waiter_count(&self, request_id: PageIoRequestId) -> usize {
        self.waiters.get(&request_id).map(Vec::len).unwrap_or(0)
    }

    pub fn has_work(&self) -> bool {
        !self.completions.is_empty()
            || !self.backend_resumes.is_empty()
            || !self.submissions.is_empty()
    }

    pub fn drain_turn(&mut self, mut budget: ServiceBudget) -> PageServiceTurn {
        if !self.has_work() {
            return PageServiceTurn::Sleep;
        }

        let mut work = Vec::new();
        while budget.take_one() {
            if let Some(entry) = self.completions.pop_front() {
                let waiters = self
                    .waiters
                    .remove(&entry.completion.id)
                    .unwrap_or_default();
                work.push(PageServiceWork::Completion(PageCompletionRoute {
                    completion: entry.completion,
                    frame: entry.frame,
                    waiters,
                }));
                continue;
            }
            if let Some((page_request, resume)) = self.backend_resumes.pop_front() {
                work.push(PageServiceWork::BackendResume {
                    page_request,
                    resume,
                });
                continue;
            }
            if let Some(request) = self.submissions.pop_next() {
                work.push(PageServiceWork::Submission(request));
                continue;
            }
            break;
        }

        if work.is_empty() {
            PageServiceTurn::Sleep
        } else {
            PageServiceTurn::Work(work)
        }
    }

    pub fn push_metadata_block_completion(
        &mut self,
        id: BlockRequestId,
        result: Result<(), Errno>,
    ) -> bool {
        self.complete_metadata_block(id, result).0
    }

    pub fn drive_turn(&mut self, budget: ServiceBudget) -> PageServiceStep {
        let turn = self.drain_turn(budget);
        let next = if self.has_work() {
            PageServiceNext::Runnable
        } else {
            PageServiceNext::Sleeping
        };
        self.next = next;
        PageServiceStep { turn, next }
    }

    fn note_work_ready(&mut self) -> PageServiceWake {
        match self.next {
            PageServiceNext::Sleeping => {
                self.next = PageServiceNext::Runnable;
                PageServiceWake::Wake
            }
            PageServiceNext::Runnable => PageServiceWake::AlreadyRunnable,
        }
    }

    fn post_kick_if_needed<F>(&self, wake: PageServiceWake, kick: &mut F)
    where
        F: FnMut(ServiceKick) -> bool,
    {
        if wake == PageServiceWake::Wake {
            let _ = kick(ServiceKick::new(IoServiceKind::Page));
        }
    }
}

fn submit_l6_action_to_queue(action: PageL6Action, block_queue: &mut BlockQueue) -> PageL6Receipt {
    let id = action.id();
    let mut submitted = Vec::new();
    let mut failure = None;
    for (index, bio) in action.into_bios().into_vec().into_iter().enumerate() {
        match block_queue.submit(bio) {
            Ok(outcome) => submitted.push(outcome),
            Err(error) => {
                failure = Some(PageL6SubmitFailure {
                    failed_index: index,
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

const fn queue_error_errno(error: QueueError) -> Errno {
    match error {
        QueueError::EmptyRange => Errno::EINVAL,
        QueueError::Full | QueueError::DispatchDepthFull => Errno::EAGAIN,
    }
}

const fn merge_page_wake(
    left: Option<PageServiceWake>,
    right: Option<PageServiceWake>,
) -> Option<PageServiceWake> {
    match (left, right) {
        (Some(PageServiceWake::Wake), _) | (_, Some(PageServiceWake::Wake)) => {
            Some(PageServiceWake::Wake)
        }
        (Some(PageServiceWake::AlreadyRunnable), _)
        | (_, Some(PageServiceWake::AlreadyRunnable)) => Some(PageServiceWake::AlreadyRunnable),
        (None, None) => None,
    }
}

const fn backend_submit_error_to_tagged(
    error: PageServiceBackendSubmitError,
) -> PageServiceTaggedBlockCompletionError {
    match error {
        PageServiceBackendSubmitError::Graph(error) => {
            PageServiceTaggedBlockCompletionError::Graph(error)
        }
        PageServiceBackendSubmitError::BlockQueue(error) => {
            PageServiceTaggedBlockCompletionError::Graph(BackendGraphSchedulerError::Queue(error))
        }
        PageServiceBackendSubmitError::DuplicateGraph(_)
        | PageServiceBackendSubmitError::UnknownL6Action(_) => {
            PageServiceTaggedBlockCompletionError::Graph(
                BackendGraphSchedulerError::InvalidSubmissionReceipt,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::Errno;
    use crate::io_manager::backend::{
        BackendBioDependency, BackendBioGraph, BackendBioNode, BackendBioNodeId, BackendDispatch,
        BackendPageRequest, BackendPlan, BackendPlanner, BioPlanList, FsObjectKey, IoDataSource,
        IoDataTarget, PageCompletion, PageCompletionList, PageFrameRef, PageIoCompletionList,
        PagerResumeToken, WaitSourceId,
    };
    use crate::io_manager::block::{
        BioPlan, BioVec, BlockCompletion, BlockFlags, BlockOp, BlockQueue, BlockRequestId,
        BlockTag, BlockTagTable, DeviceKey, LbaRange, SubmitOutcome,
    };
    use crate::io_manager::page::{
        PageContainerKey, PageGeneration, PageIoCompletion, PageIoCompletionKind, PageIoFlags,
        PageIoOp, PageIoPriority, PageIoRange, PageIoRequestId, PageIoResult,
    };
    use crate::io_manager::runtime::{IoServiceKind, QueueDepth, ServiceBudget, ServiceKick};

    fn demand_request(service: &mut PageService) -> PageIoRequestId {
        service
            .submit(
                PageContainerKey::new(7),
                PageIoRange::new(4, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(11)),
            )
            .expect("submit demand read")
    }

    fn read_completion(id: PageIoRequestId) -> PageIoCompletion {
        PageIoCompletion::new(
            id,
            PageIoRange::new(4, 1),
            PageIoResult::Done,
            PageGeneration::new(11),
            PageIoCompletionKind::ReadInstalled,
        )
    }

    fn read_bio() -> BioPlan {
        BioPlan::new(
            DeviceKey::new(9),
            BlockOp::Read,
            LbaRange::new(64, 2),
            alloc::vec![BioVec::new(88, 0, 8192)],
            BlockFlags::EMPTY,
        )
    }

    fn graph_bio(lba: u64, buffer_key: u64) -> BioPlan {
        BioPlan::new(
            DeviceKey::new(9),
            BlockOp::Read,
            LbaRange::new(lba, 1),
            alloc::vec![BioVec::new(buffer_key, 0, 512)],
            BlockFlags::EMPTY,
        )
    }

    fn join_graph() -> BackendBioGraph {
        BackendBioGraph::new(
            alloc::vec![
                BackendBioNode::new(
                    BackendBioNodeId::new(1),
                    graph_bio(64, 1),
                    IoDataSource::None,
                ),
                BackendBioNode::new(
                    BackendBioNodeId::new(2),
                    graph_bio(72, 2),
                    IoDataSource::None,
                ),
                BackendBioNode::new(
                    BackendBioNodeId::new(3),
                    graph_bio(80, 3),
                    IoDataSource::None,
                ),
            ],
            alloc::vec![
                BackendBioDependency::new(BackendBioNodeId::new(1), BackendBioNodeId::new(3)),
                BackendBioDependency::new(BackendBioNodeId::new(2), BackendBioNodeId::new(3)),
            ],
        )
        .expect("valid join graph")
    }

    fn linear_graph(start_lba: u64) -> BackendBioGraph {
        BackendBioGraph::new(
            alloc::vec![
                BackendBioNode::new(
                    BackendBioNodeId::new(1),
                    graph_bio(start_lba, 1),
                    IoDataSource::None,
                ),
                BackendBioNode::new(
                    BackendBioNodeId::new(2),
                    graph_bio(start_lba + 8, 2),
                    IoDataSource::None,
                ),
            ],
            alloc::vec![BackendBioDependency::new(
                BackendBioNodeId::new(1),
                BackendBioNodeId::new(2),
            )],
        )
        .expect("valid linear graph")
    }

    fn read_block_completion() -> BlockCompletion {
        BlockCompletion {
            tag: BlockTag::new(4),
            id: BlockRequestId::new(12),
            plan: read_bio(),
            result: Ok(()),
        }
    }

    #[test]
    fn metadata_block_completions_resume_only_after_all_bios_finish() {
        let mut service = PageService::new(4);
        let request = BackendPageRequest::new_with_source_and_target(
            FsObjectKey::new(17),
            PageIoRequestId::new(70),
            PageIoRange::new(2, 1),
            PageIoOp::Read,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(9)),
            IoDataSource::None,
            IoDataTarget::None,
        );
        let mut block_queue = BlockQueue::new(4);
        let first = block_queue
            .submit(BioPlan::new(
                DeviceKey::new(9),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(1, 0, 512)],
                BlockFlags::EMPTY,
            ))
            .expect("first metadata bio");
        let second = block_queue
            .submit(BioPlan::new(
                DeviceKey::new(9),
                BlockOp::Read,
                LbaRange::new(80, 1),
                alloc::vec![BioVec::new(2, 0, 512)],
                BlockFlags::EMPTY,
            ))
            .expect("second metadata bio");
        let mut depth = QueueDepth::new(2);
        let mut tags = BlockTagTable::new();
        let first_dispatch = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("first dispatch")
            .expect("first dispatch exists");
        let second_dispatch = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("second dispatch")
            .expect("second dispatch exists");
        let submitted = [first, second];
        let page_request = PageIoRequest::new(
            PageIoRequestId::new(70),
            PageContainerKey::new(17),
            PageIoRange::new(2, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(9)),
        );
        service.register_metadata_continuation(
            page_request.clone(),
            request.clone(),
            PagerResumeToken::new(91),
            &submitted,
        );
        let mut tracker = BlockPageRequestTracker::new();

        let first_outcome = service
            .push_tagged_block_completion(
                &mut tags,
                &mut depth,
                &mut tracker,
                first_dispatch.tag,
                Ok(()),
                |_| None,
            )
            .expect("first metadata completion");
        assert_eq!(first_outcome.wake, None);
        assert!(matches!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Sleep
        ));

        let second_outcome = service
            .push_tagged_block_completion(
                &mut tags,
                &mut depth,
                &mut tracker,
                second_dispatch.tag,
                Ok(()),
                |_| None,
            )
            .expect("second metadata completion");
        assert_eq!(second_outcome.wake, Some(PageServiceWake::Wake));
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::BackendResume {
                    page_request: resumed,
                    resume,
                } => {
                    assert_eq!(resumed, page_request);
                    assert_eq!(resume.token, PagerResumeToken::new(91));
                    assert_eq!(resume.completions.len(), 2);
                    assert!(resume
                        .completions
                        .iter()
                        .all(|completion| completion.result == Ok(())));
                }
                other => panic!("expected metadata resume, got {other:?}"),
            },
            other => panic!("expected metadata resume work, got {other:?}"),
        }
    }

    #[test]
    fn metadata_block_error_queues_terminal_page_error_without_resume() {
        let mut service = PageService::new(4);
        let page_request = PageIoRequest::new(
            PageIoRequestId::new(71),
            PageContainerKey::new(17),
            PageIoRange::new(2, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(9)),
        );
        let backend_request = BackendPageRequest::new_with_source_and_target(
            FsObjectKey::new(17),
            page_request.id,
            page_request.range,
            page_request.op,
            page_request.flags,
            page_request.generation_hint,
            IoDataSource::None,
            IoDataTarget::None,
        );
        let mut block_queue = BlockQueue::new(4);
        let submitted = block_queue
            .submit(BioPlan::new(
                DeviceKey::new(9),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(1, 0, 512)],
                BlockFlags::EMPTY,
            ))
            .expect("metadata bio");
        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let dispatch = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("metadata dispatch")
            .expect("metadata dispatch exists");
        service.register_metadata_continuation(
            page_request.clone(),
            backend_request,
            PagerResumeToken::new(92),
            &[submitted],
        );
        let mut tracker = BlockPageRequestTracker::new();

        service
            .push_tagged_block_completion(
                &mut tags,
                &mut depth,
                &mut tracker,
                dispatch.tag,
                Err(Errno::EIO),
                |_| None,
            )
            .expect("metadata error completion");

        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, page_request.id);
                    assert_eq!(route.completion.result, PageIoResult::Err(Errno::EIO));
                }
                other => panic!("expected terminal page error, got {other:?}"),
            },
            other => panic!("expected terminal page error work, got {other:?}"),
        }
    }

    #[test]
    fn merged_metadata_bios_resume_once() {
        let mut service = PageService::new(4);
        let page_request = PageIoRequest::new(
            PageIoRequestId::new(72),
            PageContainerKey::new(17),
            PageIoRange::new(3, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(10)),
        );
        let backend_request = BackendPageRequest::new_with_source_and_target(
            FsObjectKey::new(17),
            page_request.id,
            page_request.range,
            page_request.op,
            page_request.flags,
            page_request.generation_hint,
            IoDataSource::None,
            IoDataTarget::None,
        );
        let mut block_queue = BlockQueue::new(4);
        let first = block_queue
            .submit(BioPlan::new(
                DeviceKey::new(9),
                BlockOp::Read,
                LbaRange::new(64, 1),
                alloc::vec![BioVec::new(1, 0, 512)],
                BlockFlags::EMPTY,
            ))
            .expect("first metadata bio");
        let second = block_queue
            .submit(BioPlan::new(
                DeviceKey::new(9),
                BlockOp::Read,
                LbaRange::new(65, 1),
                alloc::vec![BioVec::new(2, 0, 512)],
                BlockFlags::EMPTY,
            ))
            .expect("merged metadata bio");
        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let dispatch = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("metadata dispatch")
            .expect("metadata dispatch exists");
        service.register_metadata_continuation(
            page_request,
            backend_request,
            PagerResumeToken::new(93),
            &[first, second],
        );
        let mut tracker = BlockPageRequestTracker::new();

        service
            .push_tagged_block_completion(
                &mut tags,
                &mut depth,
                &mut tracker,
                dispatch.tag,
                Ok(()),
                |_| None,
            )
            .expect("merged metadata completion");

        let PageServiceTurn::Work(work) = service.drain_turn(ServiceBudget::new(2)) else {
            panic!("merged completion must queue one resume");
        };
        assert_eq!(work.len(), 1);
        assert!(matches!(work[0], PageServiceWork::BackendResume { .. }));
    }

    #[test]
    fn empty_metadata_plan_fails_instead_of_requeueing_forever() {
        let mut service = PageService::new(4);
        let page_request = PageIoRequest::new(
            PageIoRequestId::new(73),
            PageContainerKey::new(17),
            PageIoRange::new(4, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(11)),
        );
        let backend_request = BackendPageRequest::new_with_source_and_target(
            FsObjectKey::new(17),
            page_request.id,
            page_request.range,
            page_request.op,
            page_request.flags,
            page_request.generation_hint,
            IoDataSource::None,
            IoDataTarget::None,
        );

        service.register_metadata_continuation(
            page_request.clone(),
            backend_request,
            PagerResumeToken::new(94),
            &[],
        );

        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, page_request.id);
                    assert_eq!(route.completion.result, PageIoResult::Err(Errno::ENOSYS));
                }
                other => panic!("expected terminal empty-plan error, got {other:?}"),
            },
            other => panic!("expected terminal empty-plan error work, got {other:?}"),
        }
    }

    #[test]
    fn page_service_routes_backend_frame_ref_with_completion() {
        let mut service = PageService::new(4);
        let id = PageIoRequestId::new(33);
        let completion = PageCompletion::new(
            id,
            PageIoRange::new(4, 1),
            PageIoResult::Done,
            PageGeneration::new(11),
            PageIoCompletionKind::ReadInstalled,
        )
        .with_frame_ref(PageFrameRef::new(tx_hal::Ppn(88)));

        let outcome = service.consume_backend_dispatch(BackendDispatch::PageCompletions(
            PageIoCompletionList::from_page_completions(PageCompletionList::from_vec(alloc::vec![
                completion
            ])),
        ));

        assert_eq!(
            outcome,
            PageServiceBackendOutcome::QueuedPageCompletions {
                queued: 1,
                wake: Some(PageServiceWake::Wake),
            }
        );
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, id);
                    assert_eq!(route.frame, Some(PageFrameRef::new(tx_hal::Ppn(88))));
                }
                other => panic!("expected completion route, got {other:?}"),
            },
            other => panic!("expected completion work, got {other:?}"),
        }
    }

    #[test]
    fn page_service_queues_block_page_completion_bridge_output() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(44));
        let completion = BlockPageCompletion::new(request, read_block_completion())
            .with_frame(PageFrameRef::new(tx_hal::Ppn(99)));

        let wake = service
            .push_block_page_completion(completion)
            .expect("block page completion");

        assert_eq!(wake, PageServiceWake::Wake);
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, PageIoRequestId::new(44));
                    assert_eq!(route.completion.generation, PageGeneration::new(11));
                    assert_eq!(route.frame, Some(PageFrameRef::new(tx_hal::Ppn(99))));
                }
                other => panic!("expected completion route, got {other:?}"),
            },
            other => panic!("expected completion work, got {other:?}"),
        }
    }

    #[test]
    fn page_service_routes_tracked_block_completion_with_frame() {
        let mut service = PageService::new(4);
        let mut tracker = BlockPageRequestTracker::new();
        let request = submission_request(PageIoRequestId::new(45));
        tracker.record_submit_outcomes(request, &[SubmitOutcome::Queued(BlockRequestId::new(12))]);

        let outcome = service
            .push_tracked_block_completion(&mut tracker, read_block_completion(), |completion| {
                assert_eq!(completion.request().id, PageIoRequestId::new(45));
                Some(PageFrameRef::new(tx_hal::Ppn(101)))
            })
            .expect("tracked block completion");

        assert_eq!(
            outcome,
            PageServiceBlockCompletionOutcome {
                queued: 1,
                wake: Some(PageServiceWake::Wake),
                block_submitted: 0,
            }
        );
        assert!(tracker.is_empty());
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, PageIoRequestId::new(45));
                    assert_eq!(route.frame, Some(PageFrameRef::new(tx_hal::Ppn(101))));
                }
                other => panic!("expected completion route, got {other:?}"),
            },
            other => panic!("expected completion work, got {other:?}"),
        }
    }

    #[test]
    fn page_service_routes_tracked_block_error_without_frame() {
        let mut service = PageService::new(4);
        let mut tracker = BlockPageRequestTracker::new();
        let request = submission_request(PageIoRequestId::new(46));
        tracker.record_submit_outcomes(request, &[SubmitOutcome::Queued(BlockRequestId::new(13))]);
        let mut completion = read_block_completion();
        completion.id = BlockRequestId::new(13);
        completion.result = Err(Errno::EIO);

        let outcome = service
            .push_tracked_block_completion(&mut tracker, completion, |_| None)
            .expect("tracked block error completion");

        assert_eq!(outcome.queued, 1);
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, PageIoRequestId::new(46));
                    assert_eq!(route.completion.result, PageIoResult::Err(Errno::EIO));
                    assert_eq!(route.frame, None);
                }
                other => panic!("expected completion route, got {other:?}"),
            },
            other => panic!("expected completion work, got {other:?}"),
        }
    }

    #[test]
    fn page_service_routes_tagged_l6_completion_into_l4_queue() {
        let mut service = PageService::new(4);
        let mut tracker = BlockPageRequestTracker::new();
        let mut block_queue = BlockQueue::new(4);
        let mut depth = crate::io_manager::runtime::QueueDepth::new(1);
        let mut tags = crate::io_manager::block::BlockTagTable::new();
        let request = submission_request(PageIoRequestId::new(47));
        let submit = block_queue.submit(read_bio()).expect("submit bio");
        tracker.record_submit_outcomes(request, &[submit]);
        let dispatch = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("dispatch check")
            .expect("dispatch");

        let outcome = service
            .push_tagged_block_completion(
                &mut tags,
                &mut depth,
                &mut tracker,
                dispatch.tag,
                Ok(()),
                |completion| {
                    assert_eq!(completion.request().id, PageIoRequestId::new(47));
                    Some(PageFrameRef::new(tx_hal::Ppn(102)))
                },
            )
            .expect("tagged completion");

        assert_eq!(
            outcome,
            PageServiceBlockCompletionOutcome {
                queued: 1,
                wake: Some(PageServiceWake::Wake),
                block_submitted: 0,
            }
        );
        assert!(tracker.is_empty());
        assert_eq!(tags.len(), 0);
        assert_eq!(depth.in_flight(), 0);
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, PageIoRequestId::new(47));
                    assert_eq!(route.frame, Some(PageFrameRef::new(tx_hal::Ppn(102))));
                }
                other => panic!("expected completion route, got {other:?}"),
            },
            other => panic!("expected completion work, got {other:?}"),
        }
    }

    struct CompletePlanner;

    impl BackendPlanner for CompletePlanner {
        fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan {
            BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![
                PageCompletion::new(
                    request.id,
                    request.range,
                    PageIoResult::Done,
                    request.generation_hint.unwrap_or(PageGeneration::new(0)),
                    PageIoCompletionKind::ReadInstalled,
                ),
            ]))
        }
    }

    impl PageServiceBackendContext for CompletePlanner {
        fn plan_submission(&self, request: PageIoRequest) -> Option<BackendPlan> {
            Some(self.plan_page_io(BackendPageRequest::from_page_io_request(
                FsObjectKey::new(55),
                request,
            )))
        }
    }

    struct BioPlanner;

    impl BackendPlanner for BioPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![read_bio()]))
        }
    }

    struct GraphPlanner;

    impl PageServiceBackendContext for GraphPlanner {
        fn plan_submission(&self, _request: PageIoRequest) -> Option<BackendPlan> {
            Some(BackendPlan::SubmitGraph(linear_graph(64)))
        }
    }

    fn routed_completion(id: PageIoRequestId) -> PageServiceWork {
        PageServiceWork::Completion(PageCompletionRoute {
            completion: read_completion(id),
            frame: None,
            waiters: alloc::vec![],
        })
    }

    fn submission_request(id: PageIoRequestId) -> PageIoRequest {
        PageIoRequest::new(
            id,
            PageContainerKey::new(7),
            PageIoRange::new(4, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(11)),
        )
    }

    fn demand_request_parts() -> (
        PageContainerKey,
        PageIoRange,
        PageIoOp,
        PageIoPriority,
        PageIoFlags,
        Option<PageGeneration>,
    ) {
        (
            PageContainerKey::new(7),
            PageIoRange::new(4, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(11)),
        )
    }

    #[test]
    fn page_service_turn_processes_completions_before_submissions() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));

        let work = service.drain_turn(ServiceBudget::new(2));

        assert_eq!(
            work,
            PageServiceTurn::Work(alloc::vec![
                routed_completion(id),
                PageServiceWork::Submission(submission_request(id)),
            ])
        );
        assert!(!service.has_work());
    }

    #[test]
    fn page_service_submission_work_preserves_full_request_metadata() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);

        let work = service.drain_turn(ServiceBudget::new(1));

        assert_eq!(
            work,
            PageServiceTurn::Work(alloc::vec![PageServiceWork::Submission(
                submission_request(id)
            )])
        );
    }

    #[test]
    fn page_service_consumes_backend_page_completions_into_l4_queue() {
        let mut service = PageService::new(4);
        let completion = read_completion(PageIoRequestId::new(70));

        let outcome = service.consume_backend_dispatch(BackendDispatch::PageCompletions(
            PageIoCompletionList::from_vec(alloc::vec![completion.clone()]),
        ));

        assert_eq!(
            outcome,
            PageServiceBackendOutcome::QueuedPageCompletions {
                queued: 1,
                wake: Some(PageServiceWake::Wake),
            }
        );
        assert_eq!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Work(alloc::vec![PageServiceWork::Completion(
                PageCompletionRoute {
                    completion,
                    frame: None,
                    waiters: alloc::vec![],
                }
            )])
        );
    }

    #[test]
    fn page_service_preserves_backend_block_bios_as_l6_work() {
        let mut service = PageService::new(4);
        let bio = read_bio();

        let outcome = service.consume_backend_dispatch(BackendDispatch::BlockBios(
            BioPlanList::from_vec(alloc::vec![bio.clone()]),
        ));

        assert_eq!(
            outcome,
            PageServiceBackendOutcome::BlockBios(BioPlanList::from_vec(alloc::vec![bio]))
        );
        assert!(!service.has_work());
    }

    #[test]
    fn page_service_l6_receipt_preserves_partial_admission_routes() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(79));
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::BlockBios(BioPlanList::from_vec(alloc::vec![
                    read_bio(),
                    graph_bio(48, 2),
                ])),
                request.clone(),
            )
            .expect("prepare L6 action");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("BIO plans must leave L4 as an immutable action");
        };
        assert!(matches!(&action, PageL6Action::PageBios { .. }));
        assert_eq!(action.bios().as_slice().len(), 2);

        let accepted = SubmitOutcome::Queued(BlockRequestId::new(201));
        let applied = service
            .apply_l6_receipt(PageL6Receipt {
                id: action.id(),
                submitted: alloc::vec![accepted],
                failure: Some(PageL6SubmitFailure {
                    failed_index: 1,
                    error: QueueError::Full,
                }),
            })
            .expect("apply partial receipt");

        assert_eq!(applied.failure, None);
        assert!(matches!(
            applied.outcome,
            PageServiceBackendSubmitOutcome::BlockBiosQueued {
                request: applied_request,
                submitted,
            } if applied_request == request && submitted == alloc::vec![accepted]
        ));
    }

    #[test]
    fn page_service_partial_page_receipt_emits_one_error_after_prefix_terminal() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(791));
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::BlockBios(BioPlanList::from_vec(alloc::vec![
                    read_bio(),
                    graph_bio(48, 2),
                ])),
                request.clone(),
            )
            .expect("prepare L6 action");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("BIO plans must leave L4 as an immutable action");
        };
        let mut queue = BlockQueue::new(1);
        let receipt = submit_l6_action_to_queue(action, &mut queue);
        let submitted = receipt.submitted.clone();
        assert_eq!(submitted.len(), 1);
        assert_eq!(
            receipt.failure.map(|failure| failure.error),
            Some(QueueError::Full)
        );
        let applied = service.apply_l6_receipt(receipt).expect("partial receipt");
        assert!(matches!(
            applied.outcome,
            PageServiceBackendSubmitOutcome::BlockBiosQueued { .. }
        ));
        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let dispatch = queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("dispatch prefix")
            .expect("prefix dispatch");
        let mut tracker = BlockPageRequestTracker::new();
        tracker.record_submit_outcomes(request.clone(), &submitted);
        let outcome = service
            .push_tagged_block_completion(
                &mut tags,
                &mut depth,
                &mut tracker,
                dispatch.tag,
                Ok(()),
                |_| None,
            )
            .expect("prefix completion");
        assert_eq!(outcome.queued, 1);
        let PageServiceTurn::Work(work) = service.drain_turn(ServiceBudget::new(1)) else {
            panic!("admission error completion must be queued");
        };
        let PageServiceWork::Completion(route) = &work[0] else {
            panic!("expected terminal completion");
        };
        assert_eq!(route.completion.id, request.id);
        assert_eq!(route.completion.result, PageIoResult::Err(Errno::EAGAIN));
    }

    #[test]
    fn page_service_zero_page_receipt_returns_synchronous_error() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(792));
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::BlockBios(BioPlanList::from_vec(alloc::vec![
                    read_bio(),
                ])),
                request.clone(),
            )
            .expect("prepare page action");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("BIO plan must leave L4 as an immutable action");
        };

        let applied = service
            .apply_l6_receipt(PageL6Receipt {
                id: action.id(),
                submitted: alloc::vec![],
                failure: Some(PageL6SubmitFailure {
                    failed_index: 0,
                    error: QueueError::Full,
                }),
            })
            .expect("apply rejected receipt");

        assert_eq!(
            applied.outcome,
            PageServiceBackendSubmitOutcome::Err {
                request,
                errno: Errno::EAGAIN,
            }
        );
        assert_eq!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Sleep
        );
    }

    #[test]
    fn page_service_zero_metadata_receipt_returns_synchronous_error() {
        let mut service = PageService::new(4);
        let page_request = submission_request(PageIoRequestId::new(793));
        let backend_request =
            BackendPageRequest::from_page_io_request(FsObjectKey::new(793), page_request.clone());
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::MetadataFirst {
                    request: backend_request,
                    bios: BioPlanList::from_vec(alloc::vec![read_bio()]),
                    resume: PagerResumeToken::new(793),
                },
                page_request.clone(),
            )
            .expect("prepare metadata action");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("metadata plan must leave L4 as an immutable action");
        };

        let applied = service
            .apply_l6_receipt(PageL6Receipt {
                id: action.id(),
                submitted: alloc::vec![],
                failure: Some(PageL6SubmitFailure {
                    failed_index: 0,
                    error: QueueError::Full,
                }),
            })
            .expect("apply rejected metadata receipt");

        assert_eq!(
            applied.outcome,
            PageServiceBackendSubmitOutcome::Err {
                request: page_request,
                errno: Errno::EAGAIN,
            }
        );
        assert_eq!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Sleep
        );
    }

    #[test]
    fn page_service_partial_metadata_receipt_defers_admission_error() {
        let mut service = PageService::new(4);
        let page_request = submission_request(PageIoRequestId::new(795));
        let backend_request =
            BackendPageRequest::from_page_io_request(FsObjectKey::new(795), page_request.clone());
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::MetadataFirst {
                    request: backend_request,
                    bios: BioPlanList::from_vec(alloc::vec![read_bio(), graph_bio(72, 4)]),
                    resume: PagerResumeToken::new(795),
                },
                page_request.clone(),
            )
            .expect("prepare metadata action");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("metadata plan must leave L4 as an immutable action");
        };
        let mut block_queue = BlockQueue::new(1);
        let receipt = submit_l6_action_to_queue(action, &mut block_queue);
        assert_eq!(receipt.submitted.len(), 1);
        assert!(receipt.failure.is_some());

        let accepted = receipt.submitted[0];
        let applied = service.apply_l6_receipt(receipt).expect("partial receipt");
        assert_eq!(applied.failure, None);
        assert!(matches!(
            applied.outcome,
            PageServiceBackendSubmitOutcome::MetadataFirstQueued { submitted, .. }
                if submitted == alloc::vec![accepted]
        ));

        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let dispatch = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("metadata dispatch")
            .expect("metadata dispatch exists");
        let mut tracker = BlockPageRequestTracker::new();
        let outcome = service
            .push_tagged_block_completion(
                &mut tags,
                &mut depth,
                &mut tracker,
                dispatch.tag,
                Ok(()),
                |_| None,
            )
            .expect("metadata completion");
        assert_eq!(outcome.queued, 0);
        assert!(matches!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Work(work)
                if matches!(work.as_slice(), [PageServiceWork::Completion(route)]
                    if route.completion.id == page_request.id
                        && route.completion.result == PageIoResult::Err(Errno::EAGAIN))
        ));
    }

    #[test]
    fn page_service_zero_graph_receipt_returns_synchronous_error() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(794));
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(join_graph()),
                request.clone(),
            )
            .expect("prepare graph roots");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("graph roots must leave L4 as an immutable action");
        };

        let applied = service
            .apply_l6_receipt(PageL6Receipt {
                id: action.id(),
                submitted: alloc::vec![],
                failure: Some(PageL6SubmitFailure {
                    failed_index: 0,
                    error: QueueError::Full,
                }),
            })
            .expect("apply rejected graph receipt");

        assert_eq!(
            applied.outcome,
            PageServiceBackendSubmitOutcome::Err {
                request: request.clone(),
                errno: Errno::EAGAIN,
            }
        );
        assert!(matches!(
            service.prepare_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(join_graph()),
                request,
            ),
            Ok(PageServiceBackendPrepared::Submit(_))
        ));
        assert_eq!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Sleep
        );
    }

    #[test]
    fn page_service_partial_graph_receipt_defers_admission_error() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(796));
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(join_graph()),
                request.clone(),
            )
            .expect("prepare graph roots");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("graph roots must leave L4 as an immutable action");
        };
        let mut block_queue = BlockQueue::new(1);
        let receipt = submit_l6_action_to_queue(action, &mut block_queue);
        assert_eq!(receipt.submitted.len(), 1);
        assert!(receipt.failure.is_some());

        let accepted = receipt.submitted[0];
        let applied = service.apply_l6_receipt(receipt).expect("partial receipt");
        assert_eq!(applied.failure, None);
        assert!(matches!(
            applied.outcome,
            PageServiceBackendSubmitOutcome::BlockGraphQueued { submitted, .. }
                if submitted == alloc::vec![accepted]
        ));

        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let dispatch = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("graph root dispatch")
            .expect("graph root exists");
        let mut tracker = BlockPageRequestTracker::new();
        let outcome = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                dispatch.tag,
                Ok(()),
                |_| None,
            )
            .expect("graph completion");
        assert_eq!(outcome.queued, 1);
        assert!(matches!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Work(work)
                if matches!(work.as_slice(), [PageServiceWork::Completion(route)]
                    if route.completion.id == request.id
                        && route.completion.result == PageIoResult::Err(Errno::EAGAIN))
        ));
    }

    #[test]
    fn page_service_l6_metadata_receipt_registers_one_resume_route() {
        let mut service = PageService::new(4);
        let page_request = submission_request(PageIoRequestId::new(790));
        let backend_request =
            BackendPageRequest::from_page_io_request(FsObjectKey::new(790), page_request.clone());
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::MetadataFirst {
                    request: backend_request.clone(),
                    bios: BioPlanList::from_vec(alloc::vec![read_bio()]),
                    resume: PagerResumeToken::new(790),
                },
                page_request.clone(),
            )
            .expect("prepare metadata action");
        let PageServiceBackendPrepared::Submit(action) = prepared else {
            panic!("metadata dispatch must cross L6 as an action");
        };

        service
            .apply_l6_receipt(PageL6Receipt {
                id: action.id(),
                submitted: alloc::vec![SubmitOutcome::Queued(BlockRequestId::new(790))],
                failure: None,
            })
            .expect("accept metadata receipt");
        service
            .prepare_block_completion_routes(
                BlockCompletion {
                    tag: BlockTag::new(790),
                    id: BlockRequestId::new(790),
                    plan: read_bio(),
                    result: Ok(()),
                },
                alloc::vec![],
                false,
                |_| None,
            )
            .expect("metadata completion must resume through L4");

        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::BackendResume {
                    page_request: resumed,
                    resume,
                } => {
                    assert_eq!(resumed, page_request);
                    assert_eq!(resume.request, Some(backend_request));
                    assert_eq!(resume.token, PagerResumeToken::new(790));
                    assert_eq!(resume.completions.len(), 1);
                }
                other => panic!("expected metadata resume, got {other:?}"),
            },
            other => panic!("expected one metadata resume, got {other:?}"),
        }
        assert_eq!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Sleep
        );
    }

    #[test]
    fn page_service_consumes_l6_page_routes_once_without_a_second_tracker() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(791));
        let completion = BlockCompletion {
            tag: BlockTag::new(71),
            id: BlockRequestId::new(701),
            plan: read_bio(),
            result: Ok(()),
        };
        let route = BlockPageCompletion::new(request.clone(), completion.clone());

        let first = service
            .prepare_block_completion_routes(completion.clone(), alloc::vec![route], false, |_| {
                Some(PageFrameRef::new(tx_hal::Ppn(701)))
            })
            .expect("L6 route must reach L4 once");
        assert_eq!(first.outcome.queued, 1);
        assert!(first.actions.is_empty());

        assert_eq!(
            service.prepare_block_completion_routes(completion, alloc::vec![], false, |_| None),
            Err(PageServiceTaggedBlockCompletionError::Page(
                PageServiceBlockCompletionError::Tracker(
                    BlockPageRequestTrackerError::UnknownBlockRequest(BlockRequestId::new(701))
                )
            ))
        );
        assert!(matches!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Work(work)
                if matches!(work.as_slice(), [PageServiceWork::Completion(route)] if route.completion.id == request.id)
        ));
    }

    #[test]
    fn page_service_graph_completion_emits_followup_l6_action_without_queue_borrow() {
        let mut service = PageService::new(4);
        let request = submission_request(PageIoRequestId::new(80));
        let prepared = service
            .prepare_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(join_graph()),
                request.clone(),
            )
            .expect("prepare graph roots");
        let PageServiceBackendPrepared::Submit(roots) = prepared else {
            panic!("graph roots must leave L4 as an immutable action");
        };
        assert_eq!(roots.bios().as_slice().len(), 2);
        service
            .apply_l6_receipt(PageL6Receipt {
                id: roots.id(),
                submitted: alloc::vec![
                    SubmitOutcome::Queued(BlockRequestId::new(211)),
                    SubmitOutcome::Queued(BlockRequestId::new(212)),
                ],
                failure: None,
            })
            .expect("apply graph-root receipt");

        let first = service
            .prepare_block_completion_routes(
                BlockCompletion {
                    tag: BlockTag::new(1),
                    id: BlockRequestId::new(211),
                    plan: read_bio(),
                    result: Ok(()),
                },
                alloc::vec![],
                false,
                |_| None,
            )
            .expect("first root completion");
        assert!(first.actions.is_empty());

        let second = service
            .prepare_block_completion_routes(
                BlockCompletion {
                    tag: BlockTag::new(2),
                    id: BlockRequestId::new(212),
                    plan: read_bio(),
                    result: Ok(()),
                },
                alloc::vec![],
                false,
                |_| None,
            )
            .expect("second root completion");
        assert_eq!(second.actions.len(), 1);
        let join = second.actions.into_iter().next().expect("join action");
        assert!(matches!(&join, PageL6Action::GraphReady { .. }));
        assert_eq!(join.bios().as_slice().len(), 1);
        service
            .apply_l6_receipt(PageL6Receipt {
                id: join.id(),
                submitted: alloc::vec![SubmitOutcome::Queued(BlockRequestId::new(213))],
                failure: None,
            })
            .expect("apply join receipt");

        let terminal = service
            .prepare_block_completion_routes(
                BlockCompletion {
                    tag: BlockTag::new(3),
                    id: BlockRequestId::new(213),
                    plan: read_bio(),
                    result: Ok(()),
                },
                alloc::vec![],
                false,
                |_| None,
            )
            .expect("join completion");
        assert_eq!(terminal.outcome.queued, 1);
        assert!(terminal.actions.is_empty());
        assert!(matches!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Work(work)
                if matches!(work.as_slice(), [PageServiceWork::Completion(route)] if route.completion.id == request.id)
        ));
    }

    #[test]
    fn page_service_preserves_backend_graph_for_l4_execution() {
        let mut service = PageService::new(4);
        let graph = BackendBioGraph::new(
            alloc::vec![BackendBioNode::new(
                BackendBioNodeId::new(1),
                read_bio(),
                IoDataSource::None,
            )],
            alloc::vec![],
        )
        .expect("single-node graph");

        assert_eq!(
            service.consume_backend_dispatch(BackendDispatch::BlockGraph(graph)),
            PageServiceBackendOutcome::BlockGraph(
                BackendBioGraph::new(
                    alloc::vec![BackendBioNode::new(
                        BackendBioNodeId::new(1),
                        read_bio(),
                        IoDataSource::None,
                    )],
                    alloc::vec![],
                )
                .expect("single-node graph"),
            ),
        );
    }

    #[test]
    fn page_service_releases_graph_join_only_after_all_predecessors_complete() {
        let mut service = PageService::new(4);
        let mut block_queue = BlockQueue::new(8);
        let request = submission_request(PageIoRequestId::new(80));

        let queued = service
            .queue_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(join_graph()),
                &mut block_queue,
                request.clone(),
            )
            .expect("queue graph roots");
        let PageServiceBackendSubmitOutcome::BlockGraphQueued { submitted, .. } = queued else {
            panic!("graph roots should enter the L6 queue");
        };
        assert_eq!(submitted.len(), 2);

        let mut depth = QueueDepth::new(2);
        let mut tags = BlockTagTable::new();
        let first = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("first root dispatch")
            .expect("first root exists");
        let second = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("second root dispatch")
            .expect("second root exists");
        let mut tracker = BlockPageRequestTracker::new();

        let first_completion = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                first.tag,
                Ok(()),
                |_| None,
            )
            .expect("first root completion");
        assert_eq!(first_completion.queued, 0);
        assert_eq!(first_completion.block_submitted, 0);
        assert!(
            block_queue
                .pop_dispatchable_tagged(&mut depth, &mut tags)
                .expect("join is not yet ready")
                .is_none(),
            "one predecessor must not release the join"
        );

        let second_completion = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                second.tag,
                Ok(()),
                |_| None,
            )
            .expect("second root completion");
        assert_eq!(second_completion.queued, 0);
        assert_eq!(second_completion.block_submitted, 1);
        let joined = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("joined dispatch")
            .expect("join becomes dispatchable after both roots");

        let terminal = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                joined.tag,
                Ok(()),
                |_| None,
            )
            .expect("joined completion");
        assert_eq!(terminal.queued, 1);
        assert_eq!(terminal.block_submitted, 0);
        assert_eq!(terminal.wake, Some(PageServiceWake::Wake));
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, request.id);
                    assert_eq!(route.completion.result, PageIoResult::Done);
                    assert_eq!(route.completion.generation, PageGeneration::new(11));
                }
                other => panic!("expected graph terminal completion, got {other:?}"),
            },
            other => panic!("expected graph terminal work, got {other:?}"),
        }
    }

    #[test]
    fn page_service_driver_submits_graph_plan_roots_to_l6() {
        let mut service = PageService::new(4);
        let request_id = demand_request(&mut service);
        let mut driver = PageServiceDriver::new(ServiceBudget::new(1));
        let mut block_queue = BlockQueue::new(8);

        let driven =
            driver.drive_once_with_backend(&mut service, &GraphPlanner, &mut block_queue, |_| true);

        assert!(matches!(
            driven.work.as_slice(),
            [PageServiceDrivenWork::BackendSubmission(
                PageServiceBackendSubmitOutcome::BlockGraphQueued {
                    request,
                    submitted,
                }
            )] if request.id == request_id && submitted.len() == 1
        ));
        assert_eq!(block_queue.len(), 1);
    }

    #[test]
    fn page_service_graph_failure_preserves_generation_and_stops_dependents() {
        let mut service = PageService::new(4);
        let mut block_queue = BlockQueue::new(8);
        let request = submission_request(PageIoRequestId::new(81));
        service
            .queue_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(linear_graph(64)),
                &mut block_queue,
                request.clone(),
            )
            .expect("queue graph roots");

        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let root = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("root dispatch")
            .expect("root exists");
        let mut tracker = BlockPageRequestTracker::new();

        let terminal = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                root.tag,
                Err(Errno::EIO),
                |_| None,
            )
            .expect("root failure");
        assert_eq!(terminal.queued, 1);
        assert_eq!(terminal.block_submitted, 0);
        assert!(block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("failed graph must not admit dependents")
            .is_none());
        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, request.id);
                    assert_eq!(route.completion.result, PageIoResult::Err(Errno::EIO));
                    assert_eq!(route.completion.generation, PageGeneration::new(11));
                }
                other => panic!("expected graph terminal error, got {other:?}"),
            },
            other => panic!("expected graph terminal error work, got {other:?}"),
        }
    }

    #[test]
    fn reserved_background_graph_emits_noop_without_pending_submission() {
        let mut service = PageService::new(4);
        let mut block_queue = BlockQueue::new(4);
        let request = service
            .reserve_background_request(PageContainerKey::new(7), PageIoRange::new(4, 1))
            .expect("reserve checkpoint request");
        assert_eq!(request.op, PageIoOp::Checkpoint);
        assert_eq!(request.priority, PageIoPriority::BackgroundWriteback);
        assert_eq!(service.submission_len(), 0);

        let graph = BackendBioGraph::new(
            alloc::vec![BackendBioNode::new(
                BackendBioNodeId::new(91),
                graph_bio(96, 9),
                IoDataSource::None,
            )],
            alloc::vec![],
        )
        .expect("valid checkpoint graph");
        service
            .queue_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(graph),
                &mut block_queue,
                request.clone(),
            )
            .expect("queue checkpoint graph");

        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let dispatched = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("checkpoint dispatch")
            .expect("checkpoint graph root");
        let mut tracker = BlockPageRequestTracker::new();
        let terminal = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                dispatched.tag,
                Ok(()),
                |_| None,
            )
            .expect("checkpoint completion");
        assert_eq!(terminal.queued, 1);

        match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut work) => match work.remove(0) {
                PageServiceWork::Completion(route) => {
                    assert_eq!(route.completion.id, request.id);
                    assert_eq!(route.completion.kind, PageIoCompletionKind::Noop);
                    assert_eq!(route.completion.result, PageIoResult::Done);
                }
                other => panic!("expected checkpoint completion, got {other:?}"),
            },
            other => panic!("expected checkpoint completion work, got {other:?}"),
        }
    }

    #[test]
    fn page_service_routes_merged_l6_completion_to_every_graph_consumer() {
        let mut service = PageService::new(4);
        let mut block_queue = BlockQueue::new(8);
        let first_request = submission_request(PageIoRequestId::new(82));
        let second_request = submission_request(PageIoRequestId::new(83));
        service
            .queue_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(linear_graph(64)),
                &mut block_queue,
                first_request.clone(),
            )
            .expect("queue first graph");
        service
            .queue_backend_outcome(
                PageServiceBackendOutcome::BlockGraph(linear_graph(65)),
                &mut block_queue,
                second_request.clone(),
            )
            .expect("queue merged second graph");
        assert_eq!(block_queue.len(), 1, "equivalent roots should merge in L6");

        let mut depth = QueueDepth::new(1);
        let mut tags = BlockTagTable::new();
        let root = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("root dispatch")
            .expect("merged root exists");
        let mut tracker = BlockPageRequestTracker::new();
        let root_completion = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                root.tag,
                Ok(()),
                |_| None,
            )
            .expect("merged root completion");
        assert_eq!(root_completion.block_submitted, 2);
        assert_eq!(
            block_queue.len(),
            1,
            "equivalent successors should merge in L6"
        );

        let successor = block_queue
            .pop_dispatchable_tagged(&mut depth, &mut tags)
            .expect("successor dispatch")
            .expect("merged successor exists");
        let terminal = service
            .push_tagged_block_completion_with_graphs(
                &mut tags,
                &mut depth,
                &mut tracker,
                &mut block_queue,
                successor.tag,
                Ok(()),
                |_| None,
            )
            .expect("merged successor completion");
        assert_eq!(terminal.queued, 2);

        let PageServiceTurn::Work(work) = service.drain_turn(ServiceBudget::new(2)) else {
            panic!("both graph consumers need terminal completions");
        };
        let ids = work
            .into_iter()
            .map(|item| match item {
                PageServiceWork::Completion(route) => route.completion.id,
                other => panic!("expected graph terminal completion, got {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, alloc::vec![first_request.id, second_request.id]);
    }

    #[test]
    fn page_service_preserves_backend_metadata_yield_and_error_outcomes() {
        let mut service = PageService::new(4);
        let bio = read_bio();
        let resume = PagerResumeToken::new(5);
        let backend_request = BackendPageRequest::new(
            FsObjectKey::new(1),
            PageIoRequestId::new(4),
            PageIoRange::new(1, 1),
            PageIoOp::Read,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(1)),
        );

        assert_eq!(
            service.consume_backend_dispatch(BackendDispatch::MetadataFirst {
                request: backend_request.clone(),
                bios: BioPlanList::from_vec(alloc::vec![bio.clone()]),
                resume,
            }),
            PageServiceBackendOutcome::MetadataFirst {
                request: backend_request,
                bios: BioPlanList::from_vec(alloc::vec![bio]),
                resume,
            }
        );
        assert_eq!(
            service.consume_backend_dispatch(BackendDispatch::Yield(WaitSourceId::new(99))),
            PageServiceBackendOutcome::Yield(WaitSourceId::new(99))
        );
        assert_eq!(
            service.consume_backend_dispatch(BackendDispatch::Err(Errno::EIO)),
            PageServiceBackendOutcome::Err(Errno::EIO)
        );
        assert!(!service.has_work());
    }

    #[test]
    fn page_service_submission_plans_dispatches_and_queues_page_completion() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        let request = match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Submission(request) => request,
                other => panic!("expected submission, got {other:?}"),
            },
            other => panic!("expected submission turn, got {other:?}"),
        };
        let mut block_queue = BlockQueue::new(4);

        let outcome = service
            .consume_backend_submission(
                FsObjectKey::new(55),
                request,
                &CompletePlanner,
                &mut block_queue,
            )
            .expect("backend submission");

        assert_eq!(
            outcome,
            PageServiceBackendSubmitOutcome::QueuedPageCompletions {
                queued: 1,
                wake: Some(PageServiceWake::AlreadyRunnable),
            }
        );
        assert!(block_queue.is_empty());
        assert_eq!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Work(alloc::vec![routed_completion(id)])
        );
    }

    #[test]
    fn page_service_submission_feeds_backend_bios_into_l6_block_queue() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        let request = match service.drain_turn(ServiceBudget::new(1)) {
            PageServiceTurn::Work(mut items) => match items.remove(0) {
                PageServiceWork::Submission(request) => request,
                other => panic!("expected submission, got {other:?}"),
            },
            other => panic!("expected submission turn, got {other:?}"),
        };
        let mut block_queue = BlockQueue::new(4);

        let outcome = service
            .consume_backend_submission(
                FsObjectKey::new(56),
                request,
                &BioPlanner,
                &mut block_queue,
            )
            .expect("backend submission");

        assert_eq!(
            outcome,
            PageServiceBackendSubmitOutcome::BlockBiosQueued {
                request: submission_request(id),
                submitted: alloc::vec![SubmitOutcome::Queued(
                    crate::io_manager::block::BlockRequestId::new(1)
                )],
            }
        );
        assert_eq!(block_queue.len(), 1);
        let mut depth = crate::io_manager::runtime::QueueDepth::new(1);
        let bio = block_queue
            .pop_dispatchable(&mut depth)
            .expect("dispatch check")
            .expect("queued bio");
        assert_eq!(bio.plan, read_bio());
    }

    #[test]
    fn page_service_driver_plans_submission_and_rekicks_for_queued_completion() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        let mut block_queue = BlockQueue::new(4);
        let mut driver = PageServiceDriver::new(ServiceBudget::new(1));
        let mut kicks = Vec::new();

        let driven = driver.drive_once_with_backend(
            &mut service,
            &CompletePlanner,
            &mut block_queue,
            |kick| {
                kicks.push(kick);
                true
            },
        );

        assert_eq!(
            driven.work,
            alloc::vec![PageServiceDrivenWork::BackendSubmission(
                PageServiceBackendSubmitOutcome::QueuedPageCompletions {
                    queued: 1,
                    wake: Some(PageServiceWake::Wake),
                }
            )]
        );
        assert_eq!(driven.next, PageServiceNext::Runnable);
        assert_eq!(driven.kicks, 1);
        assert_eq!(kicks, alloc::vec![ServiceKick::new(IoServiceKind::Page)]);
        assert_eq!(
            service.drain_turn(ServiceBudget::new(1)),
            PageServiceTurn::Work(alloc::vec![routed_completion(id)])
        );
    }

    #[test]
    fn page_service_driver_returns_unplanned_submission_for_compatibility_path() {
        struct NoPlanner;

        impl PageServiceBackendContext for NoPlanner {
            fn plan_submission(&self, _request: PageIoRequest) -> Option<BackendPlan> {
                None
            }
        }

        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        let mut block_queue = BlockQueue::new(4);
        let mut driver = PageServiceDriver::new(ServiceBudget::new(1));

        let driven =
            driver.drive_once_with_backend(&mut service, &NoPlanner, &mut block_queue, |_| {
                panic!("unplanned compatibility work must not kick page service")
            });

        assert_eq!(
            driven.work,
            alloc::vec![PageServiceDrivenWork::UnplannedSubmission(
                submission_request(id)
            )]
        );
        assert_eq!(driven.next, PageServiceNext::Sleeping);
        assert_eq!(driven.kicks, 0);
        assert!(block_queue.is_empty());
        assert!(!service.has_work());
    }

    #[test]
    fn page_service_budget_preserves_submission_backlog_after_completion() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));

        let work = service.drain_turn(ServiceBudget::new(1));

        assert_eq!(
            work,
            PageServiceTurn::Work(alloc::vec![routed_completion(id)])
        );
        assert!(service.has_work());

        let work = service.drain_turn(ServiceBudget::new(1));
        assert!(matches!(
            work,
            PageServiceTurn::Work(items)
                if matches!(items.as_slice(), [PageServiceWork::Submission(request)] if request.op == PageIoOp::Read)
        ));
        assert!(!service.has_work());
    }

    #[test]
    fn page_service_turn_sleeps_when_queues_are_empty() {
        let mut service = PageService::new(4);

        assert_eq!(
            service.drain_turn(ServiceBudget::new(4)),
            PageServiceTurn::Sleep
        );
    }

    #[test]
    fn page_service_completion_routes_registered_waiters_before_submission() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service
            .wait_on(
                id,
                PageWaiter {
                    source_id: 44,
                    interests: PageWaitInterest::READY,
                },
            )
            .expect("register waiter");
        service.push_completion(read_completion(id));

        let work = service.drain_turn(ServiceBudget::new(2));

        assert_eq!(
            work,
            PageServiceTurn::Work(alloc::vec![
                PageServiceWork::Completion(PageCompletionRoute {
                    completion: read_completion(id),
                    frame: None,
                    waiters: alloc::vec![PageWaiter {
                        source_id: 44,
                        interests: PageWaitInterest::READY,
                    }],
                }),
                PageServiceWork::Submission(submission_request(id)),
            ])
        );
        assert!(!service.has_waiters(id));
    }

    #[test]
    fn page_service_completion_routes_waiters_once() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service
            .wait_on(
                id,
                PageWaiter {
                    source_id: 45,
                    interests: PageWaitInterest::READY,
                },
            )
            .expect("register waiter");
        service.push_completion(read_completion(id));

        let _ = service.drain_turn(ServiceBudget::new(1));
        service.push_completion(read_completion(id));
        let work = service.drain_turn(ServiceBudget::new(1));

        assert_eq!(
            work,
            PageServiceTurn::Work(alloc::vec![PageServiceWork::Completion(
                PageCompletionRoute {
                    completion: read_completion(id),
                    frame: None,
                    waiters: alloc::vec![],
                }
            )])
        );
    }

    #[test]
    fn page_service_drive_reschedules_when_budget_leaves_backlog() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));

        let step = service.drive_turn(ServiceBudget::new(1));

        assert_eq!(
            step,
            PageServiceStep {
                turn: PageServiceTurn::Work(alloc::vec![routed_completion(id)]),
                next: PageServiceNext::Runnable,
            }
        );
        assert!(service.has_work());
    }

    #[test]
    fn page_service_drive_sleeps_after_draining_all_work() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));

        let step = service.drive_turn(ServiceBudget::new(2));

        assert_eq!(
            step,
            PageServiceStep {
                turn: PageServiceTurn::Work(alloc::vec![
                    routed_completion(id),
                    PageServiceWork::Submission(submission_request(id)),
                ]),
                next: PageServiceNext::Sleeping,
            }
        );
        assert!(!service.has_work());
    }

    #[test]
    fn page_service_drive_sleeps_without_work() {
        let mut service = PageService::new(4);

        assert_eq!(
            service.drive_turn(ServiceBudget::new(2)),
            PageServiceStep {
                turn: PageServiceTurn::Sleep,
                next: PageServiceNext::Sleeping,
            }
        );
    }

    #[test]
    fn page_service_submit_wakes_after_service_sleeps() {
        let mut service = PageService::new(4);
        assert_eq!(
            service.drive_turn(ServiceBudget::new(2)).next,
            PageServiceNext::Sleeping
        );

        let (pc, range, op, priority, flags, generation) = demand_request_parts();
        let outcome = service
            .submit_with_wake(pc, range, op, priority, flags, generation)
            .expect("submit after sleep");

        assert_eq!(outcome.wake, PageServiceWake::Wake);
        assert_eq!(outcome.id, PageIoRequestId::new(1));
    }

    #[test]
    fn page_service_submit_does_not_duplicate_wake_when_backlog_is_runnable() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));
        assert_eq!(
            service.drive_turn(ServiceBudget::new(1)).next,
            PageServiceNext::Runnable
        );

        let outcome = service
            .submit_with_wake(
                PageContainerKey::new(7),
                PageIoRange::new(5, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(12)),
            )
            .expect("submit while runnable");

        assert_eq!(outcome.wake, PageServiceWake::AlreadyRunnable);
    }

    #[test]
    fn page_service_completion_wakes_after_service_sleeps() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        let _ = service.drive_turn(ServiceBudget::new(1));
        assert_eq!(
            service.drive_turn(ServiceBudget::new(1)).next,
            PageServiceNext::Sleeping
        );

        let wake = service.push_completion_with_wake(read_completion(id));

        assert_eq!(wake, PageServiceWake::Wake);
    }

    #[test]
    fn page_service_submit_with_kick_posts_page_service_wake_after_sleep() {
        let mut service = PageService::new(4);
        assert_eq!(
            service.drive_turn(ServiceBudget::new(1)).next,
            PageServiceNext::Sleeping
        );
        let mut kicks = Vec::new();
        let (pc, range, op, priority, flags, generation) = demand_request_parts();

        let outcome = service
            .submit_with_kick(pc, range, op, priority, flags, generation, |kick| {
                kicks.push(kick);
                true
            })
            .expect("submit with kick");

        assert_eq!(outcome.wake, PageServiceWake::Wake);
        assert_eq!(kicks, alloc::vec![ServiceKick::new(IoServiceKind::Page)]);
    }

    #[test]
    fn page_service_submit_with_kick_skips_post_when_already_runnable() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));
        assert_eq!(
            service.drive_turn(ServiceBudget::new(1)).next,
            PageServiceNext::Runnable
        );
        let mut kicks = Vec::new();

        let outcome = service
            .submit_with_kick(
                PageContainerKey::new(7),
                PageIoRange::new(5, 1),
                PageIoOp::Read,
                PageIoPriority::Demand,
                PageIoFlags::DEMAND,
                Some(PageGeneration::new(12)),
                |kick| {
                    kicks.push(kick);
                    true
                },
            )
            .expect("submit while runnable");

        assert_eq!(outcome.wake, PageServiceWake::AlreadyRunnable);
        assert!(kicks.is_empty());
    }

    #[test]
    fn page_service_completion_with_kick_posts_page_service_wake_after_sleep() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        let _ = service.drive_turn(ServiceBudget::new(1));
        assert_eq!(
            service.drive_turn(ServiceBudget::new(1)).next,
            PageServiceNext::Sleeping
        );
        let mut kicks = Vec::new();

        let wake = service.push_completion_with_kick(read_completion(id), |kick| {
            kicks.push(kick);
            true
        });

        assert_eq!(wake, PageServiceWake::Wake);
        assert_eq!(kicks, alloc::vec![ServiceKick::new(IoServiceKind::Page)]);
    }

    #[test]
    fn page_service_driver_kicks_again_when_budget_leaves_backlog() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));
        let mut driver = PageServiceDriver::new(ServiceBudget::new(1));
        let mut kicks = Vec::new();

        let driven = driver.drive_once(&mut service, |kick| {
            kicks.push(kick);
            true
        });

        assert_eq!(
            driven.step,
            PageServiceStep {
                turn: PageServiceTurn::Work(alloc::vec![routed_completion(id)]),
                next: PageServiceNext::Runnable,
            }
        );
        assert_eq!(driven.kicks, 1);
        assert_eq!(kicks, alloc::vec![ServiceKick::new(IoServiceKind::Page)]);
    }

    #[test]
    fn page_service_driver_sleeps_without_rekick_after_draining_work() {
        let mut service = PageService::new(4);
        let id = demand_request(&mut service);
        service.push_completion(read_completion(id));
        let mut driver = PageServiceDriver::new(ServiceBudget::new(2));
        let mut kicks = Vec::new();

        let driven = driver.drive_once(&mut service, |kick| {
            kicks.push(kick);
            true
        });

        assert_eq!(driven.step.next, PageServiceNext::Sleeping);
        assert_eq!(driven.kicks, 0);
        assert!(kicks.is_empty());
    }
}
