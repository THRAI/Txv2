//! VFS path walker (`step_walk`) and open-by-path (`step_open`).
//!
//! The walker resolves a `&[u8]` path rooted at a `Cap<DEntry>` into a
//! terminal `Cap<DEntry>`, honouring mount-point boundaries and
//! symlink chasing per
//! `txdoc:VFS-CHECKS-RUN-WALKER-LOOP-1`,
//! `txdoc:VFS-CHECKS-MOUNT-BOUNDARY-DISCIPLINE-1`,
//! `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`, and the symlink budget
//! rule cited in `docs/design/05_filesystem/VFS_CHECKS_V2.1.md`
//! (`SymlinkBudgetExceeded → ELOOP`).
//!
//! The walker is the production "absent today" piece per the trio
//! decision (`docs/progress/decisions/2026-05-06-trio-trap-syscall-tmpfs-devfs.md`)
//! and Part 3 of the pre-ELF runtime plan
//! (`docs/progress/plans/2026-05-06-pre-elf-runtime-completion.md`).
//!
//! ## Surface
//!
//! - [`step_walk`] returns the terminal `Cap<DEntry>` for a path.
//! - [`step_open`] composes [`step_walk`] with `OpenFile::new_cap` to
//!   produce a `Cap<OpenFile>` over the resolved RNode.
//!
//! Both are `async` so future cross-await disciplines (range-lock
//! waits inside `FsOps::lookup`, page-cache materialisation inside
//! `OpenFile::step_read` for regular files) compose cleanly. The
//! day-1 implementations call only synchronous `FsOps::lookup` /
//! `load_inode_meta` / `read_link` against in-memory backends, so
//! every `.await` is a no-op today.
//!
//! ## Mount-point crossing
//!
//! When a freshly-resolved DEntry carries a `mounted: Some(Weak<MountIdentity>)`
//! hint (set at mount-publication time per
//! `txdoc:MOUNT-STEP-MOUNT-COMMIT-ORDERING-1`, see
//! `crate::mount::MountIdentity`), the walker upgrades the weak,
//! switches the active filesystem to the mount's `payload().fs_ops`,
//! and continues from a fresh DEntry over `mount.root()`. If the
//! upgrade fails (mount torn down mid-walk), the walker reports
//! `Errno::EIO` (`Errno::ENXIO` is not in the day-1 set; the
//! mount-tear-down errno can sharpen in a follow-up).
//!
//! ## Symlink chasing
//!
//! Per Open Q #3 (decided 2026-05-06): chase up to `SYMLOOP_MAX = 40`
//! hops; the 41st observation returns `Errno::ELOOP`. Targets are
//! substituted into the remaining component stream:
//!
//! - **Absolute target** (`/` prefix): traversal restarts from the
//!   walk's "mount root" (the namespace root reachable from the
//!   `rooted_at` argument) with `target[1..]` prepended to the
//!   remaining path.
//! - **Relative target**: traversal continues from the symlink's
//!   parent DEntry with `target` prepended to the remaining path.
//!
//! ## Permissions
//!
//! `Credential` is threaded through but the slice defers mode-bit
//! checking; matches the existing `bind_init_cwd_and_root` "always
//! allow for init" surface.

use alloc::sync::Arc;
use alloc::vec::Vec;

use tx_substrate::zone::{Cap, Weak};

use crate::execution::{Errno, Guard, StepOutcome};
use crate::mount::{self, MountIdentity, MountPayload};
use crate::vfs::structure::{
    Credential, DEntry, FsObjectId, InlineName, InodeKind, InodeMeta, OpenFile, OpenFileFlags,
    RNode, RNodeBacking,
};
use crate::vfs::FsOps;

/// POSIX symlink-loop budget. Matches Linux's `MAXSYMLINKS = 40`.
/// The 41st observed symlink (after 40 hops have already been
/// substituted into the path) returns `Errno::ELOOP`.
pub const SYMLOOP_MAX: u32 = 40;

