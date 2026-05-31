//! Authoritative VM state and value vocabulary.
//!
//! This module contains the VM subsystem's structure-side single source of
//! truth: AddressSpace recipes, the staged range index, RangeLock
//! coordination state, and the public value types consumed by checks and
//! execution. Mutation entrypoints live in `execution.rs`.

mod address_space;
mod private;
mod range_lock;
mod recipe;
mod types;

pub use address_space::AddressSpace;
pub use private::{
    PrivateFrame, PrivateFrameIdentity, PrivateFrameSnapshot, PrivateFrameState, PrivatePageError,
    PrivatePageSet, VmPageOff,
};
pub use range_lock::{
    AcquirePairResult, AcquireResult, LockMode, PendingWriter, RangeGuard, RangeGuardPair,
    RangeLock, WouldBlock, RANGE_LOCK_RELEASE_MASK,
};
pub use types::{
    AccessMode, AddressSpaceStats, MapPlacement, Prot, UfdRegistration, UserPage, UserPageIter,
    UserRange, UserRangeError, UserVirtAddr, VmBacking, VmEntry, VmEntryError, VmEntryFlags,
    VmEntryRewrite, VmFault, VmFaultError, VmFaultMaterialization, VmFaultMaterializationBacking,
    VmFaultMaterializationStep, VmFaultOutcome, VmMapCommit, VmMapError, VmMapOutcome,
    VmMapRequest, VmMapTarget, VmRemapOutcome, VmRemapPlacement, VmRemapRequest, FULL_USER_V1_TOP,
    USER_PAGE_SIZE,
};

pub(in crate::vm) use private::{
    private_page_debug_samples, private_page_debug_totals, reset_private_page_debug_totals,
};
pub(in crate::vm) use recipe::{AddressSpaceStatsCell, RecipeIndex};
pub(in crate::vm) use types::AddressSpaceStatsDelta;
