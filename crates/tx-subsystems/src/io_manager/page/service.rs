//! Budgeted L4 page-service queue ownership.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use crate::execution::{Errno, Guard};
use crate::fs_iface::{IoDataSource, IoDataTarget};
use crate::io_manager::backend::{
    BackendBioCompletion, BackendBioGraph, BackendBioNodeId, BackendDispatch, BackendGraphAdvance,
    BackendGraphScheduler, BackendGraphSchedulerError, BackendPageRequest, BackendPlan,
    BackendPlanResume, BackendPlanner, BioPlanList, BlockPageCompletion, BlockPageCompletionError,
    BlockPageRequestTracker, BlockPageRequestTrackerError, FsObjectKey, PageCompletion,
    PageFrameRef, PageIoCompletionEntry, PagerResumeToken, WaitSourceId, dispatch_backend_plan,
    plan_backend_request,
};
use crate::io_manager::block::{
    BlockCompletion, BlockCompletionError, BlockQueue, BlockRequestId, BlockTag, BlockTagTable,
    QueueError, SubmitOutcome,
};
use crate::io_manager::page::{
    PageContainerKey, PageIoCompletion, PageIoFlags, PageIoOp, PageIoPriority, PageIoRange,
    PageIoRequest, PageIoRequestId, PageQueueError, PageRequestQueue,
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
    Err(Errno),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageServiceBackendSubmitError {
    BlockQueue(QueueError),
    Graph(BackendGraphSchedulerError),
    DuplicateGraph(PageIoRequestId),
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
    BackendSubmitError(PageServiceBackendSubmitError),
    UnplannedSubmission(PageIoRequest),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageServiceBackendDriven {
    pub work: Vec<PageServiceDrivenWork>,
    pub next: PageServiceNext,
    pub kicks: usize,
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
                        let page_request = request.clone();
                        match service.queue_backend_outcome(outcome, block_queue, request) {
                            Ok(outcome) => {
                                register_metadata_outcome(service, page_request, &outcome);
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
                        let original_page_request = page_request.clone();
                        match service.queue_backend_outcome(outcome, block_queue, page_request) {
                            Ok(outcome) => {
                                register_metadata_outcome(service, original_page_request, &outcome);
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

        PageServiceBackendDriven { work, next, kicks }
    }
}

#[derive(Debug)]
pub struct PageService {
    submissions: PageRequestQueue,
    completions: VecDeque<PageIoCompletionEntry>,
    backend_resumes: VecDeque<(PageIoRequest, BackendPlanResume)>,
    metadata: BTreeMap<BlockRequestId, Vec<MetadataContinuation>>,
    graphs: BTreeMap<PageIoRequestId, BackendGraphExecution>,
    waiters: BTreeMap<PageIoRequestId, Vec<PageWaiter>>,
    next: PageServiceNext,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MetadataContinuation {
    page_request: PageIoRequest,
    request: BackendPageRequest,
    token: PagerResumeToken,
    pending: Vec<BlockRequestId>,
    completions: Vec<BackendBioCompletion>,
}

#[derive(Debug)]
struct BackendGraphExecution {
    request: PageIoRequest,
    scheduler: BackendGraphScheduler,
}

impl PageService {
    pub fn new(max_pending_submissions: usize) -> Self {
        Self {
            submissions: PageRequestQueue::new(max_pending_submissions),
            completions: VecDeque::new(),
            backend_resumes: VecDeque::new(),
            metadata: BTreeMap::new(),
            graphs: BTreeMap::new(),
            waiters: BTreeMap::new(),
            next: PageServiceNext::Sleeping,
        }
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
        mut frame_for: F,
    ) -> Result<PageServiceBlockCompletionOutcome, PageServiceBlockCompletionError>
    where
        F: FnMut(&BlockPageCompletion) -> Option<PageFrameRef>,
    {
        let completions = tracker.complete(completion)?;
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
        let graph = self.complete_graph_block(&completion, block_queue)?;
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
            let (queued, graph_wake, block_submitted) = graph.unwrap_or((0, None, 0));
            return Ok(PageServiceBlockCompletionOutcome {
                queued,
                wake: metadata_wake.or(graph_wake),
                block_submitted,
            });
        }
        let mut outcome = self.push_tracked_block_completion(tracker, completion, frame_for)?;
        if let Some((queued, graph_wake, block_submitted)) = graph {
            outcome.queued += queued;
            outcome.wake = metadata_wake.or(graph_wake).or(outcome.wake);
            outcome.block_submitted = block_submitted;
        } else {
            outcome.wake = metadata_wake.or(outcome.wake);
        }
        Ok(outcome)
    }

    fn complete_graph_block(
        &mut self,
        completion: &BlockCompletion,
        block_queue: &mut BlockQueue,
    ) -> Result<Option<(usize, Option<PageServiceWake>, usize)>, BackendGraphSchedulerError> {
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
        let mut block_submitted = 0usize;
        for graph_id in graph_ids {
            let advance = self
                .graphs
                .get_mut(&graph_id)
                .expect("graph id came from the registry")
                .scheduler
                .complete(completion.id, completion.result, block_queue)?;
            match advance {
                BackendGraphAdvance::Pending { submitted } => {
                    block_submitted += submitted.len();
                }
                BackendGraphAdvance::Complete(result) => {
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
        Ok(Some((queued, wake, block_submitted)))
    }

    pub fn register_metadata_continuation(
        &mut self,
        page_request: PageIoRequest,
        request: BackendPageRequest,
        token: PagerResumeToken,
        submitted: &[SubmitOutcome],
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
            self.queue_metadata_error(page_request, Errno::ENOSYS);
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
        self.register_metadata_continuation(page_request, backend_request, resume, submitted);
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
                if let Some(errno) = continuation
                    .completions
                    .iter()
                    .find_map(|completion| completion.result.err())
                {
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
        let kind = match request.op {
            PageIoOp::Read | PageIoOp::Readahead => {
                crate::io_manager::page::PageIoCompletionKind::ReadInstalled
            }
            PageIoOp::Writeback => crate::io_manager::page::PageIoCompletionKind::WritebackFinished,
            PageIoOp::Fsync => crate::io_manager::page::PageIoCompletionKind::Noop,
        };
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
        let PageServiceBackendOutcome::BlockGraph(graph) = outcome else {
            return queue_backend_outcome(outcome, block_queue, request);
        };
        if self.graphs.contains_key(&request.id) {
            return Err(PageServiceBackendSubmitError::DuplicateGraph(request.id));
        }
        let mut scheduler = BackendGraphScheduler::new(graph);
        match scheduler.start(block_queue)? {
            BackendGraphAdvance::Pending { submitted } => {
                self.graphs.insert(
                    request.id,
                    BackendGraphExecution {
                        request: request.clone(),
                        scheduler,
                    },
                );
                Ok(PageServiceBackendSubmitOutcome::BlockGraphQueued { request, submitted })
            }
            BackendGraphAdvance::Complete(result) => {
                let wake = self.queue_graph_terminal_completion(request, result);
                Ok(PageServiceBackendSubmitOutcome::QueuedPageCompletions {
                    queued: 1,
                    wake: Some(wake),
                })
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
            PageIoOp::Fsync => crate::io_manager::page::PageIoCompletionKind::Noop,
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

fn queue_bio_plans(
    block_queue: &mut BlockQueue,
    bios: BioPlanList,
) -> Result<Vec<SubmitOutcome>, PageServiceBackendSubmitError> {
    let mut submitted = Vec::new();
    for bio in bios.into_vec() {
        submitted.push(block_queue.submit(bio)?);
    }
    Ok(submitted)
}

fn queue_backend_outcome(
    outcome: PageServiceBackendOutcome,
    block_queue: &mut BlockQueue,
    request: PageIoRequest,
) -> Result<PageServiceBackendSubmitOutcome, PageServiceBackendSubmitError> {
    match outcome {
        PageServiceBackendOutcome::QueuedPageCompletions { queued, wake } => {
            Ok(PageServiceBackendSubmitOutcome::QueuedPageCompletions { queued, wake })
        }
        PageServiceBackendOutcome::BlockBios(bios) => {
            Ok(PageServiceBackendSubmitOutcome::BlockBiosQueued {
                request,
                submitted: queue_bio_plans(block_queue, bios)?,
            })
        }
        PageServiceBackendOutcome::BlockGraph(_) => {
            Ok(PageServiceBackendSubmitOutcome::Err(Errno::ENOSYS))
        }
        PageServiceBackendOutcome::MetadataFirst {
            request: backend_request,
            bios,
            resume,
        } => Ok(PageServiceBackendSubmitOutcome::MetadataFirstQueued {
            request,
            backend_request,
            submitted: queue_bio_plans(block_queue, bios)?,
            resume,
        }),
        PageServiceBackendOutcome::Yield(wait) => Ok(PageServiceBackendSubmitOutcome::Yield(wait)),
        PageServiceBackendOutcome::Err(errno) => Ok(PageServiceBackendSubmitOutcome::Err(errno)),
    }
}

fn register_metadata_outcome(
    service: &mut PageService,
    page_request: PageIoRequest,
    outcome: &PageServiceBackendSubmitOutcome,
) {
    if let PageServiceBackendSubmitOutcome::MetadataFirstQueued {
        backend_request,
        submitted,
        resume,
        ..
    } = outcome
    {
        service.register_metadata_submission(
            page_request,
            backend_request.clone(),
            *resume,
            submitted,
        );
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
                    assert!(
                        resume
                            .completions
                            .iter()
                            .all(|completion| completion.result == Ok(()))
                    );
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
        assert!(
            block_queue
                .pop_dispatchable_tagged(&mut depth, &mut tags)
                .expect("failed graph must not admit dependents")
                .is_none()
        );
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
