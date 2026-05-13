//! VM subsystem facade arranged by subsystem anatomy.
//!
//! `structure/` owns authoritative AddressSpace recipes, RangeLock state,
//! and VM value vocabulary. `checks.rs` hosts pure observation predicates,
//! `execution.rs` hosts mutating step-like flows, and `project.rs` is the
//! read-only projection home for future procfs/sysfs adapters. Persistent
//! epoch recipe snapshots remain a staged seam; pmap materialization owns
//! HAL `PmapIf` root evidence through `pmap.rs`.

pub mod adapter;
pub mod checks;
pub mod execution;
mod pmap;
pub mod project;
pub mod scripts;
mod structure;
mod user_access;

#[cfg(test)]
pub(crate) use pmap::TestPmap;

#[cfg(test)]
mod tests;

pub use execution::{
    MadviseAdvice, MapReservation, MapReserveResult, NullUfdDispatch, UfdDispatch,
    UfdDispatchTarget,
};
pub use pmap::{PmapMappingSnapshot, PmapPublishOutcome, PmapStats, VmPmapError};
pub use scripts::{
    build_aspace_from_image, populate_detached_user_range, BssTail, ImagePlan, LoadSegment,
    ScriptError, SegmentFlags, USER_STACK_INITIAL_RESERVATION, USER_STACK_TOP_DEFAULT,
};
pub use structure::{
    AccessMode, AcquirePairResult, AcquireResult, AddressSpace, AddressSpaceStats, LockMode,
    MapPlacement, PendingWriter, PrivateFrame, PrivateFrameIdentity, PrivateFrameSnapshot,
    PrivateFrameState, PrivatePageError, PrivatePageSet, Prot, RangeGuard, RangeGuardPair,
    RangeLock, UfdRegistration, UserPage, UserPageIter, UserRange, UserRangeError, UserVirtAddr,
    VmBacking, VmEntry, VmEntryError, VmEntryFlags, VmEntryRewrite, VmFault, VmFaultError,
    VmFaultMaterialization, VmFaultMaterializationBacking, VmFaultOutcome, VmMapCommit, VmMapError,
    VmMapOutcome, VmMapRequest, VmMapTarget, VmPageOff, VmRemapOutcome, VmRemapRequest, WouldBlock,
    RANGE_LOCK_RELEASE_MASK, USER_PAGE_SIZE,
};
pub use user_access::UserAccessKind;
