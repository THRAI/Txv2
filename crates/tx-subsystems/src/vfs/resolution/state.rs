//! Walker state vocabulary per `txdoc:VFS-CHECKS-WALKSTATE-1` (§5–§6).
//!
//! The resolution state machine uses these types to drive
//! component-by-component path walks.  `Walking` carries the
//! full mutable walker frame; `kernel_step` advances one
//! component per call; the driver loops until terminal or error.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::vfs::adapter::step_engine::Cap;
use crate::vfs::{DEntry, FsObjectId, FsOps, InodeMeta, RNode};
use crate::mount::{MountPayload, MountIdentity};

// ============================================================================
// WalkMode
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkMode {
    Entity,
    EntityUnfollowed,
    ParentAndName,
    ParentAndNamedChild,
    EntityOrParentAndName,
    MountPoint,
}

// ============================================================================
// WalkCause
// ============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkCause {
    TraverseDenied,
    ComponentNotFound,
    NotADirectory,
    SymlinkLimit,
    MountPointGap,
    FsOpsRejected(crate::execution::Errno),
    Permission(NonTerminalDenial),
    TerminalOpenFailed(crate::execution::Errno),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonTerminalDenial {
    SearchDenied,
}

// ============================================================================
// WalkState
// ============================================================================

#[derive(Clone, Debug)]
pub enum WalkState {
    /// Walker is mid-walk. `kernel_step` will extract the next
    /// component, call FsOps, and transition to another state.
    Walking(WalkingState),
    /// Walker emitted an IO request and must yield.
    Defer {
        resume: ResumeToken,
        cause: WalkCause,
    },
    /// Walker reached a terminal outcome.
    Terminal(PathResolution),
}

/// Full mutable walker frame — all state threaded through the
/// component loop in the synchronous `walk_inner_v3`.
#[derive(Clone, Debug)]
pub struct WalkingState {
    /// Current DEntry (parent directory for next lookup, or terminal).
    pub current: Cap<DEntry>,
    /// Remaining path bytes.
    pub remaining: Vec<u8>,
    /// Symlink hop counter.
    pub hop_count: u32,
    /// Namespace root dentry (for absolute symlinks and `..` bound).
    pub mount_root: Cap<DEntry>,
    /// True when the original path ended with `/`.
    pub must_be_directory: bool,
}

// ============================================================================
// ResumeToken
// ============================================================================

/// Opaque re-entry token encoding the full walker state for yield/resume.
///
/// v1: encodes `WalkingState` inline plus the surrounding context
/// (`fs_ops`, `mount_payload`, `mode`, `policy`).
#[derive(Clone, Debug)]
pub struct ResumeToken {
    /// Serialised walking state.
    pub walking: WalkingState,
    /// How many symlink hops accounted for.
    pub hop_count: u32,
}

// ============================================================================
// PathResolution
// ============================================================================

#[derive(Clone, Debug)]
pub struct PathResolution {
    pub dentry: Cap<DEntry>,
    pub rnode: Cap<RNode>,
    pub fs_object_id: FsObjectId,
    pub meta: InodeMeta,
}

// ============================================================================
// KernelStep
// ============================================================================

#[derive(Clone, Debug)]
pub enum KernelStep {
    Continue(WalkState),
    NeedIO(IORequest, ResumeToken),
    Error(WalkCause),
}

// ============================================================================
// IORequest
// ============================================================================

#[derive(Clone, Debug)]
pub enum IORequest {
    DirLookup { fs_object_id: FsObjectId, name: Box<[u8]> },
    LoadInodeMeta { fs_object_id: FsObjectId },
    ReadLink { fs_object_id: FsObjectId },
    /// Materialise an RNode for a freshly-looked-up inode.
    MaterialiseRnode { fs_object_id: FsObjectId, meta: InodeMeta },
}

// ============================================================================
// FinalSymlinkPolicy
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalSymlinkPolicy {
    Follow,
    NoFollow,
}

// ============================================================================
// WalkTrail
// ============================================================================

pub struct WalkTrail<'g> {
    entries: Vec<TrailEntry<'g>>,
}

impl<'g> WalkTrail<'g> {
    pub fn new() -> Self { Self { entries: Vec::new() } }
    pub fn push_dentry(&mut self, dentry: &'g DEntry) {
        self.entries.push(TrailEntry::DEntry(dentry));
    }
    pub fn push_mount_boundary(&mut self, was_at: &'g DEntry) {
        self.entries.push(TrailEntry::MountBoundary { was_at });
    }
    pub fn pop(&mut self) -> Option<TrailEntry<'g>> { self.entries.pop() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
    pub fn len(&self) -> usize { self.entries.len() }
}

impl<'g> Default for WalkTrail<'g> {
    fn default() -> Self { Self::new() }
}

#[derive(Clone, Copy, Debug)]
pub enum TrailEntry<'g> {
    DEntry(&'g DEntry),
    MountBoundary { was_at: &'g DEntry },
}
