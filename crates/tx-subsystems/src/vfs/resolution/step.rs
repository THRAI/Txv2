//! Shared transition kernel per `txdoc:VFS-CHECKS-THE-SHARED-KERNEL-KERNEL-STEP-1` (§6).
//!
//! `kernel_step` is the mode-agnostic δ function: it reads the
//! current `WalkState` and the caller-supplied `WalkMode`, and
//! produces a `KernelStep` describing the next action — continue,
//! wait for IO, or fail.
//!
//! ## v1
//!
//! The current implementation is a thin dispatcher.  The real
//! component-by-component walk logic lives in the synchronous
//! `walker::walk_inner_v3` loop.  When ext4 / disk-backed backends
//! grow real IO yields, the walker's component loop will be lifted
//! into `kernel_step` as a proper state machine that can yield at
//! each `FsOps::lookup` / `load_inode_meta` call site and resume
//! through `ResumeToken`.

use crate::execution::Guard;

use super::state::{FinalSymlinkPolicy, KernelStep, WalkCause, WalkMode, WalkState};
use super::terminal;

/// Mode-agnostic transition kernel.
///
/// Reads `WalkState` and `mode`; produces `KernelStep`.  No mutation.
///
/// Per `txdoc:VFS-CHECKS-THE-SHARED-KERNEL-KERNEL-STEP-1`.
pub fn kernel_step(
    state: WalkState,
    mode: WalkMode,
    _policy: FinalSymlinkPolicy,
    _guard: &Guard<'_>,
) -> KernelStep {
    match state {
        WalkState::Advance => {
            // v1: the synchronous walker handles component-by-component
            // advance in walk_inner_v3.  Advance → Continue(Advance)
            // lets the driver loop iterate.
            KernelStep::Continue(WalkState::Advance)
        }
        WalkState::Defer { resume, cause } => {
            // v1: no IO-resume path.  Treat Defer as a terminal error.
            let _ = resume;
            KernelStep::Error(cause)
        }
        WalkState::Terminal(resolved) => {
            if terminal::accepts(&WalkState::Terminal(resolved.clone()), mode) {
                KernelStep::Continue(WalkState::Terminal(resolved))
            } else {
                KernelStep::Error(WalkCause::ComponentNotFound)
            }
        }
    }
}
