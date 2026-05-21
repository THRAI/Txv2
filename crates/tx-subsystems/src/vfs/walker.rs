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
//! Both are synchronous per STEP-2; yields surface through
//! `StepOutcome::Yield` rather than `.await`. Future cross-await
//! disciplines (range-lock waits inside `FsOps::lookup`, page-cache
//! materialisation inside `OpenFile::step_read` for regular files)
//! compose cleanly through the yield mechanism. The day-1
//! implementations call only synchronous `FsOps::lookup` /
//! `load_inode_meta` / `read_link` against in-memory backends.
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
//!   POSIX search (`X`) permission via [`super::predicates::check_descend_perm`]. A
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

use crate::vfs::adapter::step_engine::{self, Cap, NoProgress, StepOutcome, Weak};

use crate::execution::{Errno, Guard};
use crate::mount::{MountIdentity, MountPayload};
use crate::vfs::structure::{Credential, DEntry, InlineName, OpenFile, OpenFileFlags, RNode};
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
/// `step_walk` and [`step_open`] are **script-level resolvers**,
/// not `StepOp` implementations. PR-2 explicitly skipped wrapping them.
/// They obey the same external yield safety rules:
/// - no `epoch::Guard` across yield
/// - no witness across yield
/// - no reservation guard across yield
/// - resume revalidates path state
///
/// A future dedicated VFS PR may introduce a `PathResolveOp` state
/// machine; that work is out of scope for v3 foundation.
pub fn step_walk<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    cred: &Credential,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<DEntry>, NoProgress> {
    // Delegated to the resolution state-machine driver.
    // When the driver encounters a yield, it returns EAGAIN;
    // synchronous callers see the yield as an error.
    match crate::vfs::resolution::driver::walk_to_completion(
        rooted_at,
        path,
        crate::vfs::resolution::state::WalkMode::Entity,
        crate::vfs::resolution::state::FinalSymlinkPolicy::Follow,
        cred,
        guard,
    ) {
        Ok(resolved) => StepOutcome::done(resolved.dentry),
        Err(e) => StepOutcome::err(e.into()),
    }
}

/// Open a path by name. Composes [`step_walk`] with
/// `OpenFile::new_cap` and the DAC R/W check, returning a
/// `Cap<OpenFile>` over the resolved RNode.
pub fn step_open<'g>(
    rooted_at: Cap<DEntry>,
    path: &[u8],
    flags: OpenFileFlags,
    mode: u16,
    cred: &Credential,
    guard: &Guard<'g>,
) -> StepOutcome<Cap<OpenFile>, NoProgress> {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    // observe — credentials + flags validated via check_open_perm below
    // upgrade — dentry Cap resolved via step_walk (pass-through variant dispatch)
    // reserve — OpenFile::new_cap reserves zone slot
    // commit — N/A: delegated to OpenFile::new_cap internals
    // publish — N/A: no signal attachments
    use StepOutcome as V3;

    // `mode` is reserved for future create-on-open semantics; the
    // current surface only resolves existing entries.
    let _ = mode;

    let dentry = match step_walk(rooted_at, path, cred, guard) {
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
    // R/W permission bits. Routes through the cred::checks witness
    // surface rather than calling vfs::predicates directly — same
    // bit math, intact witness chain. The _w witness is dropped
    // because the publication site (OpenFile::new_cap_with_dentry
    // below) does not yet consume an OpenAuthorized<'g> token; when
    // it does, this is the mint point.
    let terminal_meta = dentry.rnode().meta();
    let _w = match crate::cred::checks::require_open_with_walker_cred(
        cred,
        &terminal_meta,
        flags,
        guard,
    ) {
        Ok(w) => w,
        Err(err) => return V3::err(err.into()),
    };

    let rnode = dentry.rnode().clone();
    match OpenFile::new_cap_with_dentry(rnode, flags, dentry) {
        Ok(open) => V3::done(open),
        Err(_) => V3::err(step_engine::Errno::EIO),
    }
}

/// Synchronous core of [`step_walk`], now delegated to the
/// resolution state machine (`kernel_step` + driver loop).
///
/// The old `walk_inner_v3` loop has been lifted into
/// `resolution::step::kernel_step` and `resolution::driver`.
/// This function remains as the synchronous shell; callers that
/// need yield/resume use `resolution::driver::run_walker` /
/// `resume_walker` directly.
/// Resolve the `Arc<dyn FsOps>` in scope for a given dentry by
/// upgrading its RNode's containing-mount weak and reading
/// `MountPayload::fs_ops` directly. The field is populated at
/// mount-publication time.
pub(crate) fn fs_ops_for<'g>(dentry: &Cap<DEntry>, guard: &Guard<'g>) -> Option<Arc<dyn FsOps>> {
    let rnode = dentry.rnode();
    let mount_payload_weak: Weak<MountPayload> = match rnode.containing_mount_weak() {
        Some(w) => w,
        None => {
            use crate::vfs::resolution::diagnostic;
            diagnostic::record_ctx(
                1, // fs_ops_for: no containing_mount_weak
                dentry.name().as_bytes(),
                rnode.fs_object_id(),
                b"", // fs_ops_for doesn't have path context
                false,
            );
            diagnostic::record_label(b"fs_ops_for: no containing_mount");
            return None;
        }
    };
    let payload = match mount_payload_weak.upgrade(guard) {
        Some(p) => p,
        None => {
            use crate::vfs::resolution::diagnostic;
            diagnostic::record_ctx(
                10, // fs_ops_for: weak upgrade failed
                dentry.name().as_bytes(),
                rnode.fs_object_id(),
                b"",
                true, // had containing_mount, just dead
            );
            diagnostic::record_label(b"fs_ops_for: weak upgrade dead");
            return None;
        }
    };
    let ops = payload.fs_ops().clone();
    Some(ops)
}

