//! Walker state vocabulary per `txdoc:VFS-CHECKS-WALKSTATE-1` (§5–§6).
//!
//! The resolution state machine uses these types to drive
//! component-by-component path walks.  `Walking` carries the
//! full mutable walker frame; `kernel_step` advances one
//! component per call; the driver loops until terminal or error.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::mount::{MountIdentity, MountNamespace};
use crate::vfs::adapter::step_engine::Cap;
use crate::vfs::{DEntry, FsObjectId, InodeMeta, RNode};

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
        request: IORequest,
        resume: ResumeToken,
        cause: WalkCause,
    },
    /// Walker hit a terminal error before producing a path resolution.
    Error(WalkCause),
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
    /// Mount identity containing `current` for namespace-aware walks.
    pub current_mount: Option<Cap<MountIdentity>>,
    /// Namespace root mount restored by absolute symlink substitution.
    pub mount_root_mount: Option<Cap<MountIdentity>>,
    /// True when the original path ended with `/`.
    pub must_be_directory: bool,
}

// ============================================================================
// ResumeToken
// ============================================================================

/// Opaque re-entry token encoding the full walker state for yield/resume.
///
/// v1: encodes `WalkingState` inline plus mount-namespace context. The
/// caller still supplies mode/policy/credential when resuming.
#[derive(Clone, Debug)]
pub struct ResumeToken {
    /// Serialised walking state.
    pub walking: WalkingState,
    /// I/O request that suspended this walker.
    pub request: IORequest,
    /// Mount namespace that constrained mountpoint crossing before yield.
    pub mount_namespace: Option<Cap<MountNamespace>>,
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
    pub mount: Option<Cap<MountIdentity>>,
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
    DirLookup {
        fs_object_id: FsObjectId,
        name: Vec<u8>,
    },
    LoadInodeMeta {
        fs_object_id: FsObjectId,
    },
    ReadLink {
        fs_object_id: FsObjectId,
        meta: InodeMeta,
    },
    /// Materialise an RNode for a freshly-looked-up inode.
    MaterialiseRnode {
        fs_object_id: FsObjectId,
        meta: InodeMeta,
    },
}

#[derive(Clone, Debug)]
pub enum IOResult {
    DirLookup(Result<FsObjectId, crate::execution::Errno>),
    LoadInodeMeta(Result<InodeMeta, crate::execution::Errno>),
    ReadLink(Result<Box<[u8]>, crate::execution::Errno>),
    MaterialiseRnode(Result<Cap<RNode>, crate::execution::Errno>),
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
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
    pub fn push_dentry(&mut self, dentry: &'g DEntry) {
        self.entries.push(TrailEntry::DEntry(dentry));
    }
    pub fn push_mount_boundary(&mut self, was_at: &'g DEntry) {
        self.entries.push(TrailEntry::MountBoundary { was_at });
    }
    pub fn pop(&mut self) -> Option<TrailEntry<'g>> {
        self.entries.pop()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl<'g> Default for WalkTrail<'g> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TrailEntry<'g> {
    DEntry(&'g DEntry),
    MountBoundary { was_at: &'g DEntry },
}

pub(super) fn try_copy_path(bytes: &[u8]) -> Result<Vec<u8>, crate::execution::Errno> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(bytes.len())
        .map_err(|_| crate::execution::Errno::ENOMEM)?;
    copied.extend_from_slice(bytes);
    Ok(copied)
}

pub(super) fn try_join_path(
    prefix: &[u8],
    remaining: &[u8],
) -> Result<Vec<u8>, crate::execution::Errno> {
    let len = checked_join_path_len(prefix.len(), remaining.len())?;
    let mut joined = Vec::new();
    joined
        .try_reserve_exact(len)
        .map_err(|_| crate::execution::Errno::ENOMEM)?;
    joined.extend_from_slice(prefix);
    if !remaining.is_empty() {
        joined.push(b'/');
        joined.extend_from_slice(remaining);
    }
    Ok(joined)
}

fn checked_join_path_len(
    prefix_len: usize,
    remaining_len: usize,
) -> Result<usize, crate::execution::Errno> {
    let separator = usize::from(remaining_len != 0);
    prefix_len
        .checked_add(separator)
        .and_then(|len| len.checked_add(remaining_len))
        .ok_or(crate::execution::Errno::ENOMEM)
}

#[cfg(test)]
mod allocation_tests {
    use super::*;

    #[test]
    fn path_join_capacity_overflow_maps_to_enomem() {
        assert_eq!(
            checked_join_path_len(usize::MAX, 1),
            Err(crate::execution::Errno::ENOMEM)
        );
    }
}

pub(super) fn try_clone_io_request(
    request: &IORequest,
) -> Result<IORequest, crate::execution::Errno> {
    Ok(match request {
        IORequest::DirLookup { fs_object_id, name } => IORequest::DirLookup {
            fs_object_id: *fs_object_id,
            name: try_copy_path(name)?,
        },
        IORequest::LoadInodeMeta { fs_object_id } => IORequest::LoadInodeMeta {
            fs_object_id: *fs_object_id,
        },
        IORequest::ReadLink { fs_object_id, meta } => IORequest::ReadLink {
            fs_object_id: *fs_object_id,
            meta: *meta,
        },
        IORequest::MaterialiseRnode { fs_object_id, meta } => IORequest::MaterialiseRnode {
            fs_object_id: *fs_object_id,
            meta: *meta,
        },
    })
}
