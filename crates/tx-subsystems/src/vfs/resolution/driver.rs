//! Walker driver loop per `txdoc:VFS-CHECKS-DRIVER-LOOP-1` (§9).
//!
//! The driver is the sealed execution loop that threads `kernel_step`
//! calls until the walker reaches a terminal outcome or error.
//!
//! Three entry points:
//!
//! - `walk_to_completion` — drive the walker synchronously from start
//!   to terminal (v1: delegates to the existing synchronous
//!   `walker::walk_inner_v3`).
//! - `run_walker` — start a walk and return after the first terminal
//!   or yield.  Used for async callers that can suspend on `Yield`.
//! - `resume_walker` — resume a walker from a `ResumeToken` after IO
//!   completion.  v1 always returns `Err(EIO)` (resume not implemented).
//!
//! ## v1
//!
//! The current implementation bridges the synchronous `walk_inner_v3`
//! loop with the state-machine vocabulary.  `walk_to_completion`
//! delegates directly to the synchronous walker; `run_walker` and
//! `resume_walker` are stubs that will grow real state-machine bodies
//! when ext4 backends yield.

use alloc::sync::Arc;

use crate::execution::{Errno, Guard};
use crate::vfs::adapter::step_engine::{self, Cap, NoProgress, StepOutcome};
use crate::vfs::structure::{Credential, DEntry, InlineName, OpenFileFlags};
use crate::vfs::FsOps;
use crate::vfs::walker;

use super::state::{FinalSymlinkPolicy, KernelStep, PathResolution, ResumeToken, WalkMode, WalkState};

// ---------------------------------------------------------------------------
// Driver: walk_to_completion (synchronous bridge)
// ---------------------------------------------------------------------------

/// Drive a walk from start to terminal, synchronously.
///
/// Delegates to the synchronous `walker::step_walk` and wraps the
/// result in the state-machine vocabulary.  The caller receives
/// either `PathResolution` (for witness construction) or an `Errno`.
///
/// Per `txdoc:VFS-CHECKS-DRIVER-WALK-TO-COMPLETION-1`.
pub fn walk_to_completion<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> Result<PathResolution, Errno> {
    // v1 bridge: delegate to the synchronous walker.  The mode
    // parameter is observed only by the facade (require.rs); the
    // synchronous walker always does a full Entity walk.
    let dentry = match walker::step_walk(rooted_at, path, cred, guard) {
        StepOutcome::Done(d) => d,
        StepOutcome::Err(e) => return Err(Errno::from(e)),
        StepOutcome::Continue { .. } => return Err(Errno::EAGAIN),
        StepOutcome::Yield { .. } => return Err(Errno::EAGAIN),
    };
    let rnode = dentry.rnode().clone();
    let meta = rnode.meta();
    let fs_object_id = rnode.fs_object_id();
    Ok(PathResolution {
        dentry,
        rnode,
        fs_object_id,
        meta,
    })
}

// ---------------------------------------------------------------------------
// Driver: run_walker (async entry point — v1 stub)
// ---------------------------------------------------------------------------

/// Start a walk and return after the first terminal or yield.
///
/// v1: delegates to `walk_to_completion` and wraps the result as
/// `WalkState::Terminal`.
///
/// Per `txdoc:VFS-CHECKS-DRIVER-RUN-WALKER-1`.
pub fn run_walker<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> WalkState {
    match walk_to_completion(rooted_at, path, mode, cred, guard) {
        Ok(resolved) => WalkState::Terminal(resolved),
        Err(_err) => WalkState::Advance,
    }
}

// ---------------------------------------------------------------------------
// Driver: resume_walker (v1 stub)
// ---------------------------------------------------------------------------

/// Resume a walker from a `ResumeToken` after IO completion.
///
/// v1 always returns `Err(EIO)`.  When the walker is refactored to a
/// true state machine, this function will decode the `ResumeToken`,
/// restore the walker's internal state (current dentry, remaining
/// components, hop count, etc.), and re-enter `kernel_step`.
///
/// Per `txdoc:VFS-CHECKS-DRIVER-RESUME-WALKER-1`.
pub fn resume_walker<'g>(
    _token: ResumeToken,
    _mode: WalkMode,
    _policy: FinalSymlinkPolicy,
    _guard: &'g Guard<'_>,
) -> Result<WalkState, Errno> {
    // v1: resume not supported.
    Err(Errno::EIO)
}
