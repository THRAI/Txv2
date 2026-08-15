//! Shared transition kernel per `txdoc:VFS-CHECKS-THE-SHARED-KERNEL-KERNEL-STEP-1` (§6).
//!
//! `kernel_step` advances the walker one component at a time.
//! Each call extracts the next path segment, calls `FsOps`
//! (`lookup` → `load_inode_meta` → `materialise_rnode`), handles
//! symlink chasing and mount-point crossing, then returns a
//! `KernelStep` for the driver to act on.
//!
//! The kernel is mode-agnostic: `WalkMode` only affects which
//! terminal outcome `accepts` permits; the component-by-component
//! advance is identical for all modes.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::execution::Guard;
use crate::mount::{MountIdentity, MountNamespace, MountPayload};
use crate::vfs::adapter::step_engine::{self, Cap, StepOutcome};
use crate::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_ISVTX,
};
use crate::vfs::walker::{self, SYMLOOP_MAX};
use crate::vfs::FsOps;

use super::state::{
    try_clone_io_request, try_copy_path, try_join_path, FinalSymlinkPolicy, IORequest, KernelStep,
    PathResolution, RemainingPath, ResumeToken, WalkCause, WalkMode, WalkState, WalkingState,
};
use super::terminal;

#[derive(Clone, Copy)]
pub struct TerminalRules {
    mode: WalkMode,
    policy: FinalSymlinkPolicy,
}

impl TerminalRules {
    pub fn new(mode: WalkMode, policy: FinalSymlinkPolicy) -> Self {
        Self { mode, policy }
    }
}

/// Advance the walker one component.
///
/// Reads the current `WalkingState`, the caller-supplied `mode` and
/// `policy`, and the epoch `guard`.  Produces `KernelStep::Continue`
/// (next state), `NeedIO` (must yield), or `Error` (terminal failure).
///
/// The driver wraps this in a loop until terminal.
pub fn kernel_step(
    walking: WalkingState,
    fs_ops: Arc<dyn FsOps>,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    kernel_step_with_lookup_policy(
        walking,
        fs_ops,
        mount_payload,
        mount_namespace,
        cred,
        rules,
        false,
        guard,
    )
}

/// Advance a walker without consulting filesystem metadata backends.
///
/// This is the VFS half of the trap-local path lookup lane. It accepts only
/// parent-local dentries whose backend generation still matches; a missing or
/// stale component is reported as `EAGAIN` so the syscall can fall back to the
/// ordinary wait-capable walker. No negative result from this function is
/// exposed directly to userspace.
pub fn kernel_step_cached(
    walking: WalkingState,
    fs_ops: Arc<dyn FsOps>,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    kernel_step_with_lookup_policy(
        walking,
        fs_ops,
        mount_payload,
        mount_namespace,
        cred,
        rules,
        true,
        guard,
    )
}

