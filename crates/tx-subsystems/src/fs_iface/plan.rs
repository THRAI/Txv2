//! Neutral page-to-block planning IR.

use alloc::vec::Vec;

use crate::execution::{Errno, Guard};
use crate::io_manager::block::{BioPlan, BioVec};
use crate::io_manager::page::{
    PageGeneration, PageIoCompletionKind, PageIoFlags, PageIoOp, PageIoRange, PageIoRequest,
    PageIoRequestId, PageIoResult,
};
use tx_hal::Ppn;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FsObjectKey(u64);

impl FsObjectKey {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WaitSourceId(u64);

impl WaitSourceId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PagerResumeToken(u64);

impl PagerResumeToken {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageFrameRef {
    ppn: Ppn,
}

impl PageFrameRef {
    pub const fn new(ppn: Ppn) -> Self {
        Self { ppn }
    }

    pub const fn ppn(self) -> Ppn {
        self.ppn
    }
}

/// Opaque L4 ownership token for a DMA-visible page-cache or direct-I/O source.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IoDataLeaseId(u64);

impl IoDataLeaseId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Data bytes consumed by a backend plan; ownership remains with L4.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IoDataSource {
    None,
    PageCache {
        lease: IoDataLeaseId,
        frame: PageFrameRef,
        offset: u32,
        len: u32,
    },
    Direct {
        lease: IoDataLeaseId,
        vecs: Vec<BioVec>,
    },
}

impl IoDataSource {
    pub const fn page_cache(
        lease: IoDataLeaseId,
        frame: PageFrameRef,
        offset: u32,
        len: u32,
    ) -> Self {
        Self::PageCache {
            lease,
            frame,
            offset,
            len,
        }
    }

    pub fn direct(lease: IoDataLeaseId, vecs: Vec<BioVec>) -> Self {
        Self::Direct { lease, vecs }
    }

    pub const fn lease(&self) -> Option<IoDataLeaseId> {
        match self {
            Self::None => None,
            Self::PageCache { lease, .. } | Self::Direct { lease, .. } => Some(*lease),
        }
    }
}

/// DMA-visible destination supplied by L4 for a backend read plan.
///
/// L5 may translate this target into `BioVec`s, but it never owns the frame or
/// releases the lease. Direct targets contain L4-pinned user-page vectors under
/// the same ownership rule.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IoDataTarget {
    None,
    PageCache {
        lease: IoDataLeaseId,
        frame: PageFrameRef,
        offset: u32,
        len: u32,
    },
    Direct {
        lease: IoDataLeaseId,
        vecs: Vec<BioVec>,
    },
}

impl IoDataTarget {
    pub const fn page_cache(
        lease: IoDataLeaseId,
        frame: PageFrameRef,
        offset: u32,
        len: u32,
    ) -> Self {
        Self::PageCache {
            lease,
            frame,
            offset,
            len,
        }
    }

    pub fn direct(lease: IoDataLeaseId, vecs: Vec<BioVec>) -> Self {
        Self::Direct { lease, vecs }
    }

