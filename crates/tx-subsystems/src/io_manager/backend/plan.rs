//! Re-export of neutral backend-plan values.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::io_manager::block::{BlockCompletion, BlockRequestId, SubmitOutcome};
use crate::io_manager::page::{
    PageIoCompletion, PageIoCompletionKind, PageIoOp, PageIoRequest, PageIoResult,
};

pub use crate::fs_iface::plan::{
    BackendBioCompletion, BackendBioDependency, BackendBioGraph, BackendBioGraphError,
    BackendBioNode, BackendBioNodeId, BackendPageRequest, BackendPlan, BackendPlanResume,
    BackendPlanner, BioPlanList, FsObjectKey, IoDataLeaseId, IoDataSource, IoDataTarget,
    PageCacheSegment, PageCompletion, PageCompletionList, PageFrameRef, PagerResumeToken,
    WaitSourceId,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageIoCompletionEntry {
    pub completion: PageIoCompletion,
    pub frame: Option<PageFrameRef>,
}

impl PageIoCompletionEntry {
    pub const fn new(completion: PageIoCompletion, frame: Option<PageFrameRef>) -> Self {
        Self { completion, frame }
    }

    pub fn from_page_completion(completion: PageCompletion) -> Self {
        Self {
            completion: page_completion_to_io_completion(&completion),
            frame: completion.frame,
        }
    }
}

impl core::ops::Deref for PageIoCompletionEntry {
    type Target = PageIoCompletion;

    fn deref(&self) -> &Self::Target {
        &self.completion
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlockPageCompletion {
    request: PageIoRequest,
    completion: BlockCompletion,
    frame: Option<PageFrameRef>,
}

impl BlockPageCompletion {
    pub const fn new(request: PageIoRequest, completion: BlockCompletion) -> Self {
        Self {
            request,
            completion,
            frame: None,
        }
    }

    pub const fn with_frame(mut self, frame: PageFrameRef) -> Self {
        self.frame = Some(frame);
        self
    }

    pub const fn request(&self) -> &PageIoRequest {
        &self.request
    }

    pub const fn block_completion(&self) -> &BlockCompletion {
        &self.completion
    }

    pub fn into_page_completion(self) -> Result<PageIoCompletionEntry, BlockPageCompletionError> {
        let generation = self
            .request
            .generation_hint
            .ok_or(BlockPageCompletionError::MissingGeneration)?;
        let kind = completion_kind_for_request(self.request.op);
        let result = match self.completion.result {
            Ok(()) => PageIoResult::Done,
            Err(errno) => PageIoResult::Err(errno),
        };
        let frame = if requires_read_frame(self.request.op, result) {
            Some(
                self.frame
                    .ok_or(BlockPageCompletionError::MissingReadFrame)?,
            )
        } else {
            self.frame
        };
        Ok(PageIoCompletionEntry::new(
            PageIoCompletion::new(
                self.request.id,
                self.request.range,
                result,
                generation,
                kind,
            ),
            frame,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockPageCompletionError {
    MissingGeneration,
    MissingReadFrame,
}

#[derive(Debug, Default)]
pub struct BlockPageRequestTracker {
    pending: BTreeMap<BlockRequestId, Vec<PageIoRequest>>,
}

impl BlockPageRequestTracker {
    pub const fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn contains(&self, id: BlockRequestId) -> bool {
        self.pending.contains_key(&id)
    }

    pub fn record_submit_outcomes(&mut self, request: PageIoRequest, outcomes: &[SubmitOutcome]) {
        for outcome in outcomes {
            let id = block_request_id_for_submit_outcome(*outcome);
            self.pending.entry(id).or_default().push(request.clone());
        }
    }

    pub fn complete(
        &mut self,
        completion: BlockCompletion,
    ) -> Result<Vec<BlockPageCompletion>, BlockPageRequestTrackerError> {
        let Some(requests) = self.pending.remove(&completion.id) else {
            return Err(BlockPageRequestTrackerError::UnknownBlockRequest(
                completion.id,
            ));
        };
        Ok(requests
            .into_iter()
            .map(|request| BlockPageCompletion::new(request, completion.clone()))
            .collect())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockPageRequestTrackerError {
    UnknownBlockRequest(BlockRequestId),
}

const fn block_request_id_for_submit_outcome(outcome: SubmitOutcome) -> BlockRequestId {
    match outcome {
        SubmitOutcome::Queued(id) | SubmitOutcome::Merged(id) => id,
    }
}

const fn completion_kind_for_request(op: PageIoOp) -> PageIoCompletionKind {
    match op {
        PageIoOp::Read | PageIoOp::Readahead => PageIoCompletionKind::ReadInstalled,
        PageIoOp::Writeback => PageIoCompletionKind::WritebackFinished,
        PageIoOp::Fsync | PageIoOp::Checkpoint => PageIoCompletionKind::Noop,
    }
}

const fn requires_read_frame(op: PageIoOp, result: PageIoResult) -> bool {
    matches!(op, PageIoOp::Read | PageIoOp::Readahead) && matches!(result, PageIoResult::Done)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageIoCompletionList(Vec<PageIoCompletionEntry>);

impl PageIoCompletionList {
    pub fn from_vec(completions: Vec<PageIoCompletion>) -> Self {
        Self(
            completions
                .into_iter()
                .map(|completion| PageIoCompletionEntry::new(completion, None))
                .collect(),
        )
    }

    pub fn from_page_completions(completions: PageCompletionList) -> Self {
        Self(
            completions
                .into_vec()
                .into_iter()
                .map(PageIoCompletionEntry::from_page_completion)
                .collect(),
        )
    }

    pub fn as_slice(&self) -> &[PageIoCompletionEntry] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<PageIoCompletionEntry> {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendDispatch {
    PageCompletions(PageIoCompletionList),
    BlockBios(BioPlanList),
    BlockGraph(BackendBioGraph),
    MetadataFirst {
        request: BackendPageRequest,
        bios: BioPlanList,
        resume: PagerResumeToken,
    },
    Yield(WaitSourceId),
    Err(crate::execution::Errno),
}

pub fn plan_backend_request<P>(planner: &P, request: BackendPageRequest) -> BackendPlan
where
    P: BackendPlanner + ?Sized,
{
    planner.plan_page_io(request)
}

pub fn dispatch_backend_plan(plan: BackendPlan) -> BackendDispatch {
    match plan {
        BackendPlan::Complete(completions) => BackendDispatch::PageCompletions(
            PageIoCompletionList::from_page_completions(completions),
        ),
        BackendPlan::SubmitBios(bios) => BackendDispatch::BlockBios(bios),
        BackendPlan::SubmitGraph(graph) => BackendDispatch::BlockGraph(graph),
        BackendPlan::MetadataFirst {
            request,
            bios,
            resume,
        } => BackendDispatch::MetadataFirst {
            request,
            bios,
            resume,
        },
        BackendPlan::Yield(wait) => BackendDispatch::Yield(wait),
        BackendPlan::Err(errno) => BackendDispatch::Err(errno),
    }
}

fn page_completion_to_io_completion(completion: &PageCompletion) -> PageIoCompletion {
    PageIoCompletion::new(
        completion.id,
        completion.range,
        completion.result,
        completion.generation,
        completion.kind,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::Errno;
    use crate::fs_iface::plan::{
        BackendBioDependency, BackendBioGraph, BackendBioNode, BackendBioNodeId, IoDataSource,
    };
    use crate::io_manager::block::{
        BioPlan, BioVec, BlockCompletion, BlockFlags, BlockOp, BlockRequestId, BlockTag, DeviceKey,
        LbaRange, SubmitOutcome,
    };
    use crate::io_manager::page::{
        PageContainerKey, PageGeneration, PageIoCompletionKind, PageIoFlags, PageIoOp,
        PageIoPriority, PageIoRange, PageIoRequest, PageIoRequestId, PageIoResult,
    };
    use tx_hal::Ppn;

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

    struct BioPlanner;

    impl BackendPlanner for BioPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![BioPlan::new(
                DeviceKey::new(55),
                BlockOp::Read,
                LbaRange::new(128, 1),
                alloc::vec![BioVec::new(91, 0, 4096)],
                BlockFlags::EMPTY,
            )]))
        }
    }

    fn backend_request() -> BackendPageRequest {
        BackendPageRequest::new(
            FsObjectKey::new(77),
            PageIoRequestId::new(21),
            PageIoRange::new(40, 2),
            PageIoOp::Read,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(12)),
        )
    }

    #[test]
    fn backend_planner_seam_invokes_neutral_planner_and_dispatches_completion() {
        let plan = plan_backend_request(&CompletePlanner, backend_request());
        let dispatch = dispatch_backend_plan(plan);

        match dispatch {
            BackendDispatch::PageCompletions(completions) => {
                let completions = completions.into_vec();
                assert_eq!(completions.len(), 1);
                assert_eq!(completions[0].id, PageIoRequestId::new(21));
                assert_eq!(completions[0].range, PageIoRange::new(40, 2));
                assert_eq!(completions[0].generation, PageGeneration::new(12));
                assert_eq!(completions[0].kind, PageIoCompletionKind::ReadInstalled);
            }
            other => panic!("expected page completions, got {other:?}"),
        }
    }

    #[test]
    fn backend_planner_seam_preserves_submit_bios_for_l6_dispatch() {
        let plan = plan_backend_request(&BioPlanner, backend_request());
        let dispatch = dispatch_backend_plan(plan);

        match dispatch {
            BackendDispatch::BlockBios(bios) => {
                assert_eq!(bios.as_slice().len(), 1);
                assert_eq!(bios.as_slice()[0].device, DeviceKey::new(55));
                assert_eq!(bios.as_slice()[0].lba, LbaRange::new(128, 1));
            }
            other => panic!("expected block bios, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_backend_plan_preserves_submit_graph() {
        let data = BackendBioNode::new(
            BackendBioNodeId::new(1),
            BioPlan::new(
                DeviceKey::new(3),
                BlockOp::Write,
                LbaRange::new(16, 1),
                alloc::vec![BioVec::new(44, 0, 512)],
                BlockFlags::EMPTY,
            ),
            IoDataSource::None,
        );
        let commit = BackendBioNode::new(
            BackendBioNodeId::new(2),
            BioPlan::new(
                DeviceKey::new(3),
                BlockOp::Barrier,
                LbaRange::new(0, 0),
                alloc::vec![],
                BlockFlags::BARRIER,
            ),
            IoDataSource::None,
        );
        let graph = BackendBioGraph::new(
            alloc::vec![data, commit],
            alloc::vec![BackendBioDependency::new(
                BackendBioNodeId::new(1),
                BackendBioNodeId::new(2),
            )],
        )
        .expect("ordered graph");

        assert_eq!(
            dispatch_backend_plan(BackendPlan::SubmitGraph(graph.clone())),
            BackendDispatch::BlockGraph(graph),
        );
    }

    #[test]
    fn io_manager_backend_plan_reexports_neutral_completion_plan() {
        let completion = PageCompletion::new(
            PageIoRequestId::new(9),
            PageIoRange::new(4, 1),
            PageIoResult::Err(Errno::EIO),
            PageGeneration::new(2),
            PageIoCompletionKind::Noop,
        );
        let plan = BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![completion]));

        match plan {
            BackendPlan::Complete(list) => {
                assert_eq!(list.as_slice()[0].id, PageIoRequestId::new(9));
            }
            _ => panic!("expected completion plan"),
        }
    }

    #[test]
    fn backend_plan_dispatches_complete_to_page_completion_work() {
        let completion = PageCompletion::new(
            PageIoRequestId::new(10),
            PageIoRange::new(5, 1),
            PageIoResult::Done,
            PageGeneration::new(8),
            PageIoCompletionKind::ReadInstalled,
        );
        let plan = BackendPlan::Complete(PageCompletionList::from_vec(alloc::vec![completion]));

        let dispatch = dispatch_backend_plan(plan);

        match dispatch {
            BackendDispatch::PageCompletions(completions) => {
                let completions = completions.into_vec();
                assert_eq!(completions.len(), 1);
                assert_eq!(completions[0].id, PageIoRequestId::new(10));
                assert_eq!(completions[0].range, PageIoRange::new(5, 1));
                assert_eq!(completions[0].kind, PageIoCompletionKind::ReadInstalled);
            }
            other => panic!("expected page completions, got {other:?}"),
        }
    }

    #[test]
    fn backend_plan_dispatches_bios_to_block_work_without_page_completion() {
        let bio = BioPlan::new(
            DeviceKey::new(3),
            BlockOp::Read,
            LbaRange::new(16, 2),
            alloc::vec![BioVec::new(44, 0, 1024)],
            BlockFlags::EMPTY,
        );
        let plan = BackendPlan::SubmitBios(BioPlanList::from_vec(alloc::vec![bio.clone()]));

        let dispatch = dispatch_backend_plan(plan);

        match dispatch {
            BackendDispatch::BlockBios(bios) => {
                assert_eq!(bios.as_slice(), &[bio]);
            }
            other => panic!("expected block bios, got {other:?}"),
        }
    }

    #[test]
    fn backend_plan_dispatch_preserves_metadata_resume_token() {
        let bio = BioPlan::new(
            DeviceKey::new(3),
            BlockOp::Read,
            LbaRange::new(32, 1),
            alloc::vec![BioVec::new(45, 0, 512)],
            BlockFlags::EMPTY,
        );
        let resume = PagerResumeToken::new(99);
        let request = BackendPageRequest::new(
            FsObjectKey::new(3),
            crate::io_manager::page::PageIoRequestId::new(8),
            crate::io_manager::page::PageIoRange::new(4, 1),
            crate::io_manager::page::PageIoOp::Read,
            crate::io_manager::page::PageIoFlags::DEMAND,
            Some(crate::io_manager::page::PageGeneration::new(1)),
        );
        let plan = BackendPlan::MetadataFirst {
            request,
            bios: BioPlanList::from_vec(alloc::vec![bio.clone()]),
            resume,
        };

        let dispatch = dispatch_backend_plan(plan);

        match dispatch {
            BackendDispatch::MetadataFirst {
                bios, resume: seen, ..
            } => {
                assert_eq!(bios.as_slice(), &[bio]);
                assert_eq!(seen, resume);
            }
            other => panic!("expected metadata-first dispatch, got {other:?}"),
        }
    }

    #[test]
    fn block_completion_bridge_maps_read_frame_to_page_completion() {
        let request = PageIoRequest::new(
            PageIoRequestId::new(77),
            PageContainerKey::new(5),
            PageIoRange::new(9, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(33)),
        );
        let plan = BioPlan::new(
            DeviceKey::new(3),
            BlockOp::Read,
            LbaRange::new(72, 1),
            alloc::vec![BioVec::new(90, 0, 4096)],
            BlockFlags::EMPTY,
        );
        let completion = BlockCompletion {
            tag: BlockTag::new(8),
            id: BlockRequestId::new(19),
            plan,
            result: Ok(()),
        };

        let entry = BlockPageCompletion::new(request, completion)
            .with_frame(PageFrameRef::new(Ppn(0x44)))
            .into_page_completion()
            .expect("page completion");

        assert_eq!(entry.id, PageIoRequestId::new(77));
        assert_eq!(entry.range, PageIoRange::new(9, 1));
        assert_eq!(entry.result, PageIoResult::Done);
        assert_eq!(entry.generation, PageGeneration::new(33));
        assert_eq!(entry.kind, PageIoCompletionKind::ReadInstalled);
        assert_eq!(entry.frame, Some(PageFrameRef::new(Ppn(0x44))));
    }

    #[test]
    fn block_completion_bridge_rejects_read_success_without_frame() {
        let request = PageIoRequest::new(
            PageIoRequestId::new(78),
            PageContainerKey::new(5),
            PageIoRange::new(10, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(34)),
        );
        let plan = BioPlan::new(
            DeviceKey::new(3),
            BlockOp::Read,
            LbaRange::new(80, 1),
            alloc::vec![BioVec::new(91, 0, 4096)],
            BlockFlags::EMPTY,
        );
        let completion = BlockCompletion {
            tag: BlockTag::new(9),
            id: BlockRequestId::new(20),
            plan,
            result: Ok(()),
        };

        let error = BlockPageCompletion::new(request, completion)
            .into_page_completion()
            .expect_err("read completion needs frame");

        assert_eq!(error, BlockPageCompletionError::MissingReadFrame);
    }

    #[test]
    fn backend_facade_reexports_block_completion_bridge() {
        let _ = crate::io_manager::backend::BlockPageCompletionError::MissingGeneration;
        let _ = crate::io_manager::backend::BlockPageRequestTracker::new();
    }

    #[test]
    fn block_page_request_tracker_restores_request_for_block_completion() {
        let mut tracker = BlockPageRequestTracker::new();
        let request = PageIoRequest::new(
            PageIoRequestId::new(90),
            PageContainerKey::new(5),
            PageIoRange::new(10, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(7)),
        );
        tracker.record_submit_outcomes(request, &[SubmitOutcome::Queued(BlockRequestId::new(55))]);
        let completion = BlockCompletion {
            tag: BlockTag::new(3),
            id: BlockRequestId::new(55),
            plan: BioPlan::new(
                DeviceKey::new(8),
                BlockOp::Read,
                LbaRange::new(100, 1),
                alloc::vec![BioVec::new(1, 0, 4096)],
                BlockFlags::EMPTY,
            ),
            result: Err(Errno::EIO),
        };

        let bridged = tracker.complete(completion).expect("tracked request");

        assert_eq!(bridged.len(), 1);
        let entry = bridged
            .into_iter()
            .next()
            .expect("bridge")
            .into_page_completion()
            .expect("page error completion");
        assert_eq!(entry.id, PageIoRequestId::new(90));
        assert_eq!(entry.range, PageIoRange::new(10, 1));
        assert_eq!(entry.generation, PageGeneration::new(7));
        assert_eq!(entry.result, PageIoResult::Err(Errno::EIO));
        assert_eq!(entry.frame, None);
    }

    #[test]
    fn block_page_request_tracker_preserves_merged_page_requests() {
        let mut tracker = BlockPageRequestTracker::new();
        let first = PageIoRequest::new(
            PageIoRequestId::new(91),
            PageContainerKey::new(5),
            PageIoRange::new(10, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(8)),
        );
        let second = PageIoRequest::new(
            PageIoRequestId::new(92),
            PageContainerKey::new(5),
            PageIoRange::new(11, 1),
            PageIoOp::Read,
            PageIoPriority::Demand,
            PageIoFlags::DEMAND,
            Some(PageGeneration::new(9)),
        );
        tracker.record_submit_outcomes(first, &[SubmitOutcome::Queued(BlockRequestId::new(56))]);
        tracker.record_submit_outcomes(second, &[SubmitOutcome::Merged(BlockRequestId::new(56))]);
        let completion = BlockCompletion {
            tag: BlockTag::new(4),
            id: BlockRequestId::new(56),
            plan: BioPlan::new(
                DeviceKey::new(8),
                BlockOp::Read,
                LbaRange::new(100, 2),
                alloc::vec![BioVec::new(1, 0, 8192)],
                BlockFlags::EMPTY,
            ),
            result: Err(Errno::EIO),
        };

        let bridged = tracker.complete(completion).expect("tracked requests");

        assert_eq!(bridged.len(), 2);
        let mut ids = bridged.into_iter().map(|completion| {
            completion
                .into_page_completion()
                .expect("page completion")
                .id
        });
        assert_eq!(ids.next(), Some(PageIoRequestId::new(91)));
        assert_eq!(ids.next(), Some(PageIoRequestId::new(92)));
        assert_eq!(ids.next(), None);
    }
}