fn kernel_step_with_lookup_policy(
    walking: WalkingState,
    fs_ops: Arc<dyn FsOps>,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    cred: &Credential,
    rules: TerminalRules,
    cache_only: bool,
    guard: &Guard<'_>,
) -> KernelStep {
    let WalkingState {
        mut current,
        remaining,
        mut hop_count,
        mount_root,
        mut current_mount,
        mount_root_mount,
        must_be_directory,
    } = walking;

    // --- extract next component / end of input ---
    let ((component_start, component_end), remaining) = match take_next_component(remaining) {
        Some(next) => next,
        None => {
            if must_be_directory && current.rnode().meta().kind() != InodeKind::Directory {
                return KernelStep::Error(WalkCause::NotADirectory);
            }
            let rnode = current.rnode().clone();
            let meta = rnode.meta();
            let fs_object_id = rnode.fs_object_id();
            let resolved = PathResolution {
                dentry: current,
                rnode,
                fs_object_id,
                meta,
                mount: current_mount,
            };
            if terminal::accepts(&WalkState::Terminal(resolved.clone()), rules.mode) {
                return KernelStep::Continue(WalkState::Terminal(resolved));
            }
            return KernelStep::Error(WalkCause::ComponentNotFound);
        }
    };
    let component = remaining.absolute_slice(component_start, component_end);

    // --- `.` and `..` ---
    if component == b"." {
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current,
            remaining,
            hop_count,
            mount_root,
            current_mount,
            mount_root_mount,
            must_be_directory,
        }));
    }
    if component == b".." {
        if !walker::is_same_dentry(&current, &mount_root) {
            let mount_parent = match mount_namespace {
                Some(namespace) => namespace.dotdot_parent_for_mount_root(&current),
                None => crate::mount::dotdot_parent_for_mount_root(&current),
            };
            if let Some(parent) = mount_parent {
                current = parent;
                current_mount = current_mount
                    .as_ref()
                    .and_then(|mount| mount.parent().cloned());
            } else if let Some(parent) = current.parent_hint() {
                current = parent;
            }
        }
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current,
            remaining,
            hop_count,
            mount_root,
            current_mount,
            mount_root_mount,
            must_be_directory,
        }));
    }

    // --- directory check ---
    let current_kind = current.rnode().meta().kind();
    if current_kind != InodeKind::Directory {
        return KernelStep::Error(WalkCause::NotADirectory);
    }

    // --- POSIX search permission ---
    // Routes through cred::checks for a single source of truth at
    // the cred seam; the witness is discarded because the per-
    // component walk does not yet thread a SearchAuthorized<'g>
    // token to a downstream publication site.
    let parent_meta = current.rnode().meta();
    if let Err(_err) =
        crate::cred::checks::require_path_search_with_walker_cred(cred, &parent_meta, guard)
    {
        return KernelStep::Error(WalkCause::Permission(
            super::state::NonTerminalDenial::SearchDenied,
        ));
    }

    if component.is_empty() || component.len() > crate::vfs::structure::VFS_NAME_MAX {
        return KernelStep::Error(WalkCause::ComponentNotFound);
    }

    // --- lookup / materialise, with parent-local dentry cache ---
    //
    // Ext4 can have several live DEntry instances for one directory, so
    // purging one parent-local map on rename is insufficient. Its FsOps
    // supplies a mount-global directory version: a matching token makes this
    // positive dentry authoritative without another backend lookup, while a
    // mutation invalidates every instance at once. Backends without a token
    // retain the traditional contract that their mutation path purges the
    // relevant parent-local cache synchronously.
    let parent_fs_object_id = current.rnode().fs_object_id();
    let lookup_version_before = fs_ops.lookup_cache_version(parent_fs_object_id);
    let cached_child = current.cached_child_with_version_by_name(component);
    let cached_is_authoritative = cached_child.as_ref().is_some_and(|(_, version)| {
        lookup_version_before.is_none() || *version == lookup_version_before
    });
    let (child_dentry, child_rnode_cap, child_fs_object_id, child_meta) = if cached_is_authoritative
    {
        let cached = cached_child.expect("authoritative cached child exists");
        let rnode = cached.0.rnode().clone();
        let fs_object_id = rnode.fs_object_id();
        let meta = rnode.meta();
        (cached.0, rnode, fs_object_id, meta)
    } else {
        if cache_only {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EAGAIN));
        }
        let child_fs_object_id = match fs_ops.lookup(parent_fs_object_id, component, guard) {
            StepOutcome::Done(id) => id,
            StepOutcome::Yield { .. } => {
                let request = IORequest::DirLookup {
                    fs_object_id: parent_fs_object_id,
                    name: match try_copy_path(component) {
                        Ok(name) => name,
                        Err(errno) => {
                            return KernelStep::Error(WalkCause::FsOpsRejected(errno));
                        }
                    },
                };
                let retry_remaining = remaining.rewind_to(component_start);
                let resume_request = match try_clone_io_request(&request) {
                    Ok(request) => request,
                    Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
                };
                let token = ResumeToken {
                    walking: WalkingState {
                        current,
                        remaining: retry_remaining,
                        hop_count,
                        mount_root,
                        current_mount,
                        mount_root_mount,
                        must_be_directory,
                    },
                    request: resume_request,
                    mount_namespace: None,
                    hop_count,
                };
                return KernelStep::NeedIO(request, token);
            }
            StepOutcome::Err(e) => {
                return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::from(
                    e,
                )));
            }
            StepOutcome::Continue { .. } => {
                let retry_remaining = remaining.rewind_to(component_start);
                // Re-enter lookup (v3 continue without yield).
                return KernelStep::Continue(WalkState::Walking(WalkingState {
                    current,
                    remaining: retry_remaining,
                    hop_count,
                    mount_root,
                    current_mount,
                    mount_root_mount,
                    must_be_directory,
                }));
            }
        };
        let lookup_version_after = fs_ops.lookup_cache_version(parent_fs_object_id);
        let confirmed_lookup_version = if lookup_version_before == lookup_version_after {
            lookup_version_after
        } else {
            None
        };

        if let Some(cached) = cached_child
            .map(|(child, _)| child)
            .filter(|c| c.rnode().fs_object_id() == child_fs_object_id)
        {
            // FS agrees with the cached instance: keep the existing DEntry so
            // its RNode/PageContainer identity survives the walk.
            let cached = current.cache_child_with_version(cached, confirmed_lookup_version);
            let rnode = cached.rnode().clone();
            let meta = rnode.meta();
            (cached, rnode, child_fs_object_id, meta)
        } else {
            // Stale or absent cached instance: drop it and materialise fresh.
            current.remove_cached_child_by_name(component);

            let child_meta = match fs_ops.load_inode_meta(child_fs_object_id, guard) {
                StepOutcome::Done(m) => m,
                StepOutcome::Yield { .. } => {
                    let retry_remaining = remaining.rewind_to(component_start);
                    let request = IORequest::LoadInodeMeta {
                        fs_object_id: child_fs_object_id,
                    };
                    let resume_request = match try_clone_io_request(&request) {
                        Ok(request) => request,
                        Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
                    };
                    let token = ResumeToken {
                        walking: WalkingState {
                            current,
                            remaining: retry_remaining,
                            hop_count,
                            mount_root,
                            current_mount,
                            mount_root_mount,
                            must_be_directory,
                        },
                        request: resume_request,
                        mount_namespace: None,
                        hop_count,
                    };
                    return KernelStep::NeedIO(request, token);
                }
                StepOutcome::Err(e) => {
                    return KernelStep::Error(WalkCause::FsOpsRejected(
                        crate::execution::Errno::from(e),
                    ));
                }
                StepOutcome::Continue { .. } => {
                    let retry_remaining = remaining.rewind_to(component_start);
                    return KernelStep::Continue(WalkState::Walking(WalkingState {
                        current,
                        remaining: retry_remaining,
                        hop_count,
                        mount_root,
                        current_mount,
                        mount_root_mount,
                        must_be_directory,
                    }));
                }
            };

            let child_rnode_cap = match materialise_child(
                &fs_ops,
                child_fs_object_id,
                &child_meta,
                mount_payload.as_ref(),
                &WalkingState {
                    current: current.clone(),
                    remaining: remaining.rewound_to(component_start),
                    hop_count,
                    mount_root: mount_root.clone(),
                    current_mount: current_mount.clone(),
                    mount_root_mount: mount_root_mount.clone(),
                    must_be_directory,
                },
                guard,
            ) {
                Ok(rnode) => rnode,
                Err(KernelStep::NeedIO(req, token)) => return KernelStep::NeedIO(req, token),
                Err(KernelStep::Error(cause)) => return KernelStep::Error(cause),
                Err(_) => {
                    return KernelStep::Error(WalkCause::FsOpsRejected(
                        crate::execution::Errno::EIO,
                    ));
                }
            };

            let child_inline = match InlineName::new(component) {
                Ok(name) => name,
                Err(_) => return KernelStep::Error(WalkCause::ComponentNotFound),
            };
            let mut child_dentry_raw = DEntry::new(child_inline, child_rnode_cap.clone());
            child_dentry_raw.set_parent_hint(&current);
            let child_dentry = match step_engine::sign(child_dentry_raw) {
                Ok(cap) => cap,
                Err(_) => {
                    return KernelStep::Error(WalkCause::FsOpsRejected(
                        crate::execution::Errno::ENOMEM,
                    ));
                }
            };
            let child_dentry =
                current.cache_child_with_version(child_dentry, confirmed_lookup_version);
            if let Some(mount) = current_mount.as_ref() {
                mount.retain_dentry(child_dentry.clone());
            }
            let child_rnode_cap = child_dentry.rnode().clone();
            let child_fs_object_id = child_rnode_cap.fs_object_id();
            let child_meta = child_rnode_cap.meta();
            (
                child_dentry,
                child_rnode_cap,
                child_fs_object_id,
                child_meta,
            )
        }
    };

    // --- mid-path non-directory check ---
    if child_meta.kind() != InodeKind::Directory
        && child_meta.kind() != InodeKind::Symlink
        && (!remaining.is_empty() || must_be_directory)
    {
        return KernelStep::Error(WalkCause::NotADirectory);
    }

    // --- symlink chasing ---
    if let RNodeBacking::Symlink { target } = child_rnode_cap.backing() {
        // NoFollow: if this is the final component and the caller
        // asked us not to follow, return the symlink as terminal.
        if rules.policy == FinalSymlinkPolicy::NoFollow && remaining.is_empty() {
            let rnode = child_rnode_cap.clone();
            let meta = child_meta;
            let fs_object_id = rnode.fs_object_id();
            let resolved = PathResolution {
                dentry: child_dentry,
                rnode,
                fs_object_id,
                meta,
                mount: current_mount,
            };
            return KernelStep::Continue(WalkState::Terminal(resolved));
        }
        let protected_parent_meta = if cache_only {
            parent_meta
        } else {
            match fs_ops.load_inode_meta(parent_fs_object_id, guard) {
                StepOutcome::Done(meta) => meta,
                _ => parent_meta,
            }
        };
        if protected_symlink_follow_denied(cred, &protected_parent_meta, &child_meta) {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EACCES));
        }
        hop_count += 1;
        if hop_count > SYMLOOP_MAX {
            return KernelStep::Error(WalkCause::SymlinkLimit);
        }
        let target_bytes = target.as_ref();
        if target_bytes.first() == Some(&b'/') {
            // Absolute symlink: restart from mount_root.
            let new_remaining = match try_join_path(&target_bytes[1..], &remaining) {
                Ok(remaining) => remaining,
                Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
            };
            return KernelStep::Continue(WalkState::Walking(WalkingState {
                current: mount_root.clone(),
                remaining: new_remaining.into(),
                hop_count,
                mount_root,
                current_mount: mount_root_mount.clone(),
                mount_root_mount,
                must_be_directory,
            }));
        } else {
            // Relative symlink: prepend target to remaining.
            let new_remaining = match try_join_path(target_bytes, &remaining) {
                Ok(remaining) => remaining,
                Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
            };
            return KernelStep::Continue(WalkState::Walking(WalkingState {
                current,
                remaining: new_remaining.into(),
                hop_count,
                mount_root,
                current_mount,
                mount_root_mount,
                must_be_directory,
            }));
        }
    }

    // --- mount-point crossing ---
    let crossing_mount = crossing_mount_for(
        &child_dentry,
        mount_payload.as_ref(),
        mount_namespace,
        child_fs_object_id,
        guard,
    );
    if let Some(mount_cap) = crossing_mount {
        let new_current = match walker::dentry_for_mount_root(&mount_cap, Some(&child_dentry)) {
            Ok(d) => d,
            Err(_) => return KernelStep::Error(WalkCause::MountPointGap),
        };
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current: new_current,
            remaining,
            hop_count,
            mount_root,
            current_mount: Some(mount_cap),
            mount_root_mount,
            must_be_directory,
        }));
    }

    // --- plain advance ---
    KernelStep::Continue(WalkState::Walking(WalkingState {
        current: child_dentry,
        remaining,
        hop_count,
        mount_root,
        current_mount,
        mount_root_mount,
        must_be_directory,
    }))
}

