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
//! - [`step_open`] composes [`step_walk`] with `OpenFile::new_cap`
//!   to produce a `Cap<OpenFile>` over the resolved RNode.
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
//! Per `txdoc:VFS-CHECKS-PERMISSIONS-1`
//! (`docs/design/05_filesystem/VFS_CHECKS_V2.1.md`):
//!
//! - At each *intermediate* directory component the walker enforces
//!   POSIX search (`X`) permission via [`check_descend_perm`]. A
//!   caller without the relevant `X` bit on the parent inode's mode
//!   triplet receives `Errno::EACCES` (mapping to `WalkCause::
//!   TraverseDenied` for spec-trace consumers); `CAP_DAC_OVERRIDE`
//!   short-circuits.
//! - At terminal-component open ([`step_open`]), [`check_open_perm`]
//!   validates the requested `OpenFileFlags { read, write }` against
//!   the inode's mode bits using the same triplet selection rule.
//!   Execute permission for `exec_script` is enforced separately at
//!   exec time (Wave 4); `step_open` only enforces R/W.
//!
//! Both checks consult the **effective** uid/gid (the
//! `Credential` projection of `Cred` already does this — see
//! `Credential::from(&Cred)` in `crate::vfs::structure`).
//! `Credential::default()` carries `effective_caps =
//! CapabilitySet::EMPTY`, so a default-constructed credential is a
//! fully unprivileged uid-0 caller. Production bootstrap paths use
//! [`Credential::root`] when the caller is root by construction.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::vfs::adapter::step_engine::{self, Cap, NoProgress, StepOutcome, Weak};

use crate::cred::Capability;
use crate::execution::{Errno, Guard};
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

// === Walker entry points =============================================
//
// The walker routes through `FsOps` and emits
// `StepOutcome`.
//
// Per-call-site `Continue` mapping: `FsOps::lookup` /
// `FsOps::load_inode_meta` / `FsOps::read_link` /
// `FsOps::materialise_rnode` are all one-shot identity-side queries
// with `NoProgress`. The trait surface contract (per the existing
// impls in `tmpfs.rs`, `devfs.rs`, `namespace.rs`, `pager.rs`,
// `tty/project.rs`) returns only `Done`, `Continue { progress:
// NoProgress }`, `Yield`, and `Err`. A `Continue` with `NoProgress`
// is treated as a no-op retry — the walker loops once more without
// advancing path state, which matches the monoid contract
// (`NoProgress` is the EMPTY identity). A `Yield` carries no progress
// for these one-shot ops, so it's surfaced verbatim.

/// Resolve `path` against the namespace rooted at `rooted_at` and
/// return the terminal `Cap<DEntry>`.
///
/// `rooted_at` is interpreted as the cwd-or-root for relative paths.
/// Absolute paths (leading `/`) restart traversal from `rooted_at`'s
/// mount-root (chroot-bounded). The `cred` parameter is threaded
/// through every component for the DAC search check at each
/// intermediate directory (see module-level "Permissions").
///
/// Returns the terminal `Cap<DEntry>`. Errors map to the standard set:
/// `ENOENT` (missing component), `ENOTDIR` (non-directory in the
/// middle of a walk, or trailing `/` after a non-directory), `ELOOP`
/// (symlink budget exceeded), `ENAMETOOLONG` (component too long),
/// and propagated `FsOps` errors. Resolution goes through the
/// `FsOps` trait surface and emits `StepOutcome<Cap<DEntry>,
/// NoProgress>`.
///
/// **WALKER-CARVEOUT-1** (per
/// [`docs/progress/decisions/2026-05-11-d3-walker-async-carveout.md`](../../../../../../docs/progress/decisions/2026-05-11-d3-walker-async-carveout.md)):
/// `step_walk` and [`step_open`] are **script-level async resolvers**,
/// not `StepOp` implementations. PR-2 explicitly skipped wrapping them.
/// They must still obey the same external yield safety rules:
/// - no `epoch::Guard` across `.await`
/// - no witness across `.await`
/// - no reservation guard across `.await`
/// - resume revalidates path state
///
/// A future dedicated VFS PR may introduce a `PathResolveOp` state
/// machine; that work is out of scope for v3 foundation.
pub async fn step_walk<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<DEntry>, NoProgress> {
    walk_inner_v3(rooted_at, path, cred, guard)
}