/// Like [`fs_ops_for`] but takes an `RNode` directly — used by
/// fd-based ops that don't have a `DEntry` in hand.
pub(crate) fn fs_ops_for_rnode<'g>(
    rnode: &Cap<RNode>,
    guard: &Guard<'g>,
) -> Option<Arc<dyn FsOps>> {
    let mount_payload_weak: Weak<MountPayload> = rnode.containing_mount_weak()?;
    let payload = mount_payload_weak.upgrade(guard)?;
    Some(payload.fs_ops().clone())
}

// === walker internals =================================================

/// Build a `Cap<DEntry>` over a mount's root RNode. Used when the
/// walker crosses a mount boundary or restarts from the namespace
/// root for an absolute symlink target.
pub(crate) fn dentry_for_mount_root(
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
pub(crate) fn mount_root_dentry(from: &Cap<DEntry>) -> Cap<DEntry> {
    let mut cursor: Cap<DEntry> = from.clone();
    while let Some(parent_cap) = cursor.parent_hint() {
        cursor = parent_cap;
    }
    cursor
}

/// Resolve the `Cap<MountPayload>` in scope for a given dentry by
/// upgrading its RNode's containing-mount weak. Used as the parent
/// key for `mount::mount_for` lookups during a walk.
pub(crate) fn mount_payload_for<'g>(
    dentry: &Cap<DEntry>,
    guard: &Guard<'g>,
) -> Option<Cap<MountPayload>> {
    let weak: Weak<MountPayload> = dentry.rnode().containing_mount_weak()?;
    weak.upgrade(guard)
}

/// Compare two dentries by name + RNode FsObjectId. Used by the
/// `..` ascend-only-up-to-mount-root rule. The comparison is
/// approximate (it does not check zone identity) but sufficient for
/// the boot-time chroot-shaped namespace.
pub(crate) fn is_same_dentry(a: &Cap<DEntry>, b: &Cap<DEntry>) -> bool {
    a.rnode().fs_object_id() == b.rnode().fs_object_id()
        && a.name().as_bytes() == b.name().as_bytes()
}

#[cfg(test)]
mod tests;