fn protected_symlink_follow_denied(
    cred: &Credential,
    parent_meta: &InodeMeta,
    link_meta: &InodeMeta,
) -> bool {
    let sticky_world_writable =
        (parent_meta.mode & S_ISVTX) != 0 && (parent_meta.mode & 0o002) != 0;
    sticky_world_writable && cred.uid != link_meta.uid && parent_meta.uid != link_meta.uid
}

/// Resolve a mount crossing without mixing namespace-local and legacy-global
/// publication. A namespace-aware walk consults only that namespace's table;
/// the DEntry hint and global table remain compatibility inputs for legacy
/// walks that do not carry a namespace.
fn crossing_mount_for(
    child_dentry: &Cap<DEntry>,
    mount_payload: Option<&Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    child_fs_object_id: crate::vfs::FsObjectId,
    guard: &Guard<'_>,
) -> Option<Cap<MountIdentity>> {
    if let Some(namespace) = mount_namespace {
        // Ordinary compiler paths contain no mount boundary. Reject them
        // before taking the namespace mount-table lock; the conservative
        // filter cannot produce false negatives for completed registrations.
        if !namespace.may_contain_mountpoint_object(child_fs_object_id) {
            return None;
        }
        if let Some(mount) = namespace.mount_for(child_dentry) {
            return Some(mount);
        }
        // The registered mountpoint DEntry is held weakly by its parent's
        // child cache; a mutation invalidation on the parent (e.g. `mkdir
        // /etc` invalidating "/") drops it, after which the walker holds a
        // freshly materialised instance whose cap key cannot match. Fall
        // back to object identity — still namespace-local.
        return mount_payload.and_then(|payload| {
            namespace.mount_for_mountpoint_object(payload, child_fs_object_id)
        });
    }

    child_dentry
        .mounted_hint()
        .and_then(|weak| weak.upgrade(guard))
        .or_else(|| {
            mount_payload.and_then(|payload| crate::mount::mount_for(payload, child_fs_object_id))
        })
}