/// Open a path by name. Composes [`step_walk`] with
/// `OpenFile::new_cap` and the DAC R/W check, returning a
/// `Cap<OpenFile>` over the resolved RNode.
pub async fn step_open<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    flags: OpenFileFlags,
    mode: u16,
    cred: &Credential,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<OpenFile>, NoProgress> {
    use StepOutcome as V3;

    // `mode` is reserved for future create-on-open semantics; the
    // current surface only resolves existing entries.
    let _ = mode;

    let dentry = match step_walk(rooted_at, path, cred, guard).await {
        V3::Done(d) => d,
        V3::Continue { .. } => {
            // `walk_inner_v3` only returns `Done` / `Yield` / `Err` at
            // the top level — every `Continue` is consumed by the
            // inner loop. Defence in depth: an unexpected `Continue`
            // at this layer means the walker yielded with no progress
            // and fell through; surface as `EAGAIN`.
            return V3::err(step_engine::Errno::EAGAIN);
        }
        V3::Yield { progress, shape } => return V3::Yield { progress, shape },
        V3::Err(err) => return V3::err(err),
    };

    // Validate the requested open mode against the terminal inode's
    // R/W permission bits.
    let terminal_meta = dentry.rnode().meta();
    if let Err(err) = check_open_perm(&terminal_meta, flags, cred) {
        return V3::err(err.into());
    }

    let rnode = dentry.rnode().clone();
    match OpenFile::new_cap(rnode, flags) {
        Ok(open) => V3::done(open),
        Err(_) => V3::err(step_engine::Errno::EIO),
    }
}

