//! Walker driver loop per `txdoc:VFS-CHECKS-DRIVER-LOOP-1` (§9).
//!
//! Three entry points:
//! - `walk_to_completion` — synchronous full walk (v1: component loop)
//! - `run_walker` — start a walk, return after terminal or first yield
//! - `resume_walker` — resume from a `ResumeToken` after IO

use crate::execution::{Errno, Guard};
use crate::mount::MountNamespace;
use crate::vfs::adapter::step_engine::Cap;
use crate::vfs::structure::{Credential, DEntry};
use crate::vfs::walker;

use super::error::classify;
use super::state::{
    try_copy_path, FinalSymlinkPolicy, IORequest, IOResult, KernelStep, PathResolution,
    RemainingPath, ResumeToken, WalkCause, WalkMode, WalkState, WalkingState,
};
use super::step::{
    kernel_step, kernel_step_after_lookup_io, kernel_step_after_materialise_io,
    kernel_step_after_meta_io, kernel_step_after_readlink_io, TerminalRules,
};

/// Resolve the filesystem scope already carried by the walk before falling
/// back to the RNode weak binding. A namespace-aware walk holds its current
/// `MountIdentity` strongly, so upgrading that payload once supplies both the
/// filesystem operations and mount payload for the whole component step.
fn mount_payload_for_walking(
    walking: &WalkingState,
    guard: &Guard<'_>,
) -> Option<Cap<crate::mount::MountPayload>> {
    walking
        .current_mount
        .as_ref()
        .and_then(|mount| mount.payload_cap().ok())
        .map(|payload| payload.into_cap())
        .or_else(|| walker::mount_payload_for(&walking.current, guard))
        .or_else(|| walker::mount_payload_for(&walking.mount_root, guard))
}

fn fs_scope_for_walking(
    walking: &WalkingState,
    guard: &Guard<'_>,
) -> Option<(
    alloc::sync::Arc<dyn crate::vfs::FsOps>,
    Option<Cap<crate::mount::MountPayload>>,
)> {
    let mount_payload = mount_payload_for_walking(walking, guard);
    let fs_ops = mount_payload
        .as_ref()
        .map(|payload| payload.fs_ops.clone())
        .or_else(|| walker::fs_ops_for(&walking.current, guard))
        .or_else(|| walker::fs_ops_for(&walking.mount_root, guard))?;
    Some((fs_ops, mount_payload))
}

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
    walk_to_completion_with_mount_namespace_and_origin(
        rooted_at,
        path,
        mode,
        policy,
        cred,
        mount_namespace,
        None,
        guard,
    )
}

pub fn walk_to_completion_with_mount_namespace_and_origin(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    origin_mount: Option<&Cap<crate::mount::MountIdentity>>,
    guard: &Guard<'_>,
) -> Result<PathResolution, Errno> {
    walk_to_completion_with_mount_namespace_and_origin_policy(
        rooted_at,
        path,
        mode,
        policy,
        cred,
        mount_namespace,
        origin_mount,
        false,
        guard,
    )
}