/// Resolve `path` against the namespace rooted at `rooted_at` and
/// return the terminal `Cap<DEntry>`.
///
/// `rooted_at` is interpreted as the cwd-or-root for relative paths.
/// Absolute paths (leading `/`) restart traversal from
/// `rooted_at`'s mount-root (chroot-bounded). The `cred` parameter is
/// threaded through every component for the future DAC mode check
/// (see module-level "Permissions").
///
/// Returns the terminal `Cap<DEntry>`. Errors map to the standard set:
/// `ENOENT` (missing component), `ENOTDIR` (non-directory in the
/// middle of a walk, or trailing `/` after a non-directory),
/// `ELOOP` (symlink budget exceeded), `ENAMETOOLONG` (component too
/// long), and propagated `FsOps` errors.
pub async fn step_walk<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<DEntry>> {
    // TODO(phase-vfs-perms): implement DAC mode check via cred and
    // inode meta. The slice always allows; the parameter stays in
    // the signature so the future check has a place to land.
    let _ = cred;
    walk_inner(rooted_at, path, guard)
}

/// Open a path by name. Resolves via [`step_walk`], then materialises
/// an `OpenFile` over the terminal RNode.
pub async fn step_open<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    flags: OpenFileFlags,
    mode: u16,
    cred: &Credential,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<OpenFile>> {
    // Phase 4 slice: `mode` is observed for future create-on-open
    // semantics (`O_CREAT`); the current surface only resolves
    // existing entries. `OpenFile::step_*` already routes
    // `StructBacked { Tty }` and `Directory` correctly; tmpfs
    // page-backed branches still return `ENOSYS` per the trio
    // Phase 3a contract.
    let _ = mode;

    let dentry = match step_walk(rooted_at, path, cred, guard).await {
        StepOutcome::Done(d) | StepOutcome::Advanced(d) => d,
        StepOutcome::AdvancedThenBlocked(_, token) | StepOutcome::Blocked(token) => {
            return StepOutcome::Blocked(token);
        }
        StepOutcome::Err(err) => return StepOutcome::Err(err),
    };

    let rnode = dentry.rnode().clone();
    match OpenFile::new_cap(rnode, flags) {
        Ok(open) => StepOutcome::Done(open),
        Err(_) => StepOutcome::Err(Errno::EIO),
    }
}

// === walker internals =================================================