/// Synchronous core of [`step_walk`].
///
/// The walker takes a single `&Guard<'_>` borrowed from the caller's
/// frame; it is not held across an `.await` because the walker itself
/// never awaits today. When ext4 / page-cache backends grow real
/// waits, each backend call site will take a fresh per-call guard
/// inside the `await_*` helper, matching `vm::execution::fault_script`.
///
/// All FS calls go through the `FsOps` trait surface
/// (`Arc<dyn FsOps>`); every `StepOutcome::*` / `Errno::*` is the v3
/// variant. Mount-crossing rebuilds the v3 fs_ops from the new
/// mount-root dentry's containing-payload — see `fs_ops_for`.
fn walk_inner_v3<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<DEntry>, NoProgress> {
    use StepOutcome as V3;

    let mount_root = mount_root_dentry(&rooted_at);

    let (mut current, mut remaining): (Cap<DEntry>, Vec<u8>) = if path.first() == Some(&b'/') {
        (mount_root.clone(), path[1..].to_vec())
    } else {
        (rooted_at.clone(), path.to_vec())
    };

    let must_be_directory = remaining.last().copied() == Some(b'/');

    let mut current_fs_ops: Option<Arc<dyn FsOps>> = fs_ops_for(&current, guard)
        .or_else(|| fs_ops_for(&mount_root, guard));
    let mut current_mount_payload: Option<Cap<MountPayload>> = mount_payload_for(&current, guard)
        .or_else(|| mount_payload_for(&mount_root, guard));

    let mut hop_count: u32 = 0;

    loop {
        // Collapse leading `///` runs.
        while remaining.first() == Some(&b'/') {
            remaining.remove(0);
        }

        if remaining.is_empty() {
            // End of input: enforce the trailing-`/` directory rule.
            if must_be_directory && current.rnode().meta().kind() != InodeKind::Directory {
                return V3::err(step_engine::Errno::ENOTDIR);
            }
            return V3::done(current);
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
            if let Some(parent_cap) = current.parent_hint() {
                if !is_same_dentry(&current, &mount_root) {
                    current = parent_cap;
                    current_fs_ops = fs_ops_for(&current, guard);
                    current_mount_payload = mount_payload_for(&current, guard);
                }
            }
            continue;
        }

        // Interior components require the current to be a directory.
        if current.rnode().meta().kind() != InodeKind::Directory {
            return V3::err(step_engine::Errno::ENOTDIR);
        }

        // POSIX search permission.
        let parent_meta = current.rnode().meta();
        if let Err(err) = check_descend_perm(&parent_meta, cred) {
            return V3::err(err.into());
        }

        let parent_fs_object_id = current.rnode().fs_object_id();
        let fs_ops = match &current_fs_ops {
            Some(ops) => ops.clone(),
            None => return V3::err(step_engine::Errno::ENODEV),
        };
        let child_fs_object_id = match fs_ops.lookup(parent_fs_object_id, &component, guard) {
            V3::Done(id) => id,
            V3::Continue { .. } => continue,
            V3::Yield { progress, shape } => return V3::Yield { progress, shape },
            V3::Err(err) => return V3::err(err),
        };

        let child_meta = match fs_ops.load_inode_meta(child_fs_object_id, guard) {
            V3::Done(m) => m,
            V3::Continue { .. } => continue,
            V3::Yield { progress, shape } => return V3::Yield { progress, shape },
            V3::Err(err) => return V3::err(err),
        };

        // Mid-path non-directory check.
        if child_meta.kind() != InodeKind::Directory
            && child_meta.kind() != InodeKind::Symlink
            && (!remaining.is_empty() || must_be_directory)
        {
            return V3::err(step_engine::Errno::ENOTDIR);
        }

        let child_rnode_cap =
            match materialise_child_rnode_v3(&fs_ops, child_fs_object_id, child_meta, current_mount_payload.as_ref(), guard) {
                V3::Done(rnode) => rnode,
                V3::Continue { .. } => continue,
                V3::Yield { progress, shape } => return V3::Yield { progress, shape },
                V3::Err(err) => return V3::err(err),
            };

        let child_inline = match InlineName::new(&component) {
            Ok(n) => n,
            Err(err) => return V3::err(err.into()),
        };
        let mut child_dentry_raw = DEntry::new(child_inline, child_rnode_cap.clone());
        child_dentry_raw.set_parent_hint(&current);
        let child_dentry = match step_engine::sign(child_dentry_raw) {
            Ok(cap) => cap,
            Err(_) => return V3::err(step_engine::Errno::ENOMEM),
        };

        // === Symlink chasing ============================================
        if let RNodeBacking::Symlink { target } = child_rnode_cap.backing() {
            hop_count += 1;
            if hop_count > SYMLOOP_MAX {
                return V3::err(step_engine::Errno::ELOOP);
            }
            let target_bytes = target.clone();
            if target_bytes.first() == Some(&b'/') {
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

        // === Mount-point crossing =====================================
        //
        // Two paths converge here:
        //
        // 1. The freshly-built `child_dentry`'s `mounted_hint` is set
        //    (set explicitly by callers that supply a pre-cached
        //    mount-point dentry — see `DEntry::set_mounted_hint`).
        // 2. The kernel's mount table (`mount::register_mount`) has an
        //    entry for `(parent_mount_payload, child_fs_object_id)`.
        //    This is the production path: init.rs's
        //    `mount_devfs_at_dev` registers the entry when devfs is
        //    published at `/dev`, and the walker looks it up here.
        //
        // The two paths produce the same downstream state: a fresh
        // DEntry over the mount's root rnode, with `current_fs_ops`
        // switched to the new mount's `payload.fs_ops`.
        let crossing_mount = child_dentry.mounted_hint().and_then(|w| w.upgrade(guard));
        let crossing_mount = match crossing_mount {
            Some(m) => Some(m),
            None => current_mount_payload
                .as_ref()
                .and_then(|payload| mount::mount_for(payload, child_fs_object_id)),
        };
        if let Some(mount_cap) = crossing_mount {
            current = match dentry_for_mount_root(&mount_cap, Some(&child_dentry)) {
                Ok(d) => d,
                Err(err) => return V3::err(err.into()),
            };
            // `fs_ops_for` reads `payload.fs_ops()` directly via
            // the new mount-root dentry's `containing_mount_weak`.
            current_fs_ops = fs_ops_for(&current, guard);
            current_mount_payload = mount_payload_for(&current, guard);
            continue;
        }

        // === Plain advance ==============================================
        current = child_dentry;
    }
}

/// Materialise an `RNode` for a freshly-resolved child inode using
/// `FsOps`.
///
/// - `Directory` → `RNodeBacking::Directory`.
/// - `Symlink` → call `FsOps::read_link` and wrap the bytes in
///   `RNodeBacking::Symlink { target }`.
/// - Other kinds (Regular / CharDevice / BlockDevice / Fifo / Socket)
///   delegate to the FS's `materialise_rnode` hook; default impl
///   returns `ENOSYS` for backends that don't yet support the kind.
fn materialise_child_rnode_v3<'g>(
    fs_ops: &Arc<dyn FsOps>,
    child_fs_object_id: FsObjectId,
    meta: InodeMeta,
    mount_payload: Option<&Cap<MountPayload>>,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<RNode>, NoProgress> {
    use StepOutcome as V3;

    match meta.kind() {
        InodeKind::Directory => {
            let result = if let Some(mp) = mount_payload {
                RNode::new_cap_in_mount(child_fs_object_id, meta, RNodeBacking::Directory, mp)
            } else {
                RNode::new_cap(child_fs_object_id, meta, RNodeBacking::Directory)
            };
            match result {
                Ok(rnode) => V3::done(rnode),
                Err(_) => V3::err(step_engine::Errno::ENOMEM),
            }
        }
        InodeKind::Symlink => {
            let target_bytes = match fs_ops.read_link(child_fs_object_id, guard) {
                V3::Done(b) => b,
                V3::Continue { .. } => {
                    // A well-behaved v3 backend never returns Continue
                    // for read_link (NoProgress identity). Defensively
                    // surface as ENOSYS — the caller will see the
                    // symlink is unresolvable.
                    return V3::err(step_engine::Errno::ENOSYS);
                }
                V3::Yield { progress, shape } => return V3::Yield { progress, shape },
                V3::Err(err) => return V3::err(err),
            };
            match RNode::new_cap(
                child_fs_object_id,
                meta,
                RNodeBacking::Symlink {
                    target: target_bytes,
                },
            ) {
                Ok(rnode) => V3::done(rnode),
                Err(_) => V3::err(step_engine::Errno::ENOMEM),
            }
        }
        // Regular / CharDevice / BlockDevice / Fifo / Socket — delegate
        // to the FS's v3 `materialise_rnode` hook.
        _ => fs_ops.materialise_rnode(child_fs_object_id, meta, guard),
    }
}

/// Resolve the `Arc<dyn FsOps>` in scope for a given dentry by
/// upgrading its RNode's containing-mount weak and reading
/// `MountPayload::fs_ops` directly. The field is populated at
/// mount-publication time.
fn fs_ops_for<'g>(dentry: &Cap<DEntry>, guard: &Guard<'g>) -> Option<Arc<dyn FsOps>> {
    let mount_payload_weak: Weak<MountPayload> = dentry.rnode().containing_mount_weak()?;
    let payload = mount_payload_weak.upgrade(guard)?;
    Some(payload.fs_ops().clone())
}

// === DAC predicates ===================================================

/// Pick the relevant POSIX mode-triplet bits for `cred` against the
/// inode's owner/group: owner (`>> 6`) > group (`>> 3`) > other.
/// Returns the bottom 3 bits — `(rwx)` for the chosen triplet.
fn select_perm_triplet(meta: &InodeMeta, cred: &Credential) -> u32 {
    let mode = meta.mode as u32;
    if cred.uid == meta.uid {
        (mode >> 6) & 0o7
    } else if cred.gid == meta.gid {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    }
}

/// DAC search/traversal check for an interior directory component.
/// Walker invokes this from [`walk_inner_v3`] before descending into a
/// resolved child directory's `lookup`. POSIX rule: the appropriate
/// triplet must have the `X` (execute = search) bit set, unless the
/// caller carries `CAP_DAC_OVERRIDE`.
///
/// Slice simplification: directories with at least one X bit also
/// satisfy `CAP_DAC_OVERRIDE`'s execute-bit constraint by definition,
/// so the override branch returns success unconditionally for
/// directories. Per `txdoc:VFS-CHECKS-PERMISSIONS-1`.
fn check_descend_perm(meta: &InodeMeta, cred: &Credential) -> Result<(), Errno> {
    if cred.effective_caps.contains(Capability::DAC_OVERRIDE) {
        return Ok(());
    }
    let bits = select_perm_triplet(meta, cred);
    if bits & 0o1 == 0 {
        return Err(Errno::EACCES);
    }
    Ok(())
}

/// DAC R/W check for terminal-component open. Validates
/// `OpenFileFlags::{read, write}` against the inode's owner/group
/// permission triplet. `CAP_DAC_OVERRIDE` short-circuits.
///
/// Slice simplification: full DAC override (real Linux's
/// `CAP_DAC_OVERRIDE` does not grant exec on regular files unless an
/// X bit is set, but `step_open` does not enforce exec — that
/// lives in `exec_script` in Wave 4). Per
/// `txdoc:VFS-CHECKS-PERMISSIONS-1`.
fn check_open_perm(meta: &InodeMeta, flags: OpenFileFlags, cred: &Credential) -> Result<(), Errno> {
    if cred.effective_caps.contains(Capability::DAC_OVERRIDE) {
        return Ok(());
    }
    let bits = select_perm_triplet(meta, cred);
    if flags.read && bits & 0o4 == 0 {
        return Err(Errno::EACCES);
    }
    if flags.write && bits & 0o2 == 0 {
        return Err(Errno::EACCES);
    }
    Ok(())
}

// === walker internals =================================================

/// Build a `Cap<DEntry>` over a mount's root RNode. Used when the
/// walker crosses a mount boundary or restarts from the namespace
/// root for an absolute symlink target.
fn dentry_for_mount_root(
    mount: &Cap<MountIdentity>,
    mount_point: Option<&Cap<DEntry>>,
) -> Result<Cap<DEntry>, Errno> {
    let mut raw = DEntry::new(InlineName::ROOT, mount.root().clone());
    if let Some(parent) = mount_point {
        raw.set_parent_hint(parent);
    }
    step_engine::sign(raw).map_err(|_| Errno::ENOMEM)
}

/// Walk `from`'s parent-hint chain to find the namespace's root
/// dentry. Returns `from` itself when no parent hint is installed.
fn mount_root_dentry(from: &Cap<DEntry>) -> Cap<DEntry> {
    let mut cursor: Cap<DEntry> = from.clone();
    while let Some(parent_cap) = cursor.parent_hint() {
        cursor = parent_cap;
    }
    cursor
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