/// Resolve a path using only authoritative positive dentry-cache entries.
/// `EAGAIN` means at least one component needs backend lookup or
/// materialisation and is the caller's signal to use the wait-capable path.
pub fn walk_cached_to_completion_with_mount_namespace_and_origin(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    origin_mount: Option<&Cap<crate::mount::MountIdentity>>,
    guard: &Guard<'_>,
) -> Result<PathResolution, Errno> {
    walk_to_completion_with_mount_namespace_and_origin_policy(
        rooted_at,
        path,
        mode,
        policy,
        cred,
        mount_namespace,
        origin_mount,
        true,
        guard,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_to_completion_with_mount_namespace_and_origin_policy(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    origin_mount: Option<&Cap<crate::mount::MountIdentity>>,
    cache_only: bool,
    guard: &Guard<'_>,
) -> Result<PathResolution, Errno> {
    let (current, remaining, mount_root, current_mount, mount_root_mount, must_be_directory) =
        initial_walk_frame(rooted_at, path, mount_namespace, origin_mount)?;

    let mut state = WalkState::Walking(WalkingState {
        current,
        remaining,
        hop_count: 0,
        mount_root,
        current_mount,
        mount_root_mount,
        must_be_directory,
    });

    // v1: synchronous loop — treat Yield as error (caller can't suspend).
    loop {
        let walking = match state {
            WalkState::Walking(w) => w,
            WalkState::Terminal(resolved) => return Ok(resolved),
            WalkState::Defer { cause, .. } => return Err(classify(&cause)),
            WalkState::Error(cause) => return Err(classify(&cause)),
        };

        let (fs_ops, mount_payload) = fs_scope_for_walking(&walking, guard).ok_or_else(|| {
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

        // Rich failure snapshots are useful while debugging the walker, but
        // cloning the dentry and copying the remaining path on every successful
        // component makes ordinary release path lookup pay for diagnostics it
        // never consumes.
        #[cfg(debug_assertions)]
        let diagnostic_current = walking.current.clone();
        #[cfg(debug_assertions)]
        let (diagnostic_remaining, diagnostic_len) = {
            let mut remaining = [0u8; 128];
            let len = walking.remaining.len().min(remaining.len());
            remaining[..len].copy_from_slice(&walking.remaining[..len]);
            (remaining, len)
        };
        let rules = TerminalRules::new(mode, policy);
        let step = if cache_only {
            super::step::kernel_step_cached(
                walking,
                fs_ops,
                mount_payload,
                mount_namespace,
                cred,
                rules,
                guard,
            )
        } else {
            kernel_step(
                walking,
                fs_ops,
                mount_payload,
                mount_namespace,
                cred,
                rules,
                guard,
            )
        };
        match step {
            KernelStep::Continue(next) => state = next,
            KernelStep::Error(cause) => {
                let errno = classify(&cause);
                #[cfg(debug_assertions)]
                {
                    // Capture walker failure context before returning.
                    let rn = diagnostic_current.rnode();
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
                        diagnostic_current.name().as_bytes(),
                        rn.fs_object_id(),
                        &diagnostic_remaining[..diagnostic_len],
                        rn.containing_mount_weak().is_some(),
                    );
                    // Also stamp the legacy diag for old sentinel compatibility.
                    super::diagnostic::record_diag(stage);
                }
                return Err(errno);
            }
            KernelStep::NeedIO(_req, _token) => return Err(Errno::EAGAIN),
        }
    }
}

/// Start a walk, return after terminal or first yield.
///
/// Constructs the initial `WalkingState` and drives `kernel_step`
/// until terminal, error, or first yield.
pub fn run_walker(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    guard: &Guard<'_>,
) -> WalkState {
    run_walker_with_mount_namespace(rooted_at, path, mode, policy, cred, None, guard)
}

/// Start a walk using the supplied mount namespace, returning after terminal,
/// error, or first yield.
pub fn run_walker_with_mount_namespace(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    guard: &Guard<'_>,
) -> WalkState {
    run_walker_with_mount_namespace_and_origin(
        rooted_at,
        path,
        mode,
        policy,
        cred,
        mount_namespace,
        None,
        guard,
    )
}

pub fn run_walker_with_mount_namespace_and_origin(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    origin_mount: Option<&Cap<crate::mount::MountIdentity>>,
    guard: &Guard<'_>,
) -> WalkState {
    let (current, remaining, mount_root, current_mount, mount_root_mount, must_be_directory) =
        match initial_walk_frame(rooted_at, path, mount_namespace, origin_mount) {
            Ok(frame) => frame,
            Err(errno) => return WalkState::Error(WalkCause::FsOpsRejected(errno)),
        };
    drive_walk_state(
        WalkingState {
            current,
            remaining,
            hop_count: 0,
            mount_root,
            current_mount,
            mount_root_mount,
            must_be_directory,
        },
        mode,
        policy,
        cred,
        mount_namespace,
        guard,
    )
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
    let mount_namespace = token.mount_namespace;
    match drive_walk_state(
        token.walking,
        mode,
        policy,
        cred,
        mount_namespace.as_ref(),
        guard,
    ) {
        WalkState::Terminal(resolved) => Ok(resolved),
        WalkState::Defer { cause, .. } | WalkState::Error(cause) => Err(classify(&cause)),
        WalkState::Walking(_) => Err(Errno::EAGAIN),
    }
}

/// Resume a walker after the backend completed the exact request that yielded.
///
/// This entry point consumes the completed I/O result first, then continues the
/// walker. It preserves retry-style `resume_walker` for legacy callers that do
/// not yet have a typed completion channel.
pub fn resume_walker_after_io(
    token: ResumeToken,
    io_result: IOResult,
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    guard: &Guard<'_>,
) -> WalkState {
    let mount_namespace = token.mount_namespace.clone();
    let state = match apply_io_result(
        token,
        io_result,
        mount_namespace.as_ref(),
        cred,
        mode,
        policy,
        guard,
    ) {
        Ok(state) => state,
        Err(errno) => return WalkState::Error(WalkCause::FsOpsRejected(errno)),
    };
    drive_existing_walk_state(state, mode, policy, cred, mount_namespace.as_ref(), guard)
}

fn initial_walk_frame(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    mount_namespace: Option<&Cap<MountNamespace>>,
    origin_mount: Option<&Cap<crate::mount::MountIdentity>>,
) -> Result<
    (
        Cap<DEntry>,
        RemainingPath,
        Cap<DEntry>,
        Option<Cap<crate::mount::MountIdentity>>,
        Option<Cap<crate::mount::MountIdentity>>,
        bool,
    ),
    Errno,
> {
    let (mount_root, mount_root_mount) = match mount_namespace {
        Some(namespace) => (namespace.root_dentry(), Some(namespace.root().clone())),
        None => (walker::mount_root_dentry(&rooted_at), None),
    };
    let absolute = path.first() == Some(&b'/');
    let remaining: RemainingPath = if absolute {
        try_copy_path(&path[1..])?.into()
    } else {
        try_copy_path(path)?.into()
    };
    let current = if absolute {
        mount_root.clone()
    } else {
        rooted_at
    };
    let must_be_directory = remaining.last().copied() == Some(b'/');
    let current_mount = if absolute {
        mount_root_mount.clone()
    } else {
        origin_mount.cloned().or_else(|| mount_root_mount.clone())
    };
    Ok((
        current,
        remaining,
        mount_root,
        current_mount,
        mount_root_mount,
        must_be_directory,
    ))
}

fn apply_io_result(
    token: ResumeToken,
    io_result: IOResult,
    mount_namespace: Option<&Cap<MountNamespace>>,
    cred: &Credential,
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    guard: &Guard<'_>,
) -> Result<WalkState, Errno> {
    match (token.request, io_result) {
        (
            IORequest::DirLookup { fs_object_id, name },
            IOResult::DirLookup(Ok(child_fs_object_id)),
        ) => {
            let (fs_ops, mount_payload) =
                fs_scope_for_walking(&token.walking, guard).ok_or(Errno::ENODEV)?;
            let rules = TerminalRules::new(mode, policy);
            Ok(kernel_step_to_walk_state(
                kernel_step_after_lookup_io(
                    token.walking,
                    fs_ops,
                    mount_payload,
                    mount_namespace,
                    fs_object_id,
                    &name,
                    child_fs_object_id,
                    cred,
                    rules,
                    guard,
                ),
                mount_namespace,
            ))
        }
        (IORequest::LoadInodeMeta { fs_object_id }, IOResult::LoadInodeMeta(Ok(child_meta))) => {
            let (fs_ops, mount_payload) =
                fs_scope_for_walking(&token.walking, guard).ok_or(Errno::ENODEV)?;
            let rules = TerminalRules::new(mode, policy);
            Ok(kernel_step_to_walk_state(
                kernel_step_after_meta_io(
                    token.walking,
                    fs_ops,
                    mount_payload,
                    mount_namespace,
                    fs_object_id,
                    child_meta,
                    cred,
                    rules,
                    guard,
                ),
                mount_namespace,
            ))
        }
        (IORequest::ReadLink { fs_object_id, meta }, IOResult::ReadLink(Ok(target))) => {
            let mount_payload = mount_payload_for_walking(&token.walking, guard);
            let rules = TerminalRules::new(mode, policy);
            Ok(kernel_step_to_walk_state(
                kernel_step_after_readlink_io(
                    token.walking,
                    mount_payload,
                    mount_namespace,
                    fs_object_id,
                    meta,
                    target,
                    cred,
                    rules,
                    guard,
                ),
                mount_namespace,
            ))
        }
        (IORequest::MaterialiseRnode { .. }, IOResult::MaterialiseRnode(Ok(rnode))) => {
            let mount_payload = mount_payload_for_walking(&token.walking, guard);
            let rules = TerminalRules::new(mode, policy);
            Ok(kernel_step_to_walk_state(
                kernel_step_after_materialise_io(
                    token.walking,
                    mount_payload,
                    mount_namespace,
                    rnode,
                    cred,
                    rules,
                    guard,
                ),
                mount_namespace,
            ))
        }
        (IORequest::DirLookup { .. }, IOResult::DirLookup(Err(errno)))
        | (IORequest::LoadInodeMeta { .. }, IOResult::LoadInodeMeta(Err(errno)))
        | (IORequest::ReadLink { .. }, IOResult::ReadLink(Err(errno)))
        | (IORequest::MaterialiseRnode { .. }, IOResult::MaterialiseRnode(Err(errno))) => {
            Err(errno)
        }
        _ => Err(Errno::EINVAL),
    }
}

fn kernel_step_to_walk_state(
    step: KernelStep,
    mount_namespace: Option<&Cap<MountNamespace>>,
) -> WalkState {
    match step {
        KernelStep::Continue(state) => state,
        KernelStep::NeedIO(request, mut resume) => {
            resume.mount_namespace = mount_namespace.cloned();
            WalkState::Defer {
                request,
                resume,
                cause: WalkCause::FsOpsRejected(Errno::EAGAIN),
            }
        }
        KernelStep::Error(cause) => WalkState::Error(cause),
    }
}

fn drive_existing_walk_state(
    state: WalkState,
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    guard: &Guard<'_>,
) -> WalkState {
    match state {
        WalkState::Walking(walking) => {
            drive_walk_state(walking, mode, policy, cred, mount_namespace, guard)
        }
        WalkState::Terminal(resolved) => WalkState::Terminal(resolved),
        WalkState::Defer {
            request,
            resume,
            cause,
        } => WalkState::Defer {
            request,
            resume,
            cause,
        },
        WalkState::Error(cause) => WalkState::Error(cause),
    }
}

fn drive_walk_state(
    initial: WalkingState,
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
    cred: &Credential,
    mount_namespace: Option<&Cap<MountNamespace>>,
    guard: &Guard<'_>,
) -> WalkState {
    let mut state = WalkState::Walking(initial);

    loop {
        let walking = match state {
            WalkState::Walking(w) => w,
            WalkState::Terminal(resolved) => return WalkState::Terminal(resolved),
            WalkState::Defer {
                request,
                resume,
                cause,
            } => {
                return WalkState::Defer {
                    request,
                    resume,
                    cause,
                };
            }
            WalkState::Error(cause) => return WalkState::Error(cause),
        };

        let (fs_ops, mount_payload) = match fs_scope_for_walking(&walking, guard) {
            Some(scope) => scope,
            None => return WalkState::Error(WalkCause::FsOpsRejected(Errno::ENODEV)),
        };

        let rules = TerminalRules::new(mode, policy);
        match kernel_step(
            walking,
            fs_ops,
            mount_payload,
            mount_namespace,
            cred,
            rules,
            guard,
        ) {
            KernelStep::Continue(next) => state = next,
            KernelStep::Error(cause) => return WalkState::Error(cause),
            KernelStep::NeedIO(request, mut resume) => {
                resume.mount_namespace = mount_namespace.cloned();
                return WalkState::Defer {
                    request,
                    resume,
                    cause: WalkCause::FsOpsRejected(Errno::EAGAIN),
                };
            }
        }
    }
}
