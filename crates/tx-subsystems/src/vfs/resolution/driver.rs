//! Walker driver loop per `txdoc:VFS-CHECKS-DRIVER-LOOP-1` (§9).
//!
//! Three entry points:
//! - `walk_to_completion` — synchronous full walk (v1: component loop)
//! - `run_walker` — start a walk, return after terminal or first yield
//! - `resume_walker` — resume from a `ResumeToken` after IO

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::execution::{Errno, Guard};
use crate::mount::{MountIdentity, MountPayload};
use crate::vfs::adapter::step_engine::Cap;
use crate::vfs::structure::{Credential, DEntry};
use crate::vfs::FsOps;
use crate::vfs::walker;

use super::error::classify;
use super::state::{
    FinalSymlinkPolicy, KernelStep, PathResolution, ResumeToken, WalkMode, WalkState, WalkingState,
};
use super::step::kernel_step;

/// Drive a walk from start to terminal, synchronously.
///
/// Constructs the initial `WalkingState`, then loops `kernel_step`
/// until terminal or error.  Yields from `FsOps` calls are propagated
/// as `Err(EAGAIN)` — callers that can suspend should use
/// `run_walker` / `resume_walker`.
pub fn walk_to_completion<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> Result<PathResolution, Errno> {
    let mount_root = walker::mount_root_dentry(&rooted_at);

    let (current, remaining): (Cap<DEntry>, Vec<u8>) = if path.first() == Some(&b'/') {
        (mount_root.clone(), path[1..].to_vec())
    } else {
        (rooted_at.clone(), path.to_vec())
    };

    let must_be_directory = remaining.last().copied() == Some(b'/');

    let fs_ops: Arc<dyn FsOps> = walker::fs_ops_for(&current, guard)
        .or_else(|| walker::fs_ops_for(&mount_root, guard))
        .ok_or(Errno::ENODEV)?;

    let mount_payload = walker::mount_payload_for(&current, guard)
        .or_else(|| walker::mount_payload_for(&mount_root, guard));

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
            .ok_or(Errno::ENODEV)?;

        let mount_payload = walker::mount_payload_for(&walking.current, guard)
            .or_else(|| walker::mount_payload_for(&walking.mount_root, guard));

        match kernel_step(
            walking,
            fs_ops,
            mount_payload,
            cred,
            mode,
            FinalSymlinkPolicy::Follow,
            guard,
        ) {
            KernelStep::Continue(next) => state = next,
            KernelStep::Error(cause) => return Err(classify(&cause)),
            KernelStep::NeedIO(_req, _token) => return Err(Errno::EAGAIN),
        }
    }
}

/// Start a walk, return after terminal or first yield.
///
/// Constructs the initial `WalkingState`, runs one step, and returns
/// the resulting `WalkState`.
pub fn run_walker<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> WalkState {
    let rooted = rooted_at.clone();
    let root2 = rooted.clone();
    match walk_to_completion(rooted_at, path, mode, cred, guard) {
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
pub fn resume_walker<'g>(
    token: ResumeToken,
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    guard: &'g Guard<'_>,
) -> Result<PathResolution, Errno> {
    let walking = token.walking;

    let fs_ops = walker::fs_ops_for(&walking.current, guard)
        .or_else(|| walker::fs_ops_for(&walking.mount_root, guard))
        .ok_or(Errno::ENODEV)?;

    let mount_payload = walker::mount_payload_for(&walking.current, guard)
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

        match kernel_step(w, fs_ops, mp, cred, mode, policy, guard) {
            KernelStep::Continue(next) => state = next,
            KernelStep::Error(cause) => return Err(classify(&cause)),
            KernelStep::NeedIO(_req, _token) => return Err(Errno::EAGAIN),
        }
    }
}