/// Synchronous core of `step_walk`. The walker takes a single
/// `&Guard<'_>` borrowed from the caller's frame; it is not held
/// across an `.await` because the walker itself never awaits today.
/// When ext4 / page-cache backends grow real waits, each backend
/// call site will take a fresh per-call guard inside the
/// `await_*` helper, matching `vm::execution::fault_script`.
fn walk_inner<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    guard: &Guard<'g>,
) -> StepOutcome<Cap<DEntry>> {
    let mount_root = mount_root_dentry(&rooted_at, guard);

    let mut current: Cap<DEntry>;
    let mut remaining: Vec<u8>;
    let must_be_directory: bool;

    if path.first() == Some(&b'/') {
        current = mount_root.clone();
        remaining = path[1..].to_vec();
    } else {
        current = rooted_at.clone();
        remaining = path.to_vec();
    }

    must_be_directory = remaining.last().copied() == Some(b'/');

    let mut current_fs_ops: Option<Arc<dyn FsOps>> = fs_ops_for(&current, guard);
    let mut current_mount_payload: Option<Cap<MountPayload>> = mount_payload_for(&current, guard);

    let mut hop_count: u32 = 0;

    loop {
        // Collapse leading `///` runs.
        while remaining.first() == Some(&b'/') {
            remaining.remove(0);
        }

        if remaining.is_empty() {
            // End of input: enforce the trailing-`/` directory rule.
            if must_be_directory && current.rnode().meta().kind() != InodeKind::Directory {
                return StepOutcome::Err(Errno::ENOTDIR);
            }
            return StepOutcome::Done(current);
        }

        // Extract the next component.
        let next_slash = remaining
            .iter()
            .position(|b| *b == b'/')
            .unwrap_or(remaining.len());
        let component: Vec<u8> = remaining.drain(..next_slash).collect();
        if remaining.first() == Some(&b'/') {
            remaining.remove(0);
        }

        // `.` and `..`.
        if component == b"." {
            continue;
        }
        if component == b".." {
            if let Some(parent_weak) = current.parent_hint() {
                if let Some(parent_cap) = parent_weak.upgrade(guard) {
                    if !is_same_dentry(&current, &mount_root) {
                        current = parent_cap;
                        current_fs_ops = fs_ops_for(&current, guard);
                        current_mount_payload = mount_payload_for(&current, guard);
                    }
                }
            }
            continue;
        }

        // Interior components require the current to be a directory.
        if current.rnode().meta().kind() != InodeKind::Directory {
            return StepOutcome::Err(Errno::ENOTDIR);
        }

        let parent_fs_object_id = current.rnode().fs_object_id();
        let fs_ops = match &current_fs_ops {
            Some(ops) => ops.clone(),
            None => return StepOutcome::Err(Errno::ENODEV),
        };
        let child_fs_object_id = match fs_ops.lookup(parent_fs_object_id, &component, guard) {
            StepOutcome::Done(id) | StepOutcome::Advanced(id) => id,
            StepOutcome::AdvancedThenBlocked(_, token) | StepOutcome::Blocked(token) => {
                return StepOutcome::Blocked(token);
            }
            StepOutcome::Err(err) => return StepOutcome::Err(err),
        };

        let child_meta = match fs_ops.load_inode_meta(child_fs_object_id, guard) {
            StepOutcome::Done(m) | StepOutcome::Advanced(m) => m,
            StepOutcome::AdvancedThenBlocked(_, token) | StepOutcome::Blocked(token) => {
                return StepOutcome::Blocked(token);
            }
            StepOutcome::Err(err) => return StepOutcome::Err(err),
        };

        // Mid-path non-directory check.
        //
        // If the resolved inode is a non-directory, non-symlink kind
        // (Regular / CharDevice / BlockDevice / Fifo / Socket) and
        // there are more components after this one *or* the input
        // path ended with `/` and the current component is the last
        // one, return `ENOTDIR`. Symlinks pass through here so the
        // chase loop downstream can substitute the target before
        // re-evaluating the directory invariant.
        if child_meta.kind() != InodeKind::Directory
            && child_meta.kind() != InodeKind::Symlink
            && (!remaining.is_empty() || must_be_directory)
        {
            return StepOutcome::Err(Errno::ENOTDIR);
        }

        let child_rnode_cap =
            match materialise_child_rnode(&fs_ops, child_fs_object_id, child_meta, guard) {
                StepOutcome::Done(rnode) | StepOutcome::Advanced(rnode) => rnode,
                StepOutcome::AdvancedThenBlocked(_, token) | StepOutcome::Blocked(token) => {
                    return StepOutcome::Blocked(token);
                }
                StepOutcome::Err(err) => return StepOutcome::Err(err),
            };

        let child_inline = match InlineName::new(&component) {
            Ok(n) => n,
            Err(err) => return StepOutcome::Err(err),
        };
        let mut child_dentry_raw = DEntry::new(child_inline, child_rnode_cap.clone());
        child_dentry_raw.set_parent_hint(&current);
        let child_dentry = match tx_substrate::zone::reserve_for::<DEntry>() {
            Ok(res) => tx_substrate::zone::sign_for(res, child_dentry_raw),
            Err(_) => return StepOutcome::Err(Errno::ENOMEM),
        };

        // === Symlink chasing ============================================
        //
        // Observe symlinks BEFORE mount crossing because a mount
        // hint always references a directory dentry, not a symlink
        // dentry; if a backend ever publishes both on the same
        // dentry, the symlink takes precedence (POSIX
        // `O_NOFOLLOW`-shaped semantics).
        if let RNodeBacking::Symlink { target } = child_rnode_cap.backing() {
            hop_count += 1;
            if hop_count > SYMLOOP_MAX {
                return StepOutcome::Err(Errno::ELOOP);
            }
            let target_bytes = target.clone();
            if target_bytes.first() == Some(&b'/') {
                // Absolute symlink target: restart from the
                // namespace root with `target[1..]` followed by
                // the unconsumed remainder.
                current = mount_root.clone();
                current_fs_ops = fs_ops_for(&current, guard);
                current_mount_payload = mount_payload_for(&current, guard);
                let mut new_remaining =
                    Vec::with_capacity(target_bytes.len() + remaining.len() + 1);
                new_remaining.extend_from_slice(&target_bytes[1..]);
                if !remaining.is_empty() {
                    new_remaining.push(b'/');
                    new_remaining.extend_from_slice(&remaining);
                }
                remaining = new_remaining;
            } else {
                // Relative symlink target: continue from the
                // symlink's *parent* DEntry (which is `current`,
                // not `child_dentry`).
                let mut new_remaining =
                    Vec::with_capacity(target_bytes.len() + remaining.len() + 1);
                new_remaining.extend_from_slice(&target_bytes);
                if !remaining.is_empty() {
                    new_remaining.push(b'/');
                    new_remaining.extend_from_slice(&remaining);
                }
                remaining = new_remaining;
            }
            continue;
        }

        // === Mount-point crossing =======================================
        //
        // Two paths converge here:
        //
        // 1. The freshly-built `child_dentry`'s `mounted_hint` is
        //    set (set explicitly by callers that supply a
        //    pre-cached mount-point dentry — see
        //    `DEntry::set_mounted_hint`).
        // 2. The kernel's mount table (`mount::register_mount`)
        //    has an entry for `(parent_mount_payload,
        //    child_fs_object_id)`. This is the production path:
        //    init.rs's `mount_devfs_at_dev` registers the entry
        //    when devfs is published at `/dev`, and the walker
        //    looks it up here.
        //
        // The two paths produce the same downstream state: a
        // fresh DEntry over the mount's root rnode, with
        // `current_fs_ops` switched to the new mount's
        // `payload.fs_ops`.
        let crossing_mount = child_dentry.mounted_hint().and_then(|w| w.upgrade(guard));
        let crossing_mount = match crossing_mount {
            Some(m) => Some(m),
            None => current_mount_payload
                .as_ref()
                .and_then(|payload| mount::mount_for(payload, child_fs_object_id)),
        };
        if let Some(mount_cap) = crossing_mount {
            current = match dentry_for_mount_root(&mount_cap) {
                Ok(d) => d,
                Err(err) => return StepOutcome::Err(err),
            };
            current_fs_ops = Some(mount_cap.payload().fs_ops.clone());
            current_mount_payload = Some(mount_cap.payload().clone());
            continue;
        }

        // === Plain advance ==============================================
        current = child_dentry;
        // `current_fs_ops` and `current_mount_payload` do not
        // change for an in-mount advance; the `containing_mount`
        // chain is constant within a single mount.
    }
}

