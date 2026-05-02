//! VM subsystem facade arranged by subsystem anatomy.
//!
//! `structure/` owns authoritative AddressSpace recipes, RangeLock state,
//! and VM value vocabulary. `checks.rs` hosts pure observation predicates,
//! `execution.rs` hosts mutating step-like flows, and `project.rs` is the
//! read-only projection home for future procfs/sysfs adapters. Persistent
//! epoch recipe snapshots remain a staged seam; pmap materialization owns
//! HAL `PmapIf` root evidence through `pmap.rs`.

pub mod checks;
mod execution;
mod pmap;
pub mod project;
mod structure;

#[cfg(test)]
mod tests;

pub use execution::{MapReservation, MapReserveResult};
pub use pmap::{PmapMappingSnapshot, PmapPublishOutcome, PmapStats, VmPmapError};
pub use structure::{
    AccessMode, AcquirePairResult, AcquireResult, AddressSpace, AddressSpaceStats, LockMode,
    MapPlacement, PendingWriter, Prot, RangeGuard, RangeGuardPair, RangeLock, UserPage,
    UserPageIter, UserRange, UserRangeError, UserVirtAddr, VmBacking, VmEntry, VmEntryError,
    VmEntryFlags, VmEntryRewrite, VmFault, VmFaultError, VmFaultMaterialization, VmFaultOutcome,
    VmMapCommit, VmMapError, VmMapOutcome, VmMapRequest, VmMapTarget, VmRemapOutcome,
    VmRemapRequest, WouldBlock, USER_PAGE_SIZE,
};