pub(super) fn kernel_step_after_lookup_io(
    walking: WalkingState,
    fs_ops: Arc<dyn FsOps>,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    expected_parent: crate::vfs::FsObjectId,
    expected_name: &[u8],
    child_fs_object_id: crate::vfs::FsObjectId,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    let pending = match pending_component(walking, cred, guard) {
        Ok(pending) => pending,
        Err(step) => return step,
    };
    let parent_fs_object_id = pending.current.rnode().fs_object_id();
    if parent_fs_object_id != expected_parent || pending.component.as_slice() != expected_name {
        return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EINVAL));
    }

    let child_meta = match fs_ops.load_inode_meta(child_fs_object_id, guard) {
        StepOutcome::Done(meta) => meta,
        StepOutcome::Yield { .. } => {
            let retry_remaining =
                match remaining_with_component(&pending.component, &pending.remaining) {
                    Ok(remaining) => remaining,
                    Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
                };
            let request = IORequest::LoadInodeMeta {
                fs_object_id: child_fs_object_id,
            };
            let resume_request = match try_clone_io_request(&request) {
                Ok(request) => request,
                Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
            };
            return KernelStep::NeedIO(
                request,
                ResumeToken {
                    walking: WalkingState {
                        current: pending.current,
                        remaining: retry_remaining,
                        hop_count: pending.hop_count,
                        mount_root: pending.mount_root,
                        current_mount: pending.current_mount,
                        mount_root_mount: pending.mount_root_mount,
                        must_be_directory: pending.must_be_directory,
                    },
                    request: resume_request,
                    mount_namespace: None,
                    hop_count: pending.hop_count,
                },
            );
        }
        StepOutcome::Err(errno) => {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::from(
                errno,
            )));
        }
        StepOutcome::Continue { .. } => {
            let retry_remaining =
                match remaining_with_component(&pending.component, &pending.remaining) {
                    Ok(remaining) => remaining,
                    Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
                };
            return KernelStep::Continue(WalkState::Walking(WalkingState {
                current: pending.current,
                remaining: retry_remaining,
                hop_count: pending.hop_count,
                mount_root: pending.mount_root,
                current_mount: pending.current_mount,
                mount_root_mount: pending.mount_root_mount,
                must_be_directory: pending.must_be_directory,
            }));
        }
    };

    continue_after_child_meta(
        pending,
        &fs_ops,
        mount_payload,
        mount_namespace,
        child_fs_object_id,
        child_meta,
        cred,
        rules,
        guard,
    )
}

