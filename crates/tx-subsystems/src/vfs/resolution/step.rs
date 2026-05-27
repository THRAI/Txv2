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

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::execution::Guard;
use crate::mount::{MountNamespace, MountPayload};
use crate::vfs::adapter::step_engine::{self, Cap, StepOutcome};
use crate::vfs::structure::{
    Credential, DEntry, InlineName, InodeKind, InodeMeta, RNode, RNodeBacking, S_ISVTX,
};
use crate::vfs::walker::{self, SYMLOOP_MAX};
use crate::vfs::FsOps;

use super::state::{
    FinalSymlinkPolicy, IORequest, KernelStep, PathResolution, ResumeToken, WalkCause, WalkMode,
    WalkState, WalkingState,
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
    let WalkingState {
        mut current,
        mut remaining,
        mut hop_count,
        mount_root,
        must_be_directory,
    } = walking;

    // --- collapse leading slashes ---
    while remaining.first() == Some(&b'/') {
        remaining.remove(0);
    }

    if rules.mode == WalkMode::ParentAndName && is_single_remaining_component(&remaining) {
        if current.rnode().meta().kind() != InodeKind::Directory {
            return KernelStep::Error(WalkCause::NotADirectory);
        }
        let parent_meta = current.rnode().meta();
        if let Err(_err) =
            crate::cred::checks::require_path_search_with_walker_cred(cred, &parent_meta, guard)
        {
            return KernelStep::Error(WalkCause::Permission(
                super::state::NonTerminalDenial::SearchDenied,
            ));
        }
        let rnode = current.rnode().clone();
        let meta = rnode.meta();
        let fs_object_id = rnode.fs_object_id();
        let resolved = PathResolution {
            dentry: current,
            rnode,
            fs_object_id,
            meta,
        };
        return KernelStep::Continue(WalkState::Terminal(resolved));
    }

    // --- end of input ---
    if remaining.is_empty() {
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
        };
        if terminal::accepts(&WalkState::Terminal(resolved.clone()), rules.mode) {
            return KernelStep::Continue(WalkState::Terminal(resolved));
        }
        return KernelStep::Error(WalkCause::ComponentNotFound);
    }

    // --- extract next component ---
    let next_slash = remaining
        .iter()
        .position(|b| *b == b'/')
        .unwrap_or(remaining.len());
    let component: Vec<u8> = remaining.drain(..next_slash).collect();
    if remaining.first() == Some(&b'/') {
        remaining.remove(0);
    }

    // --- `.` and `..` ---
    if component == b"." {
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current,
            remaining,
            hop_count,
            mount_root,
            must_be_directory,
        }));
    }
    if component == b".." {
        if let Some(parent_cap) = current.parent_hint() {
            let is_mount_root = walker::is_same_dentry(&current, &mount_root);
            if !is_mount_root {
                current = parent_cap;
            }
        }
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current,
            remaining,
            hop_count,
            mount_root,
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

    let child_inline = match InlineName::new(&component) {
        Ok(n) => n,
        Err(_) => return KernelStep::Error(WalkCause::ComponentNotFound),
    };

    // --- lookup / materialise, with parent-local dentry cache ---
    let parent_fs_object_id = current.rnode().fs_object_id();
    let (child_dentry, child_rnode_cap, child_fs_object_id, child_meta) = if let Some(cached) =
        current
            .cached_child(child_inline)
            .filter(|dentry| dentry.rnode().meta().kind() == InodeKind::Directory)
    {
        let rnode = cached.rnode().clone();
        let fs_object_id = rnode.fs_object_id();
        let meta = rnode.meta();
        (cached, rnode, fs_object_id, meta)
    } else {
        let child_fs_object_id = match fs_ops.lookup(parent_fs_object_id, &component, guard) {
            StepOutcome::Done(id) => id,
            StepOutcome::Yield { .. } => {
                let token = ResumeToken {
                    walking: WalkingState {
                        current,
                        remaining,
                        hop_count,
                        mount_root,
                        must_be_directory,
                    },
                    hop_count,
                };
                return KernelStep::NeedIO(
                    IORequest::DirLookup {
                        fs_object_id: parent_fs_object_id,
                        name: component.into_boxed_slice(),
                    },
                    token,
                );
            }
            StepOutcome::Err(e) => return KernelStep::Error(WalkCause::FsOpsRejected(e)),
            StepOutcome::Continue { .. } => {
                // Re-enter lookup (v3 continue without yield).
                return KernelStep::Continue(WalkState::Walking(WalkingState {
                    current,
                    remaining,
                    hop_count,
                    mount_root,
                    must_be_directory,
                }));
            }
        };

        let child_meta = match fs_ops.load_inode_meta(child_fs_object_id, guard) {
            StepOutcome::Done(m) => m,
            StepOutcome::Yield { .. } => {
                let token = ResumeToken {
                    walking: WalkingState {
                        current,
                        remaining,
                        hop_count,
                        mount_root,
                        must_be_directory,
                    },
                    hop_count,
                };
                return KernelStep::NeedIO(
                    IORequest::LoadInodeMeta {
                        fs_object_id: child_fs_object_id,
                    },
                    token,
                );
            }
            StepOutcome::Err(e) => return KernelStep::Error(WalkCause::FsOpsRejected(e)),
            StepOutcome::Continue { .. } => {
                return KernelStep::Continue(WalkState::Walking(WalkingState {
                    current,
                    remaining,
                    hop_count,
                    mount_root,
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
                remaining: remaining.clone(),
                hop_count,
                mount_root: mount_root.clone(),
                must_be_directory,
            },
            guard,
        ) {
            Ok(rnode) => rnode,
            Err(KernelStep::NeedIO(req, token)) => return KernelStep::NeedIO(req, token),
            Err(KernelStep::Error(cause)) => return KernelStep::Error(cause),
            Err(_) => {
                return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EIO))
            }
        };

        let mut child_dentry_raw = DEntry::new(child_inline, child_rnode_cap.clone());
        child_dentry_raw.set_parent_hint(&current);
        let child_dentry = match step_engine::sign(child_dentry_raw) {
            Ok(cap) => cap,
            Err(_) => {
                return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::ENOMEM))
            }
        };
        if child_meta.kind() == InodeKind::Directory {
            current.cache_child(child_dentry.clone());
        }
        (
            child_dentry,
            child_rnode_cap,
            child_fs_object_id,
            child_meta,
        )
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
            };
            return KernelStep::Continue(WalkState::Terminal(resolved));
        }
        let protected_parent_meta = match fs_ops.load_inode_meta(parent_fs_object_id, guard) {
            StepOutcome::Done(meta) => meta,
            _ => parent_meta,
        };
        if protected_symlink_follow_denied(cred, &protected_parent_meta, &child_meta) {
            return KernelStep::Error(WalkCause::FsOpsRejected(crate::execution::Errno::EACCES));
        }
        hop_count += 1;
        if hop_count > SYMLOOP_MAX {
            return KernelStep::Error(WalkCause::SymlinkLimit);
        }
        let target_bytes = target.clone();
        if target_bytes.first() == Some(&b'/') {
            // Absolute symlink: restart from mount_root.
            let mut new_remaining = Vec::with_capacity(target_bytes.len() + remaining.len() + 1);
            new_remaining.extend_from_slice(&target_bytes[1..]);
            if !remaining.is_empty() {
                new_remaining.push(b'/');
                new_remaining.extend_from_slice(&remaining);
            }
            return KernelStep::Continue(WalkState::Walking(WalkingState {
                current: mount_root.clone(),
                remaining: new_remaining,
                hop_count,
                mount_root,
                must_be_directory,
            }));
        } else {
            // Relative symlink: prepend target to remaining.
            let mut new_remaining = Vec::with_capacity(target_bytes.len() + remaining.len() + 1);
            new_remaining.extend_from_slice(&target_bytes);
            if !remaining.is_empty() {
                new_remaining.push(b'/');
                new_remaining.extend_from_slice(&remaining);
            }
            return KernelStep::Continue(WalkState::Walking(WalkingState {
                current,
                remaining: new_remaining,
                hop_count,
                mount_root,
                must_be_directory,
            }));
        }
    }

    // --- mount-point crossing ---
    let crossing_mount = child_dentry.mounted_hint().and_then(|w| w.upgrade(guard));
    let crossing_mount = match crossing_mount {
        Some(m) => Some(m),
        None => mount_payload.as_ref().and_then(|payload| {
            mount_namespace
                .and_then(|ns| ns.mount_for(payload, child_fs_object_id))
                .or_else(|| {
                    if mount_namespace.is_some() {
                        None
                    } else {
                        crate::mount::mount_for(payload, child_fs_object_id)
                    }
                })
        }),
    };
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
            must_be_directory,
        }));
    }

    // --- plain advance ---
    KernelStep::Continue(WalkState::Walking(WalkingState {
        current: child_dentry,
        remaining,
        hop_count,
        mount_root,
        must_be_directory,
    }))
}

fn is_single_remaining_component(remaining: &[u8]) -> bool {
    !remaining.is_empty() && !remaining.contains(&b'/')
}

// ---------------------------------------------------------------------------
// Internal: materialise_child
// ---------------------------------------------------------------------------

/// Materialise an RNode for a freshly-resolved child inode.
///
/// Returns `Ok(Cap<RNode>)` on success, or `Err(KernelStep)` on
/// yield / error (the driver continues or errors accordingly).
#[allow(clippy::result_large_err)]
fn materialise_child(
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
                },
                ResumeToken {
                    walking: walking.clone(),
                    hop_count: walking.hop_count,
                },
            )),
            StepOutcome::Err(e) => Err(KernelStep::Error(WalkCause::FsOpsRejected(e))),
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
                        hop_count: walking.hop_count,
                    },
                )),
                StepOutcome::Err(e) => Err(KernelStep::Error(WalkCause::FsOpsRejected(e))),
                StepOutcome::Continue { .. } => Err(KernelStep::Error(WalkCause::FsOpsRejected(
                    crate::execution::Errno::ENOSYS,
                ))),
            }
        }
    }
}