/// Build a `Cap<DEntry>` over a mount's root RNode. Used when the
/// walker crosses a mount boundary or restarts from the namespace
/// root for an absolute symlink target.
fn dentry_for_mount_root(mount: &Cap<MountIdentity>) -> Result<Cap<DEntry>, Errno> {
    let raw = DEntry::new(InlineName::ROOT, mount.root().clone());
    let res = tx_substrate::zone::reserve_for::<DEntry>().map_err(|_| Errno::ENOMEM)?;
    Ok(tx_substrate::zone::sign_for(res, raw))
}

/// Walk `from`'s parent-hint chain to find the namespace's root
/// dentry. Returns `from` itself when no parent hint is installed.
fn mount_root_dentry<'g>(from: &Cap<DEntry>, guard: &Guard<'g>) -> Cap<DEntry> {
    let mut cursor: Cap<DEntry> = from.clone();
    while let Some(parent_weak) = cursor.parent_hint() {
        let Some(parent_cap) = parent_weak.upgrade(guard) else {
            break;
        };
        cursor = parent_cap;
    }
    cursor
}

/// Materialise an `RNode` for a freshly-resolved child inode.
///
/// - `Directory` → `RNodeBacking::Directory`.
/// - `Symlink` → call `FsOps::read_link` and wrap the bytes in
///   `RNodeBacking::Symlink { target }`.
/// - Other kinds (Regular / CharDevice / BlockDevice / Fifo / Socket)
///   return `Errno::ENOSYS`. The trio's bootstrap path
///   (`/dev/console`) only crosses `Directory` and `StructBacked
///   { Tty }` boundaries; the latter is materialised by devfs's
///   own `resolve_console_rnode` and does not flow through the
///   walker's lookup loop today (devfs `lookup` returns a
///   `FsObjectId` for the alias, but a future devfs hook will
///   wrap that as `StructBacked { Tty }`). When ext4 or
///   page-backed regular files land, the `Regular` arm grows a
///   `RNodeBacking::PageBacked { pc }` materialiser here.
fn materialise_child_rnode<'g>(
    fs_ops: &Arc<dyn FsOps>,
    child_fs_object_id: FsObjectId,
    meta: InodeMeta,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<RNode>> {
    match meta.kind() {
        InodeKind::Directory => {
            match RNode::new_cap(child_fs_object_id, meta, RNodeBacking::Directory) {
                Ok(rnode) => StepOutcome::Done(rnode),
                Err(_) => StepOutcome::Err(Errno::ENOMEM),
            }
        }
        InodeKind::Symlink => {
            let target_bytes = match fs_ops.read_link(child_fs_object_id, guard) {
                StepOutcome::Done(b) | StepOutcome::Advanced(b) => b,
                StepOutcome::AdvancedThenBlocked(_, token) | StepOutcome::Blocked(token) => {
                    return StepOutcome::Blocked(token);
                }
                StepOutcome::Err(err) => return StepOutcome::Err(err),
            };
            match RNode::new_cap(
                child_fs_object_id,
                meta,
                RNodeBacking::Symlink {
                    target: target_bytes,
                },
            ) {
                Ok(rnode) => StepOutcome::Done(rnode),
                Err(_) => StepOutcome::Err(Errno::ENOMEM),
            }
        }
        // Other kinds (Regular / CharDevice / BlockDevice / Fifo /
        // Socket) require backend-specific RNode materialisation
        // (page container or struct-backed payload). Delegate to
        // the FS's `materialise_rnode` hook; default impl returns
        // ENOSYS for backends that don't yet support the kind.
        _ => fs_ops.materialise_rnode(child_fs_object_id, meta, guard),
    }
}