pub(super) fn kernel_step_after_meta_io(
    walking: WalkingState,
    fs_ops: Arc<dyn FsOps>,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    child_fs_object_id: crate::vfs::FsObjectId,
    child_meta: InodeMeta,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    let pending = match pending_component(walking, cred, guard) {
        Ok(pending) => pending,
        Err(step) => return step,
    };
    continue_after_child_meta(
        pending,
        &fs_ops,
        mount_payload,
        mount_namespace,
        child_fs_object_id,
        child_meta,
        cred,
        rules,
        guard,
    )
}

pub(super) fn kernel_step_after_readlink_io(
    walking: WalkingState,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    child_fs_object_id: crate::vfs::FsObjectId,
    child_meta: InodeMeta,
    target: Box<[u8]>,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    let pending = match pending_component(walking, cred, guard) {
        Ok(pending) => pending,
        Err(step) => return step,
    };
    let child_rnode_cap = match RNode::new_cap(
        child_fs_object_id,
        child_meta,
        RNodeBacking::Symlink { target },
    ) {
        Ok(rnode) => rnode,
        Err(_) => {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::ENOMEM));
        }
    };
    continue_after_child_rnode(
        pending,
        mount_payload,
        mount_namespace,
        child_rnode_cap,
        cred,
        rules,
        guard,
    )
}

