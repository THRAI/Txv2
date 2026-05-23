//! Walker driver loop per `txdoc:VFS-CHECKS-DRIVER-LOOP-1` (§9).
//!
//! Three entry points:
//! - `walk_to_completion` — synchronous full walk (v1: component loop)
//! - `run_walker` — start a walk, return after terminal or first yield
//! - `resume_walker` — resume from a `ResumeToken` after IO

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::execution::{Errno, Guard};
use crate::mount::MountNamespace;
use crate::vfs::adapter::step_engine::Cap;
use crate::vfs::structure::{Credential, DEntry};
use crate::vfs::walker;
use crate::vfs::FsOps;

use super::error::classify;
use super::state::{
    FinalSymlinkPolicy, KernelStep, PathResolution, ResumeToken, WalkCause, WalkMode, WalkState,
    WalkingState,
};
use super::step::kernel_step;

/// Drive a walk from start to terminal, synchronously.
///
/// Constructs the initial `WalkingState`, then loops `kernel_step`
/// until terminal or error.  Yields from `FsOps` calls are propagated
/// as `Err(EAGAIN)` — callers that can suspend should use
/// `run_walker` / `resume_walker`.
pub fn walk_to_completion(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    guard: &Guard<'_>,
) -> Result<PathResolution, Errno> {
    walk_to_completion_with_mount_namespace(rooted_at, path, mode, policy, cred, None, guard)
}

/// Drive a walk from start to terminal using the supplied mount namespace.
///
/// When `mount_namespace` is present, mountpoint crossing consults that
/// namespace's table only. A `None` namespace preserves the historical global
/// mount-table fallback used by boot scaffolds and older tests.
pub fn walk_to_completion_with_mount_namespace(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    guard: &Guard<'_>,
) -> Result<PathResolution, Errno> {
    let mount_root = walker::mount_root_dentry(&rooted_at);

    let (current, remaining): (Cap<DEntry>, Vec<u8>) = if path.first() == Some(&b'/') {
        (mount_root.clone(), path[1..].to_vec())
    } else {
        (rooted_at.clone(), path.to_vec())
    };

    let must_be_directory = remaining.last().copied() == Some(b'/');

    let _fs_ops: Arc<dyn FsOps> = walker::fs_ops_for(&current, guard)
        .or_else(|| walker::fs_ops_for(&mount_root, guard))
        .ok_or_else(|| {
            use super::diagnostic;
            let rn = current.rnode();
            diagnostic::record_ctx(
                8, // walk_to_completion: fs_ops_for None (pre-loop)
                current.name().as_bytes(),
                rn.fs_object_id(),
                &remaining,
                rn.containing_mount_weak().is_some(),
            );
            Errno::ENODEV
        })?;

    let _mount_payload = walker::mount_payload_for(&current, guard)
        .or_else(|| walker::mount_payload_for(&mount_root, guard));
    // record if mount_payload is None here (non-fatal, but diagnostic)
    if walker::mount_payload_for(&current, guard).is_none()
        && walker::mount_payload_for(&mount_root, guard).is_none()
    {
        use super::diagnostic;
        diagnostic::record_diag(9); // walk_to_completion: mount_payload_for None
        diagnostic::record_label(b"walk_to_completion: no mount_payload");
    }

    let mut state = WalkState::Walking(WalkingState {
        current,
        remaining,
        hop_count: 0,
        mount_root,
        must_be_directory,
    });

    // v1: synchronous loop — treat Yield as error (caller can't suspend).
    loop {
        let walking = match state {
            WalkState::Walking(w) => w,
            WalkState::Terminal(resolved) => return Ok(resolved),
            WalkState::Defer { cause, .. } => return Err(classify(&cause)),
        };

        let fs_ops = walker::fs_ops_for(&walking.current, guard)
            .or_else(|| walker::fs_ops_for(&walking.mount_root, guard))
            .ok_or_else(|| {
                use super::diagnostic;
                let rn = walking.current.rnode();
                diagnostic::record_ctx(
                    8, // walk_to_completion loop: fs_ops_for None
                    walking.current.name().as_bytes(),
                    rn.fs_object_id(),
                    &walking.remaining,
                    rn.containing_mount_weak().is_some(),
                );
                Errno::ENODEV
            })?;

        let mount_payload = walker::mount_payload_for(&walking.current, guard)
            .or_else(|| walker::mount_payload_for(&walking.mount_root, guard));

        let walking_state = walking.clone();
        match kernel_step(
            walking,
            fs_ops,
            mount_payload,
            mount_namespace,
            cred,
            mode,
            policy,
            guard,
        ) {
            KernelStep::Continue(next) => state = next,
            KernelStep::Error(cause) => {
                // Capture walker failure context before returning.
                let rn = walking_state.current.rnode();
                let errno = classify(&cause);
                let stage = match &cause {
                    WalkCause::TraverseDenied => 30,
                    WalkCause::ComponentNotFound => 31,
                    WalkCause::NotADirectory => 32,
                    WalkCause::SymlinkLimit => 33,
                    WalkCause::MountPointGap => 34,
                    WalkCause::FsOpsRejected(_) => 35,
                    WalkCause::Permission(_) => 36,
                    WalkCause::TerminalOpenFailed(_) => 37,
                };
                super::diagnostic::record_ctx(
                    stage,
                    walking_state.current.name().as_bytes(),
                    rn.fs_object_id(),
                    &walking_state.remaining,
                    rn.containing_mount_weak().is_some(),
                );
                // Also stamp the legacy diag for old sentinel compatibility.
                super::diagnostic::record_diag(stage);
                return Err(errno);
            }
            KernelStep::NeedIO(_req, _token) => return Err(Errno::EAGAIN),
        }
    }
}

