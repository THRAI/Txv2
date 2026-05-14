//! Walker state vocabulary per `txdoc:VFS-CHECKS-WALKSTATE-1` (§5–§6).
//!
//! The resolution state machine is sealed — only the driver loop
//! (`run_walker`, `resume_walker`) constructs or transitions state.
//! The current synchronous implementation (`vfs::step_walk`)
//! predates the state-machine vocabulary; this module provides the
//! forward-looking types that future async/yielding walker
//! implementations will thread through `kernel_step`.
//!
//! All types in this module are witness-free: they operate on
//! `IdentRef<'g, DEntry>` and `FsObjectId`, leaving `Cap` promotion
//! to the terminal witness constructors in `resolution::terminal`.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::vfs::adapter::step_engine::Cap;
use crate::vfs::{DEntry, FsObjectId, InodeMeta, RNode};

// ============================================================================
// WalkMode — closed set of path-resolution intents
// ============================================================================

/// Per `txdoc:VFS-CHECKS-WALKER-MODES-1` (§4).
///
/// Six modes.  The set is closed.  Each mode determines:
/// - how many components the walker consumes,
/// - what terminal entity it expects, and
/// - which witness type the terminal builder produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkMode {
    /// Followed walk to terminal entity.
    Entity,
    /// Walk stops at final symlink; does not follow.
    EntityUnfollowed,
    /// Walk to penultimate component; final may be absent.
    ParentAndName,
    /// Walk to parent; final must be present.
    ParentAndNamedChild,
    /// Walk to terminal; returns Present or Absent.
    EntityOrParentAndName,
    /// Walk to terminal; terminal must be a mount root.
    MountPoint,
}

// ============================================================================
// WalkCause — why the walker could not reach its terminal
// ============================================================================

/// Per `txdoc:VFS-CHECKS-WALKCAUSE-1` (§5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkCause {
    /// A directory component did not have search (`X`) permission
    /// and the caller lacks `CAP_DAC_OVERRIDE`.
    TraverseDenied,
    /// An intermediate or terminal name was not found in the
    /// parent directory.
    ComponentNotFound,
    /// A non-terminal path component resolved to something that is
    /// not a directory.
    NotADirectory,
    /// The symlink count exceeded `SYMLOOP_MAX`.
    SymlinkLimit,
    /// The walker crossed a mount point that does not have a root
    /// dentry (mount-gap — v1 should never occur).
    MountPointGap,
    /// `FsOps::lookup` or `FsOps::load_inode_meta` returned an error.
    FsOpsRejected(crate::execution::Errno),
    /// Non-terminal permission check failed with a specific denial
    /// (e.g. search denied on a directory component).
    Permission(NonTerminalDenial),
    /// `step_open` failed on the terminal entity.
    TerminalOpenFailed(crate::execution::Errno),
}

/// Detail for a `WalkCause::Permission`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonTerminalDenial {
    SearchDenied,
}

// ============================================================================
// WalkState — pure machine state (mode-independent)
// ============================================================================

/// Per `txdoc:VFS-CHECKS-WALKSTATE-1` (§5).
///
/// `WalkMode` is an interpreter parameter passed alongside, **not
/// stored** in state.
#[derive(Clone, Debug)]
pub enum WalkState {
    /// Walker has made forward progress and can continue.
    Advance,
    /// Walker emitted an IO request and must yield; `ResumeToken`
    /// encodes the continuation point.
    Defer {
        resume: ResumeToken,
        cause: WalkCause,
    },
    /// Walker reached a terminal outcome.
    Terminal(PathResolution),
}

// ============================================================================
// ResumeToken — opaque re-entry handle
// ============================================================================

/// Opaque token encoding the walker's re-entry state after a yield.
///
/// Per `txdoc:VFS-CHECKS-RESUME-TOKEN-1`.  v1 synchronous
/// walker never constructs `Defer`; this type exists for future
/// async walker implementations.
#[derive(Clone, Debug)]
pub struct ResumeToken(u64);

// ============================================================================
// PathResolution — terminal walker outcome
// ============================================================================

/// Terminal outcome of a path walk.
///
/// Holds the resolved `Cap<DEntry>` and `Cap<RNode>` so that
/// witness constructors in `terminal.rs` can produce `IdentRef`
/// handles via `.ident_ref(guard)`.
#[derive(Clone, Debug)]
pub struct PathResolution {
    /// Terminal entity's DEntry capability.
    pub dentry: Cap<DEntry>,
    /// Terminal entity's RNode capability.
    pub rnode: Cap<RNode>,
    /// Resolved FsObjectId.
    pub fs_object_id: FsObjectId,
    /// Inode metadata from `load_inode_meta`.
    pub meta: InodeMeta,
}

// ============================================================================
// KernelStep — kernel_step output
// ============================================================================

/// Per `txdoc:VFS-CHECKS-THE-SHARED-KERNEL-KERNEL-STEP-1` (§6).
///
/// Mode-agnostic δ.  Reads `WalkState` and structure; produces
/// `KernelStep`.  No mutation.
#[derive(Clone, Debug)]
pub enum KernelStep {
    /// Transition the state machine to the given next state.
    Continue(WalkState),
    /// An IO operation must complete before the walker can proceed.
    NeedIO(IORequest, ResumeToken),
    /// The walker cannot proceed for the given reason.
    Error(WalkCause),
}

// ============================================================================
// IORequest — deferred IO operation
// ============================================================================

/// Per `txdoc:VFS-CHECKS-IOREQUEST-1`.
///
/// v1 synchronous walker never constructs `NeedIO`; this type exists
/// for future async walker implementations.
#[derive(Clone, Debug)]
pub enum IORequest {
    /// Lookup a name in a directory that is not resident in the
    /// dentry cache.
    DirLookup { fs_object_id: FsObjectId, name: Box<[u8]> },
    /// Load inode metadata for a newly-resolved FsObjectId.
    LoadInodeMeta { fs_object_id: FsObjectId },
    /// Read symlink target.
    ReadLink { fs_object_id: FsObjectId },
}

// ============================================================================
// FinalSymlinkPolicy — how to resolve the terminal if it's a symlink
// ============================================================================

/// Per `txdoc:VFS-CHECKS-SYMLINK-POLICY-1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinalSymlinkPolicy {
    /// Follow a terminal symlink (default for most syscalls).
    Follow,
    /// Do not follow the very last component if it's a symlink
    /// (lstat, readlink, O_NOFOLLOW).
    NoFollow,
}

// ============================================================================
// WalkTrail — ancestor stack
// ============================================================================

/// Per `txdoc:VFS-CHECKS-WALKTRAIL-1` (§5.4).
///
/// Stack of ancestor DEntries and mount-boundary markers for `..`
/// traversal.  Trail is pushed on each forward step; popped on
/// `..`.  Mount-boundary markers are pushed when the walker crosses
/// into a mount; `..` across a mount pops the boundary and restores
/// the cursor to the mountpoint DEntry.
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

/// One entry in the walker's ancestor trail.
#[derive(Clone, Copy, Debug)]
pub enum TrailEntry<'g> {
    DEntry(&'g DEntry),
    MountBoundary { was_at: &'g DEntry },
}