pub(super) fn kernel_step_after_materialise_io(
    walking: WalkingState,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    child_rnode_cap: Cap<RNode>,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    let pending = match pending_component(walking, cred, guard) {
        Ok(pending) => pending,
        Err(step) => return step,
    };
    continue_after_child_rnode(
        pending,
        mount_payload,
        mount_namespace,
        child_rnode_cap,
        cred,
        rules,
        guard,
    )
}

struct PendingComponent {
    current: Cap<DEntry>,
    component: Vec<u8>,
    child_inline: InlineName,
    remaining: RemainingPath,
    hop_count: u32,
    mount_root: Cap<DEntry>,
    current_mount: Option<Cap<MountIdentity>>,
    mount_root_mount: Option<Cap<MountIdentity>>,
    must_be_directory: bool,
}

fn pending_component(
    walking: WalkingState,
    cred: &Credential,
    guard: &Guard<'_>,
) -> Result<PendingComponent, KernelStep> {
    let WalkingState {
        current,
        remaining,
        hop_count,
        mount_root,
        current_mount,
        mount_root_mount,
        must_be_directory,
    } = walking;

    let ((component_start, component_end), remaining) =
        take_next_component(remaining).ok_or_else(|| {
            KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EINVAL))
        })?;
    let component = try_copy_path(remaining.absolute_slice(component_start, component_end))
        .map_err(|errno| KernelStep::Error(WalkCause::FsOpsRejected(errno)))?;

    if current.rnode().meta().kind() != InodeKind::Directory {
        return Err(KernelStep::Error(WalkCause::NotADirectory));
    }

    let parent_meta = current.rnode().meta();
    if let Err(_err) =
        crate::cred::checks::require_path_search_with_walker_cred(cred, &parent_meta, guard)
    {
        return Err(KernelStep::Error(WalkCause::Permission(
            super::state::NonTerminalDenial::SearchDenied,
        )));
    }

    let child_inline =
        InlineName::new(&component).map_err(|_| KernelStep::Error(WalkCause::ComponentNotFound))?;

    Ok(PendingComponent {
        current,
        component,
        child_inline,
        remaining,
        hop_count,
        mount_root,
        current_mount,
        mount_root_mount,
        must_be_directory,
    })
}

fn continue_after_child_meta(
    pending: PendingComponent,
    fs_ops: &Arc<dyn FsOps>,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    child_fs_object_id: crate::vfs::FsObjectId,
    child_meta: InodeMeta,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    let retry_remaining = match remaining_with_component(&pending.component, &pending.remaining) {
        Ok(remaining) => remaining,
        Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
    };
    let child_rnode_cap = match materialise_child(
        fs_ops,
        child_fs_object_id,
        &child_meta,
        mount_payload.as_ref(),
        &WalkingState {
            current: pending.current.clone(),
            remaining: retry_remaining,
            hop_count: pending.hop_count,
            mount_root: pending.mount_root.clone(),
            current_mount: pending.current_mount.clone(),
            mount_root_mount: pending.mount_root_mount.clone(),
            must_be_directory: pending.must_be_directory,
        },
        guard,
    ) {
        Ok(rnode) => rnode,
        Err(KernelStep::NeedIO(req, token)) => return KernelStep::NeedIO(req, token),
        Err(KernelStep::Error(cause)) => return KernelStep::Error(cause),
        Err(_) => {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EIO));
        }
    };
    continue_after_child_rnode(
        pending,
        mount_payload,
        mount_namespace,
        child_rnode_cap,
        cred,
        rules,
        guard,
    )
}

