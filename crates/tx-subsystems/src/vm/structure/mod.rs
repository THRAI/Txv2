//! Authoritative VM state and value vocabulary.
//!
//! This module contains the VM subsystem's structure-side single source of
//! truth: AddressSpace recipes, the staged range index, RangeLock
//! coordination state, and the public value types consumed by checks and
//! execution. Mutation entrypoints live in `execution.rs`.

mod address_space;
mod range_lock;
mod recipe;
mod types;

pub use address_space::AddressSpace;
pub use range_lock::{
    AcquirePairResult, AcquireResult, LockMode, PendingWriter, RangeGuard, RangeGuardPair,
    RangeLock, WouldBlock, RANGE_LOCK_RELEASE_MASK,
};
pub use types::{
    AccessMode, AddressSpaceStats, MapPlacement, Prot, UfdRegistration, UserPage, UserPageIter,
    UserRange, UserRangeError, UserVirtAddr, VmBacking, VmEntry, VmEntryError, VmEntryFlags,
    VmEntryRewrite, VmFault, VmFaultError, VmFaultMaterialization, VmFaultMaterializationBacking,
    VmFaultOutcome, VmMapCommit, VmMapError, VmMapOutcome, VmMapRequest, VmMapTarget,
    VmRemapOutcome, VmRemapRequest, USER_PAGE_SIZE,
};

pub(in crate::vm) use recipe::{AddressSpaceStatsCell, RecipeIndex};
