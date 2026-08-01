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
pub(in crate::vm) mod recipe_tree;
mod types;

pub use address_space::AddressSpace;
pub use private::{
    PrivateFrame, PrivateFrameIdentity, PrivateFrameSnapshot, PrivateFrameState, PrivatePageError,
    PrivatePageSet, VmPageOff,
};
pub use range_lock::{
    AcquirePairResult, AcquireResult, LockMode, PendingWriter, RangeGuard, RangeGuardPair,
    RangeLock, RangeLockDiagnosticSnapshot, WouldBlock, RANGE_LOCK_RELEASE_MASK,
};
pub use types::{
    AccessMode, AddressSpaceStats, MapPlacement, Prot, UfdRegistration, UserPage, UserPageIter,
    UserRange, UserRangeError, UserVirtAddr, VmBacking, VmCap, VmEntry, VmEntryBacking,
    VmEntryError, VmEntryFlags, VmEntryProtectRewrite, VmEntryRewrite, VmFault, VmFaultError,
    VmFaultMaterialization, VmFaultMaterializationBacking, VmFaultMaterializationStep,
    VmFaultOutcome, VmMapCommit, VmMapError, VmMapOutcome, VmMapRequest, VmMapTarget,
    VmRemapOutcome, VmRemapPlacement, VmRemapRequest, VmSpecialBacking, FULL_USER_V1_TOP,
    USER_PAGE_SIZE,
};

pub(in crate::vm) use private::{
    private_page_debug_samples, private_page_debug_totals, reset_private_page_debug_totals,
};
pub(in crate::vm) use recipe::{
    drain_deferred_recipe_reclaims, recipe_debug_totals, reset_recipe_debug_totals,
    AddressSpaceStatsCell, RecipeIndex,
};
pub(in crate::vm) use types::AddressSpaceStatsDelta;