fn continue_after_child_rnode(
    pending: PendingComponent,
    mount_payload: Option<Cap<MountPayload>>,
    mount_namespace: Option<&Cap<MountNamespace>>,
    child_rnode_cap: Cap<RNode>,
    cred: &Credential,
    rules: TerminalRules,
    guard: &Guard<'_>,
) -> KernelStep {
    let PendingComponent {
        current,
        child_inline,
        remaining,
        mut hop_count,
        mount_root,
        current_mount,
        mount_root_mount,
        must_be_directory,
        ..
    } = pending;

    let mut child_dentry_raw = DEntry::new(child_inline, child_rnode_cap.clone());
    child_dentry_raw.set_parent_hint(&current);
    let child_dentry = match step_engine::sign(child_dentry_raw) {
        Ok(cap) => cap,
        Err(_) => {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::ENOMEM));
        }
    };
    let child_dentry = current.cache_child(child_dentry);
    if let Some(mount) = current_mount.as_ref() {
        mount.retain_dentry(child_dentry.clone());
    }
    let child_rnode_cap = child_dentry.rnode().clone();
    let child_fs_object_id = child_rnode_cap.fs_object_id();
    let child_meta = child_rnode_cap.meta();

    if child_meta.kind() != InodeKind::Directory
        && child_meta.kind() != InodeKind::Symlink
        && (!remaining.is_empty() || must_be_directory)
    {
        return KernelStep::Error(WalkCause::NotADirectory);
    }

    if let RNodeBacking::Symlink { target } = child_rnode_cap.backing() {
        if rules.policy == FinalSymlinkPolicy::NoFollow && remaining.is_empty() {
            let rnode = child_rnode_cap.clone();
            let meta = child_meta;
            let fs_object_id = rnode.fs_object_id();
            let resolved = PathResolution {
                dentry: child_dentry,
                rnode,
                fs_object_id,
                meta,
                mount: current_mount,
            };
            return KernelStep::Continue(WalkState::Terminal(resolved));
        }
        let parent_meta = current.rnode().meta();
        if protected_symlink_follow_denied(cred, &parent_meta, &child_meta) {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EACCES));
        }
        hop_count += 1;
        if hop_count > SYMLOOP_MAX {
            return KernelStep::Error(WalkCause::SymlinkLimit);
        }
        let target_bytes = target.as_ref();
        if target_bytes.first() == Some(&b'/') {
            let new_remaining = match try_join_path(&target_bytes[1..], &remaining) {
                Ok(remaining) => remaining,
                Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
            };
            return KernelStep::Continue(WalkState::Walking(WalkingState {
                current: mount_root.clone(),
                remaining: new_remaining.into(),
                hop_count,
                mount_root,
                current_mount: mount_root_mount.clone(),
                mount_root_mount,
                must_be_directory,
            }));
        }
        let new_remaining = match try_join_path(target_bytes, &remaining) {
            Ok(remaining) => remaining,
            Err(errno) => return KernelStep::Error(WalkCause::FsOpsRejected(errno)),
        };
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current,
            remaining: new_remaining.into(),
            hop_count,
            mount_root,
            current_mount,
            mount_root_mount,
            must_be_directory,
        }));
    }

    let crossing_mount = crossing_mount_for(
        &child_dentry,
        mount_payload.as_ref(),
        mount_namespace,
        child_fs_object_id,
        guard,
    );
    if let Some(mount_cap) = crossing_mount {
        let new_current = match walker::dentry_for_mount_root(&mount_cap, Some(&child_dentry)) {
            Ok(dentry) => dentry,
            Err(_) => return KernelStep::Error(WalkCause::MountPointGap),
        };
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current: new_current,
            remaining,
            hop_count,
            mount_root,
            current_mount: Some(mount_cap),
            mount_root_mount,
            must_be_directory,
        }));
    }

    KernelStep::Continue(WalkState::Walking(WalkingState {
        current: child_dentry,
        remaining,
        hop_count,
        mount_root,
        current_mount,
        mount_root_mount,
        must_be_directory,
    }))
}

fn remaining_with_component(
    component: &[u8],
    remaining: &RemainingPath,
) -> Result<RemainingPath, crate::execution::Errno> {
    if remaining.is_empty() {
        return try_copy_path(component).map(Into::into);
    }
    try_join_path(component, remaining).map(Into::into)
}

fn take_next_component(mut remaining: RemainingPath) -> Option<((usize, usize), RemainingPath)> {
    let component = remaining.take_next_component()?;
    Some((component, remaining))
}

#[cfg(test)]
mod tests {
    use super::{take_next_component, RemainingPath};

