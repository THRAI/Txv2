//! Filesystem-neutral I/O planning values.
//!
//! Concrete filesystems implement planning in their own crates; this module
//! contains only the IR shared with the I/O manager.

pub mod plan;

pub use plan::{
    BackendBioCompletion, BackendBioDependency, BackendBioGraph, BackendBioGraphError,
    BackendBioNode, BackendBioNodeId, BackendPageCompletion, BackendPageRequest, BackendPlan,
    BackendPlanResume, BackendPlanner, BioPlanList, FsObjectKey, IoDataLeaseId, IoDataSource,
    IoDataTarget, PageCompletion, PageCompletionList, PageFrameRef, PagerResumeToken, WaitSourceId,
};