/// Start a walk, return after terminal or first yield.
///
/// Constructs the initial `WalkingState`, runs one step, and returns
/// the resulting `WalkState`.
pub fn run_walker(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    guard: &Guard<'_>,
) -> WalkState {
    let rooted = rooted_at.clone();
    let root2 = rooted.clone();
    match walk_to_completion(rooted_at, path, mode, policy, cred, guard) {
        Ok(resolved) => WalkState::Terminal(resolved),
        Err(_err) => WalkState::Walking(WalkingState {
            current: rooted,
            remaining: Vec::new(),
            hop_count: 0,
            mount_root: root2,
            must_be_directory: false,
        }),
    }
}

/// Resume a walker from a `ResumeToken` after IO completion.
///
/// v1: restores the `WalkingState` from the token and re-enters
/// the synchronous loop.
pub fn resume_walker(
    token: ResumeToken,
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    guard: &Guard<'_>,
) -> Result<PathResolution, Errno> {
    let walking = token.walking;

    let _fs_ops = walker::fs_ops_for(&walking.current, guard)
        .or_else(|| walker::fs_ops_for(&walking.mount_root, guard))
        .ok_or(Errno::ENODEV)?;

    let _mount_payload = walker::mount_payload_for(&walking.current, guard)
        .or_else(|| walker::mount_payload_for(&walking.mount_root, guard));

    let mut state = WalkState::Walking(walking);

    loop {
        let w = match state {
            WalkState::Walking(w) => w,
            WalkState::Terminal(resolved) => return Ok(resolved),
            WalkState::Defer { cause, .. } => return Err(classify(&cause)),
        };

        let fs_ops = walker::fs_ops_for(&w.current, guard)
            .or_else(|| walker::fs_ops_for(&w.mount_root, guard))
            .ok_or(Errno::ENODEV)?;

        let mp = walker::mount_payload_for(&w.current, guard)
            .or_else(|| walker::mount_payload_for(&w.mount_root, guard));

        match kernel_step(w, fs_ops, mp, None, cred, mode, policy, guard) {
            KernelStep::Continue(next) => state = next,
            KernelStep::Error(cause) => return Err(classify(&cause)),
            KernelStep::NeedIO(_req, _token) => return Err(Errno::EAGAIN),
        }
    }
}
