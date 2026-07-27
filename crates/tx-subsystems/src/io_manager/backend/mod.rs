//! L5 backend-planning facade.

pub mod graph;
pub mod plan;

pub use plan::{
    dispatch_backend_plan, plan_backend_request, BackendBioCompletion, BackendBioDependency,
    BackendBioGraph, BackendBioGraphError, BackendBioNode, BackendBioNodeId, BackendDispatch,
    BackendPageRequest, BackendPlan, BackendPlanResume, BackendPlanner, BioPlanList,
    BlockPageCompletion, BlockPageCompletionError, BlockPageRequestTracker,
    BlockPageRequestTrackerError, FsObjectKey, IoDataLeaseId, IoDataSource, IoDataTarget,
    PageCompletion, PageCompletionList, PageFrameRef, PageIoCompletionEntry, PageIoCompletionList,
    PagerResumeToken, WaitSourceId,
};

pub use graph::{BackendGraphAdvance, BackendGraphScheduler, BackendGraphSchedulerError};