    pub const fn lease(&self) -> Option<IoDataLeaseId> {
        match self {
            Self::None => None,
            Self::PageCache { lease, .. } | Self::Direct { lease, .. } => Some(*lease),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PageCompletion {
    pub id: PageIoRequestId,
    pub range: PageIoRange,
    pub result: PageIoResult,
    pub generation: PageGeneration,
    pub kind: PageIoCompletionKind,
    pub frame: Option<PageFrameRef>,
}

impl PageCompletion {
    pub const fn new(
        id: PageIoRequestId,
        range: PageIoRange,
        result: PageIoResult,
        generation: PageGeneration,
        kind: PageIoCompletionKind,
    ) -> Self {
        Self {
            id,
            range,
            result,
            generation,
            kind,
            frame: None,
        }
    }

    pub const fn with_frame_ref(mut self, frame: PageFrameRef) -> Self {
        self.frame = Some(frame);
        self
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PageCompletionList(Vec<PageCompletion>);

impl PageCompletionList {
    pub fn from_vec(completions: Vec<PageCompletion>) -> Self {
        Self(completions)
    }

    pub fn as_slice(&self) -> &[PageCompletion] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<PageCompletion> {
        self.0
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BioPlanList(Vec<BioPlan>);

impl BioPlanList {
    pub fn from_vec(bios: Vec<BioPlan>) -> Self {
        Self(bios)
    }

    pub fn as_slice(&self) -> &[BioPlan] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<BioPlan> {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BackendBioNodeId(u64);

impl BackendBioNodeId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendBioNode {
    pub id: BackendBioNodeId,
    pub bio: BioPlan,
    pub source: IoDataSource,
}

impl BackendBioNode {
    pub fn new(id: BackendBioNodeId, bio: BioPlan, source: IoDataSource) -> Self {
        Self { id, bio, source }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendBioDependency {
    pub before: BackendBioNodeId,
    pub after: BackendBioNodeId,
}

impl BackendBioDependency {
    pub const fn new(before: BackendBioNodeId, after: BackendBioNodeId) -> Self {
        Self { before, after }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendBioGraphError {
    DuplicateNode,
    UnknownNode,
    SelfDependency,
    Cycle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendBioGraph {
    nodes: Vec<BackendBioNode>,
    dependencies: Vec<BackendBioDependency>,
}

impl BackendBioGraph {
    pub fn new(
        nodes: Vec<BackendBioNode>,
        dependencies: Vec<BackendBioDependency>,
    ) -> Result<Self, BackendBioGraphError> {
        for (index, node) in nodes.iter().enumerate() {
            if nodes[index + 1..].iter().any(|other| other.id == node.id) {
                return Err(BackendBioGraphError::DuplicateNode);
            }
        }

        let mut indegree = alloc::vec![0usize; nodes.len()];
        for dependency in &dependencies {
            let Some(before) = node_index(&nodes, dependency.before) else {
                return Err(BackendBioGraphError::UnknownNode);
            };
            let Some(after) = node_index(&nodes, dependency.after) else {
                return Err(BackendBioGraphError::UnknownNode);
            };
            if before == after {
                return Err(BackendBioGraphError::SelfDependency);
            }
            indegree[after] += 1;
        }

        let mut remaining = alloc::vec![true; nodes.len()];
        let mut visited = 0usize;
        while let Some(next) = remaining
            .iter()
            .enumerate()
            .find_map(|(index, present)| (*present && indegree[index] == 0).then_some(index))
        {
            remaining[next] = false;
            visited += 1;
            for dependency in &dependencies {
                if node_index(&nodes, dependency.before) == Some(next) {
                    let after = node_index(&nodes, dependency.after)
                        .expect("dependency endpoint validated above");
                    indegree[after] -= 1;
                }
            }
        }

        if visited != nodes.len() {
            return Err(BackendBioGraphError::Cycle);
        }

        Ok(Self {
            nodes,
            dependencies,
        })
    }

    pub fn nodes(&self) -> &[BackendBioNode] {
        &self.nodes
    }

    pub fn dependencies(&self) -> &[BackendBioDependency] {
        &self.dependencies
    }
}

fn node_index(nodes: &[BackendBioNode], id: BackendBioNodeId) -> Option<usize> {
    nodes.iter().position(|node| node.id == id)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendBioCompletion {
    pub node: BackendBioNodeId,
    pub result: Result<(), Errno>,
}

impl BackendBioCompletion {
    pub const fn new(node: BackendBioNodeId, result: Result<(), Errno>) -> Self {
        Self { node, result }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendPlanResume {
    pub token: PagerResumeToken,
    pub completions: Vec<BackendBioCompletion>,
    pub request: Option<BackendPageRequest>,
}

impl BackendPlanResume {
    pub fn new(token: PagerResumeToken, completions: Vec<BackendBioCompletion>) -> Self {
        Self {
            token,
            completions,
            request: None,
        }
    }

    pub fn with_request(
        token: PagerResumeToken,
        completions: Vec<BackendBioCompletion>,
        request: BackendPageRequest,
    ) -> Self {
        Self {
            token,
            completions,
            request: Some(request),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendPlan {
    Complete(PageCompletionList),
    SubmitBios(BioPlanList),
    SubmitGraph(BackendBioGraph),
    MetadataFirst {
        request: BackendPageRequest,
        bios: BioPlanList,
        resume: PagerResumeToken,
    },
    Yield(WaitSourceId),
    Err(Errno),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendPageRequest {
    pub object: FsObjectKey,
    pub id: PageIoRequestId,
    pub range: PageIoRange,
    pub op: PageIoOp,
    pub flags: PageIoFlags,
    pub generation_hint: Option<PageGeneration>,
    pub source: IoDataSource,
    pub target: IoDataTarget,
}

impl BackendPageRequest {
    pub const fn new(
        object: FsObjectKey,
        id: PageIoRequestId,
        range: PageIoRange,
        op: PageIoOp,
        flags: PageIoFlags,
        generation_hint: Option<PageGeneration>,
    ) -> Self {
        Self {
            object,
            id,
            range,
            op,
            flags,
            generation_hint,
            source: IoDataSource::None,
            target: IoDataTarget::None,
        }
    }

    pub fn new_with_source(
        object: FsObjectKey,
        id: PageIoRequestId,
        range: PageIoRange,
        op: PageIoOp,
        flags: PageIoFlags,
        generation_hint: Option<PageGeneration>,
        source: IoDataSource,
    ) -> Self {
        Self::new_with_source_and_target(
            object,
            id,
            range,
            op,
            flags,
            generation_hint,
            source,
            IoDataTarget::None,
        )
    }

    pub fn new_with_source_and_target(
        object: FsObjectKey,
        id: PageIoRequestId,
        range: PageIoRange,
        op: PageIoOp,
        flags: PageIoFlags,
        generation_hint: Option<PageGeneration>,
        source: IoDataSource,
        target: IoDataTarget,
    ) -> Self {
        Self {
            object,
            id,
            range,
            op,
            flags,
            generation_hint,
            source,
            target,
        }
    }

    pub const fn from_page_io_request(object: FsObjectKey, request: PageIoRequest) -> Self {
        Self::new(
            object,
            request.id,
            request.range,
            request.op,
            request.flags,
            request.generation_hint,
        )
    }

    pub fn from_page_io_request_with_source(
        object: FsObjectKey,
        request: PageIoRequest,
        source: IoDataSource,
    ) -> Self {
        Self::new_with_source(
            object,
            request.id,
            request.range,
            request.op,
            request.flags,
            request.generation_hint,
            source,
        )
    }
}

/// Terminal notification for one L4 request planned by a concrete backend.
///
/// Completion is delivered only after PageBacked has installed its own local
/// state transition, so a backend may release journal record leases or queue
/// post-commit work without relying on a PageContainer lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackendPageCompletion {
    pub object: FsObjectKey,
    pub id: PageIoRequestId,
    pub op: PageIoOp,
    pub result: PageIoResult,
}

impl BackendPageCompletion {
    pub const fn new(
        object: FsObjectKey,
        id: PageIoRequestId,
        op: PageIoOp,
        result: PageIoResult,
    ) -> Self {
        Self {
            object,
            id,
            op,
            result,
        }
    }
}

pub trait BackendPlanner: Send + Sync + 'static {
    /// Admit an L4-owned source into backend-private state before planning.
    ///
    /// This synchronous hook is the only planner entry that receives an epoch
    /// guard. Backends may copy immutable metadata or stage private journal
    /// records here, but must not retain the guard or perform device I/O.
    fn prepare_page_io(
        &self,
        _request: &BackendPageRequest,
        _guard: &Guard<'_>,
    ) -> Result<(), Errno> {
        Ok(())
    }

    fn plan_page_io(&self, request: BackendPageRequest) -> BackendPlan;

    fn resume_page_io(&self, _resume: BackendPlanResume) -> BackendPlan {
        BackendPlan::Err(Errno::ENOSYS)
    }

    fn complete_page_io(&self, _completion: BackendPageCompletion) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::Errno;
    use crate::io_manager::block::{BioPlan, BioVec, BlockFlags, BlockOp, DeviceKey, LbaRange};

    fn node(id: u64) -> BackendBioNode {
        BackendBioNode::new(
            BackendBioNodeId::new(id),
            BioPlan::new(
                DeviceKey::new(1),
                BlockOp::Write,
                LbaRange::new(id, 1),
                alloc::vec![BioVec::new(id, 0, 512)],
                BlockFlags::EMPTY,
            ),
            IoDataSource::None,
        )
    }

    #[test]
    fn backend_page_request_keeps_direct_write_source() {
        let source =
            IoDataSource::direct(IoDataLeaseId::new(7), alloc::vec![BioVec::new(9, 128, 512)]);
        let request = BackendPageRequest::new_with_source(
            FsObjectKey::new(2),
            PageIoRequestId::new(3),
            PageIoRange::new(0, 1),
            PageIoOp::Writeback,
            PageIoFlags::WRITEBACK,
            Some(PageGeneration::new(4)),
            source.clone(),
        );

        assert_eq!(request.source, source);
        assert_eq!(request.target, IoDataTarget::None);
    }

    #[test]
    fn backend_page_request_keeps_page_cache_read_target() {
        let target =
            IoDataTarget::page_cache(IoDataLeaseId::new(8), PageFrameRef::new(Ppn(9)), 0, 4096);
        let request = BackendPageRequest::new_with_source_and_target(
            FsObjectKey::new(2),
            PageIoRequestId::new(4),
            PageIoRange::new(1, 1),
            PageIoOp::Read,
            PageIoFlags::EMPTY,
            Some(PageGeneration::new(5)),
            IoDataSource::None,
            target.clone(),
        );

        assert_eq!(request.target, target);
    }

    #[test]
    fn backend_page_request_keeps_direct_read_target() {
        let target = IoDataTarget::direct(
            IoDataLeaseId::new(10),
            alloc::vec![BioVec::new(11, 256, 1024)],
        );
        let request = BackendPageRequest::new_with_source_and_target(
            FsObjectKey::new(2),
            PageIoRequestId::new(5),
            PageIoRange::new(2, 1),
            PageIoOp::Read,
            PageIoFlags::EMPTY,
            Some(PageGeneration::new(6)),
            IoDataSource::None,
            target.clone(),
        );

        assert_eq!(request.target, target);
        assert_eq!(request.target.lease(), Some(IoDataLeaseId::new(10)));
    }

    #[test]
    fn backend_bio_graph_keeps_data_before_commit_dependency() {
        let graph = BackendBioGraph::new(
            alloc::vec![node(1), node(2)],
            alloc::vec![BackendBioDependency::new(
                BackendBioNodeId::new(1),
                BackendBioNodeId::new(2),
            )],
        )
        .expect("acyclic graph");

        assert_eq!(graph.nodes().len(), 2);
        assert_eq!(graph.dependencies().len(), 1);
    }

    #[test]
    fn backend_bio_graph_rejects_a_cycle() {
        let graph = BackendBioGraph::new(
            alloc::vec![node(1), node(2)],
            alloc::vec![
                BackendBioDependency::new(BackendBioNodeId::new(1), BackendBioNodeId::new(2)),
                BackendBioDependency::new(BackendBioNodeId::new(2), BackendBioNodeId::new(1)),
            ],
        );

        assert_eq!(graph, Err(BackendBioGraphError::Cycle));
    }

    struct RequestOnlyPlanner;

    impl BackendPlanner for RequestOnlyPlanner {
        fn plan_page_io(&self, _request: BackendPageRequest) -> BackendPlan {
            BackendPlan::Err(Errno::EIO)
        }
    }

    #[test]
    fn backend_planner_default_resume_is_explicitly_unsupported() {
        let resumed = RequestOnlyPlanner.resume_page_io(BackendPlanResume::new(
            PagerResumeToken::new(12),
            alloc::vec![BackendBioCompletion::new(BackendBioNodeId::new(2), Ok(()))],
        ));

        assert_eq!(resumed, BackendPlan::Err(Errno::ENOSYS));
    }
}