    #[test]
    fn take_next_component_collapses_separator_runs_without_front_removal() {
        let ((start, end), remaining) =
            take_next_component(RemainingPath::from_vec(b"////alpha///beta//".to_vec()))
                .expect("component");

        assert_eq!(remaining.absolute_slice(start, end), b"alpha");
        assert_eq!(&*remaining, b"beta");

        let ((start, end), remaining) = take_next_component(remaining).expect("second component");
        assert_eq!(remaining.absolute_slice(start, end), b"beta");
        assert!(remaining.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Internal: materialise_child
// ---------------------------------------------------------------------------

/// Materialise an RNode for a freshly-resolved child inode.
///
/// Returns `Ok(Cap<RNode>)` on success, or `Err(KernelStep)` on
/// yield / error (the driver continues or errors accordingly).
#[allow(clippy::result_large_err)]
pub(super) fn materialise_child(
    fs_ops: &Arc<dyn FsOps>,
    child_fs_object_id: crate::vfs::FsObjectId,
    child_meta: &InodeMeta,
    mount_payload: Option<&Cap<MountPayload>>,
    walking: &WalkingState,
    guard: &Guard<'_>,
) -> Result<Cap<RNode>, KernelStep> {
    match child_meta.kind() {
        InodeKind::Directory => {
            let result = if let Some(mp) = mount_payload {
                RNode::new_cap_in_mount(
                    child_fs_object_id,
                    *child_meta,
                    RNodeBacking::Directory,
                    mp,
                )
            } else {
                super::diagnostic::record_diag(6);
                super::diagnostic::record_label(b"materialise:dir-no-mount");
                RNode::new_cap(child_fs_object_id, *child_meta, RNodeBacking::Directory)
            };
            result.map_err(|_| {
                KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::ENOMEM))
            })
        }
        InodeKind::Symlink => match fs_ops.read_link(child_fs_object_id, guard) {
            StepOutcome::Done(b) => {
                // NOTE: symlink RNode created without containing_mount.
                super::diagnostic::record_diag(7);
                super::diagnostic::record_label(b"materialise:symlink-no-mount");
                RNode::new_cap(
                    child_fs_object_id,
                    *child_meta,
                    RNodeBacking::Symlink { target: b },
                )
                .map_err(|_| {
                    KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::ENOMEM))
                })
            }
            StepOutcome::Yield { .. } => Err(KernelStep::NeedIO(
                IORequest::ReadLink {
                    fs_object_id: child_fs_object_id,
                    meta: *child_meta,
                },
                ResumeToken {
                    walking: walking.clone(),
                    request: IORequest::ReadLink {
                        fs_object_id: child_fs_object_id,
                        meta: *child_meta,
                    },
                    mount_namespace: None,
                    hop_count: walking.hop_count,
                },
            )),
            StepOutcome::Err(e) => Err(KernelStep::Error(WalkCause::FsOpsRejected(
                crate::execution::Errno::from(e),
            ))),
            StepOutcome::Continue { .. } => Err(KernelStep::Error(WalkCause::FsOpsRejected(
                crate::execution::Errno::ENOSYS,
            ))),
        },
        _ => {
            // The walker must stamp the child RNode with the
            // containing mount so subsequent ops can resolve `FsOps`
            // via `containing_mount_weak()`. Without `mount_payload`
            // we can't satisfy `materialise_rnode`'s contract — the
            // root-rnode bootstrap path is the only caller without a
            // mount, and it goes through `mount/mod.rs` directly.
            let Some(mount) = mount_payload else {
                return Err(KernelStep::Error(WalkCause::FsOpsRejected(
                    crate::execution::Errno::EIO,
                )));
            };
            match fs_ops.materialise_rnode(child_fs_object_id, *child_meta, mount, guard) {
                StepOutcome::Done(rnode) => Ok(rnode),
                StepOutcome::Yield { .. } => Err(KernelStep::NeedIO(
                    IORequest::MaterialiseRnode {
                        fs_object_id: child_fs_object_id,
                        meta: *child_meta,
                    },
                    ResumeToken {
                        walking: walking.clone(),
                        request: IORequest::MaterialiseRnode {
                            fs_object_id: child_fs_object_id,
                            meta: *child_meta,
                        },
                        mount_namespace: None,
                        hop_count: walking.hop_count,
                    },
                )),
                StepOutcome::Err(e) => Err(KernelStep::Error(WalkCause::FsOpsRejected(
                    crate::execution::Errno::from(e),
                ))),
                StepOutcome::Continue { .. } => Err(KernelStep::Error(WalkCause::FsOpsRejected(
                    crate::execution::Errno::ENOSYS,
                ))),
            }
        }
    }
}