/// Resolve the FsOps in scope for a given dentry by upgrading its
/// RNode's containing-mount weak.
fn fs_ops_for<'g>(dentry: &Cap<DEntry>, guard: &Guard<'g>) -> Option<Arc<dyn FsOps>> {
    let mount_payload_weak: Weak<MountPayload> = dentry.rnode().containing_mount_weak()?;
    let payload = mount_payload_weak.upgrade(guard)?;
    Some(payload.fs_ops.clone())
}

/// Resolve the `Cap<MountPayload>` in scope for a given dentry by
/// upgrading its RNode's containing-mount weak. Used as the parent
/// key for `mount::mount_for` lookups during a walk.
fn mount_payload_for<'g>(dentry: &Cap<DEntry>, guard: &Guard<'g>) -> Option<Cap<MountPayload>> {
    let weak: Weak<MountPayload> = dentry.rnode().containing_mount_weak()?;
    weak.upgrade(guard)
}

/// Compare two dentries by name + RNode FsObjectId. Used by the
/// `..` ascend-only-up-to-mount-root rule. The comparison is
/// approximate (it does not check zone identity) but sufficient for
/// the boot-time chroot-shaped namespace.
fn is_same_dentry(a: &Cap<DEntry>, b: &Cap<DEntry>) -> bool {
    a.rnode().fs_object_id() == b.rnode().fs_object_id()
        && a.name().as_bytes() == b.name().as_bytes()
}

#[cfg(test)]
mod tests;
