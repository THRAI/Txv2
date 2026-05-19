//! Cred-side authorization checks per `cred_service_v_1` §"Checks
//! surface".
//!
//! Each `require_*` predicate consumes the caller's syscall-entry
//! [`CredSnapshot`] (§"In flight") plus a subsystem-exported value
//! type (§"Foreign inputs consumed by cred" — e.g. `&InodeMeta`,
//! `&TargetProcCred`) and returns a guard-bound, zero-sized
//! [`Authorized`] witness on success.
//!
//! The witnesses follow the §"Cred witnesses" rule:
//! *zero-sized provenance tokens*. They carry no retention, no live
//! reference to the credential, no second copy of the checked metadata
//! — only the `'g` proof that the check ran under that guard for the
//! provided inputs.
//!
//! ## Relationship to `vfs::predicates`
//!
//! The bit-level DAC math lives in [`crate::vfs::predicates`]
//! (`check_descend_perm`, `check_open_perm`). This module is the
//! authorization seam: it adapts those pure predicates into the
//! `cred::checks::require_*(snapshot, foreign_input, &guard)` shape the
//! design canonicalises, while keeping the witness type owned by
//! cred (the authorization authority) rather than by VFS (the
//! publication subsystem). New call sites should prefer the
//! `require_*` API over reaching into `vfs::predicates` directly so the
//! witness chain is intact.
//!
//! Today's coverage: `require_path_search`, `require_open`,
//! `require_signal_send` (re-export of the existing surface for
//! discoverability). `require_unlink`, `require_setuid`, `require_chmod`
//! etc. land alongside the syscall arms that need them.

use core::marker::PhantomData;

use crate::execution::Errno;
use crate::process::structure::ProcessIdentity;
use crate::signal::Signum;
use crate::vfs::structure::{Credential, InodeMeta, OpenFileFlags};

use super::adapter::step_engine::{guard as fresh_guard, Cap, Guard};
use super::CredSnapshot;

// ----- Witness types (zero-sized, guard-bound) -----

/// Witness produced by [`require_path_search`].
///
/// Proves the caller may search (X-bit traverse) the directory whose
/// metadata was checked, under the bound guard.
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct SearchAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl SearchAuthorized<'_> {
    const fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

/// Witness produced by [`require_open`].
///
/// Proves the caller may open the inode whose metadata was checked
/// with the requested `flags`, under the bound guard.
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct OpenAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl OpenAuthorized<'_> {
    const fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

/// Witness produced by [`require_link`].
///
/// Proves the caller may create a new name in the parent directory
/// whose metadata was checked, under the bound guard.
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct LinkAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl LinkAuthorized<'_> {
    const fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

/// Witness produced by [`require_chmod`].
///
/// Proves the caller may change the mode of the inode whose metadata
/// was checked, under the bound guard.
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct ChmodAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl ChmodAuthorized<'_> {
    const fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

/// Witness produced by [`require_chown`].
///
/// Proves the caller may change the ownership of the inode whose
/// metadata was checked, to the requested uid/gid pair, under the
/// bound guard.
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct ChownAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl ChownAuthorized<'_> {
    const fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

/// Witness produced by [`require_rename`].
///
/// Proves the caller may rename the entry whose old-side parent +
/// child, and new-side parent metadata, were checked. Optionally the
/// witness also covers a displaced new-side child (when the rename
/// overwrites an existing entry).
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct RenameAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl RenameAuthorized<'_> {
    const fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

/// Witness produced by [`require_unlink`].
///
/// Proves the caller may remove the entry whose parent + child
/// metadata were checked, under the bound guard. The witness is
/// consumed at the `FsOps::unlink` / `FsOps::rmdir` commit site;
/// cred mints no reusable grant for unlink (§"Not every operation
/// is tokenized").
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct UnlinkAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl UnlinkAuthorized<'_> {
    const fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

// ----- require_* functions -----

/// May the caller carrying `source` traverse the directory described
/// by `meta`?
///
/// POSIX path-resolution rule (`man 2 path_resolution`): the appropriate
/// owner/group/other triplet must have the X (= search) bit set, unless
/// the caller carries `CAP_DAC_OVERRIDE`.
///
/// Returns [`SearchAuthorized`] on success, [`Errno::EACCES`] on denial.
/// Live-checked per `cred_service_v_1` §"Not every operation is
/// tokenized" — path search mints no reusable grant; the witness is
/// consumed at the call site.
pub fn require_path_search<'g>(
    source: &CredSnapshot,
    meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<SearchAuthorized<'g>, Errno> {
    let _ = guard;
    let projection = Credential::from(source);
    crate::vfs::predicates::check_descend_perm(meta, &projection)?;
    Ok(SearchAuthorized::new())
}

/// Walker-cred variant of [`require_path_search`].
///
/// Identical rule, but accepts the walker-side
/// [`Credential`](crate::vfs::structure::Credential) projection
/// directly rather than projecting from a [`CredSnapshot`]. Used by
/// [`crate::vfs::walker`] and other VFS-layer callers that already
/// own a `Credential` (the syscall arm's `walker_cred()`) and would
/// otherwise call `vfs::predicates::check_descend_perm` directly,
/// bypassing the witness chain.
///
/// All new tx-shims code paths should prefer the
/// `&CredSnapshot`-shaped [`require_path_search`]. The walker-cred
/// variant exists to keep the witness chain intact at FS-internal
/// mint sites without churning the walker's `&Credential` plumbing.
pub fn require_path_search_with_walker_cred<'g>(
    walker_cred: &Credential,
    meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<SearchAuthorized<'g>, Errno> {
    let _ = guard;
    crate::vfs::predicates::check_descend_perm(meta, walker_cred)?;
    Ok(SearchAuthorized::new())
}

/// May the caller carrying `source` open the inode described by
/// `meta` with the requested `flags`?
///
/// POSIX rule: each requested access mode (`read` / `write`) must have
/// its bit set in the appropriate triplet, unless the caller carries
/// `CAP_DAC_OVERRIDE`. `cred_service_v_1` §"One publication" —
/// publication of the resulting `OpenFile` / `FdEntry` carries the
/// minted grant; this witness justifies that publication.
///
/// Returns [`OpenAuthorized`] on success, [`Errno::EACCES`] on denial.
pub fn require_open<'g>(
    source: &CredSnapshot,
    meta: &InodeMeta,
    flags: OpenFileFlags,
    guard: &'g Guard<'_>,
) -> Result<OpenAuthorized<'g>, Errno> {
    let _ = guard;
    let projection = Credential::from(source);
    crate::vfs::predicates::check_open_perm(meta, flags, &projection)?;
    Ok(OpenAuthorized::new())
}

/// Walker-cred variant of [`require_open`]. See
/// [`require_path_search_with_walker_cred`] for the role rationale.
pub fn require_open_with_walker_cred<'g>(
    walker_cred: &Credential,
    meta: &InodeMeta,
    flags: OpenFileFlags,
    guard: &'g Guard<'_>,
) -> Result<OpenAuthorized<'g>, Errno> {
    let _ = guard;
    crate::vfs::predicates::check_open_perm(meta, flags, walker_cred)?;
    Ok(OpenAuthorized::new())
}

/// May the caller carrying `source` remove the entry whose parent
/// and child are described by the supplied metadata?
///
/// Composes the POSIX rule for `unlink(2)` / `unlinkat(2)` /
/// `rmdir(2)`:
///
/// - **Write + Search** on parent — `Errno::EACCES` on failure
///   (`CAP_DAC_OVERRIDE` bypasses).
/// - **Sticky bit** (`S_ISVTX`) on parent — caller must own the
///   child OR own the parent OR carry `CAP_FOWNER` (or be euid 0).
///   `Errno::EPERM` on failure (sticky doesn't yield to
///   `CAP_DAC_OVERRIDE`).
///
/// Returns [`UnlinkAuthorized`] on success — consumed at the
/// `FsOps::unlink` / `FsOps::rmdir` commit site.
pub fn require_unlink<'g>(
    source: &CredSnapshot,
    parent_meta: &InodeMeta,
    child_meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<UnlinkAuthorized<'g>, Errno> {
    let _ = guard;
    let projection = Credential::from(source);
    crate::vfs::predicates::check_unlink_perm(parent_meta, child_meta, &projection)?;
    Ok(UnlinkAuthorized::new())
}

/// May the caller carrying `source` create a new name in the parent
/// directory described by `new_parent_meta`?
///
/// POSIX `link(2)` / `linkat(2)` rule: write + search bits on the new
/// parent's appropriate triplet. `CAP_DAC_OVERRIDE` bypasses.
///
/// Sticky (`S_ISVTX`) is *not* consulted — sticky governs *removal*
/// (unlink/rmdir/rename's source side), not name creation.
///
/// Returns [`LinkAuthorized`] on success — consumed at the
/// `FsOps::link` commit site.
pub fn require_link<'g>(
    source: &CredSnapshot,
    new_parent_meta: &InodeMeta,
    guard: &'g Guard<'_>,
) -> Result<LinkAuthorized<'g>, Errno> {
    let _ = guard;
    let projection = Credential::from(source);
    crate::vfs::predicates::check_link_perm(new_parent_meta, &projection)?;
    Ok(LinkAuthorized::new())
}

/// May the caller carrying `source` rename `old_child` (under
/// `old_parent_meta`) to a new name under `new_parent_meta`,
/// optionally displacing `displaced_child`?
///
/// Composes the POSIX `rename(2)` rule, which is "unlink at the old
/// name + create at the new name + (if displacing) unlink at the new
/// name":
///
/// - **Old side**: full unlink rule on `(old_parent, old_child)`
///   — W+X on old parent (EACCES) and sticky-bit ownership rule
///   (EPERM).
/// - **New side**: create rule on `new_parent` — W+X (EACCES).
/// - **Displaced side** (if `displaced_child = Some(_)`): unlink
///   rule on `(new_parent, displaced_child)` — same EACCES / EPERM
///   discipline as the old side.
///
/// Returns [`RenameAuthorized`] on success — consumed at the
/// `FsOps::rename` commit site.
///
/// **v1 limitation:** call sites that cannot cheaply pre-lookup the
/// displaced inode metadata may pass `None` for `displaced_child`;
/// the displaced side's sticky-bit rule is then unenforced. This is
/// acceptable today because the only FS supporting rename
/// (`tmpfs::rename`) doesn't yet preserve POSIX collision semantics
/// for sticky-bit-protected destinations; closing the gap requires
/// pre-resolving the displaced inode at the syscall arm.
pub fn require_rename<'g>(
    source: &CredSnapshot,
    old_parent_meta: &InodeMeta,
    old_child_meta: &InodeMeta,
    new_parent_meta: &InodeMeta,
    displaced_child: Option<&InodeMeta>,
    guard: &'g Guard<'_>,
) -> Result<RenameAuthorized<'g>, Errno> {
    let _ = guard;
    let projection = Credential::from(source);
    crate::vfs::predicates::check_unlink_perm(old_parent_meta, old_child_meta, &projection)?;
    crate::vfs::predicates::check_link_perm(new_parent_meta, &projection)?;
    if let Some(displaced) = displaced_child {
        crate::vfs::predicates::check_unlink_perm(new_parent_meta, displaced, &projection)?;
    }
    Ok(RenameAuthorized::new())
}

/// May the caller carrying `source` change the mode bits of the
/// inode described by `target_meta`?
///
/// POSIX `chmod(2)` rule: owner (`cred.uid == meta.uid`) or
/// `CAP_FOWNER` (or `euid 0`). Mode bits themselves are
/// unrestricted in v1 — Linux's `CAP_FSETID` rule for setuid/setgid
/// bits is not yet modelled.
///
/// The `new_mode` argument is accepted for API symmetry with
/// `step_chmod` and for future privilege-on-mode-bit rules; v1
/// does not consult it.
///
/// Returns [`ChmodAuthorized`] on success — consumed at the
/// `FsOps::step_chmod` commit site.
pub fn require_chmod<'g>(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_mode: u16,
    guard: &'g Guard<'_>,
) -> Result<ChmodAuthorized<'g>, Errno> {
    let _ = guard;
    let _ = new_mode;
    let projection = Credential::from(source);
    crate::vfs::predicates::check_chmod_perm(target_meta, &projection)?;
    Ok(ChmodAuthorized::new())
}

/// May the caller carrying `source` change the owner / group of
/// the inode described by `target_meta` to `(new_uid, new_gid)`?
///
/// Rule: privileged callers (`CAP_FOWNER` or `euid 0`) may set any
/// uid/gid. Non-privileged callers may set the uid only to their
/// own (`cred.uid`) and the gid only to their primary
/// (`cred.gid`); `None` for either side means "leave unchanged".
///
/// Matches the rule the per-FS `step_chown` impls enforce today
/// (`tmpfs`, `devfs`, `bdevfs`, `procfs`); the cred-side seam is
/// not stricter — it is the canonical place for the rule to live.
///
/// Returns [`ChownAuthorized`] on success.
pub fn require_chown<'g>(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_uid: Option<u32>,
    new_gid: Option<u32>,
    guard: &'g Guard<'_>,
) -> Result<ChownAuthorized<'g>, Errno> {
    let _ = guard;
    let projection = Credential::from(source);
    crate::vfs::predicates::check_chown_perm(target_meta, new_uid, new_gid, &projection)?;
    Ok(ChownAuthorized::new())
}

// ----- authorize_* combinators for the FS family -----
//
// Mirror the shape of `authorize_signal_send` for every FS-mutator
// witness. Each combinator takes its own epoch guard, runs the
// underlying `require_*` predicate, drops the witness, and returns
// `Result<(), Errno>`.
//
// **Why the second layer.** The `require_*` predicates produce a
// guard-bound, `must_use` witness — that shape is correct for sites
// that thread the witness through a composite step (e.g. an op that
// takes `OpenAuthorized<'g>` as a token). The 7 syscall arms wired
// in tx-shims today all drop the witness immediately, so they were
// repeating 5+ lines of guard-scope-and-match boilerplate per site:
//
// ```text
// let guard = step_engine::guard();
// let _auth = match cred::checks::require_X(snapshot, ..., &guard) {
//     Ok(w) => w,
//     Err(e) => return SyscallResult::error_from(e),
// };
// ```
//
// The combinator collapses that to:
//
// ```text
// if let Err(e) = cred::checks::authorize_X(snapshot, ...) {
//     return SyscallResult::error_from(e);
// }
// ```
//
// The guard discipline (must drop before commit; cannot nest with
// downstream commit-side guards) becomes a property of the
// combinator rather than a hidden contract at every call site.
//
// New cred-checked syscall arms should prefer the `authorize_*`
// shape; sites that need the typed witness (e.g. to consume it
// inside a step body that gates on its presence) keep using
// `require_*` directly.

/// Authorise a path-traversal step against the directory described
/// by `meta`. Convenience wrapper over [`require_path_search`] —
/// takes + drops its own guard, discards the witness.
pub fn authorize_path_search(
    source: &CredSnapshot,
    meta: &InodeMeta,
) -> Result<(), Errno> {
    let guard = fresh_guard();
    require_path_search(source, meta, &guard).map(drop)
}

/// Authorise an `open(2)` against `meta` with the requested
/// `flags`. Convenience wrapper over [`require_open`].
pub fn authorize_open(
    source: &CredSnapshot,
    meta: &InodeMeta,
    flags: OpenFileFlags,
) -> Result<(), Errno> {
    let guard = fresh_guard();
    require_open(source, meta, flags, &guard).map(drop)
}

/// Authorise an `unlink(2)` / `rmdir(2)` against `(parent_meta,
/// child_meta)`. Convenience wrapper over [`require_unlink`].
pub fn authorize_unlink(
    source: &CredSnapshot,
    parent_meta: &InodeMeta,
    child_meta: &InodeMeta,
) -> Result<(), Errno> {
    let guard = fresh_guard();
    require_unlink(source, parent_meta, child_meta, &guard).map(drop)
}

/// Authorise creation of a new name in `new_parent_meta`. Convenience
/// wrapper over [`require_link`]. Use for `link(2)`, `mkdir(2)`,
/// `symlink(2)` — all share the W+X-on-new-parent rule.
pub fn authorize_link(
    source: &CredSnapshot,
    new_parent_meta: &InodeMeta,
) -> Result<(), Errno> {
    let guard = fresh_guard();
    require_link(source, new_parent_meta, &guard).map(drop)
}

/// Authorise a `rename(2)` from `(old_parent, old_child)` to
/// `new_parent`, optionally displacing `displaced_child`. Convenience
/// wrapper over [`require_rename`].
pub fn authorize_rename(
    source: &CredSnapshot,
    old_parent_meta: &InodeMeta,
    old_child_meta: &InodeMeta,
    new_parent_meta: &InodeMeta,
    displaced_child: Option<&InodeMeta>,
) -> Result<(), Errno> {
    let guard = fresh_guard();
    require_rename(
        source,
        old_parent_meta,
        old_child_meta,
        new_parent_meta,
        displaced_child,
        &guard,
    )
    .map(drop)
}

/// Authorise a `chmod(2)` against `target_meta`. Convenience wrapper
/// over [`require_chmod`].
pub fn authorize_chmod(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_mode: u16,
) -> Result<(), Errno> {
    let guard = fresh_guard();
    require_chmod(source, target_meta, new_mode, &guard).map(drop)
}

/// Authorise a `chown(2)` against `target_meta` with the requested
/// uid/gid change. Convenience wrapper over [`require_chown`].
pub fn authorize_chown(
    source: &CredSnapshot,
    target_meta: &InodeMeta,
    new_uid: Option<u32>,
    new_gid: Option<u32>,
) -> Result<(), Errno> {
    let guard = fresh_guard();
    require_chown(source, target_meta, new_uid, new_gid, &guard).map(drop)
}

// Re-export the existing signal-send check so the cred::checks::* surface
// is the single discovery point for authorization predicates.
pub use super::{require_signal_send, signal_permitted, SignalAuthorized};

// ----- authorize_* combinators -----
//
// The `require_*` predicates above are the witness-producing primitives
// (one input → one Authorized<'g>). The `authorize_*` combinators sit
// one level up: they package the recurring "capture snapshot →
// resolve target facts → require under a guard" sequence that every
// signal script repeated, and present a 3-state outcome that maps
// cleanly onto syscall-arm dispatch. Callers no longer manage:
//   • snapshot capture + ESRCH-on-zombie mapping
//   • foreign-value-type resolution from a target Cap
//   • the auth-phase epoch-guard scope (must drop before commit to
//     avoid nesting with downstream guards in `post_signal`,
//     `upgrade_owner_proc`, etc.)
//
// Two flavours per check: the bare form captures its own guard +
// snapshot (use it when you only do one check per syscall); the
// `_under_guard` form takes a caller-held snapshot + guard so a fanout
// loop (`script_kill_pgrp`) reuses one snapshot across N members
// instead of re-loading the `AtomicSlot<Cap<Cred>>` per iteration.

/// Outcome of an `authorize_*` combinator.
///
/// `Ok(Authorized)` — caller may proceed to commit.
/// `Ok(NoLiveTarget)` — target has no payload (zombie/dropped Weak);
///                      caller's commit branch should short-circuit
///                      to the no-delivery outcome (typically
///                      `NoLiveThread` for the signal scripts).
/// `Err(Errno::EPERM)` — cred check denied the operation.
/// `Err(Errno::ESRCH)` — source itself is a zombie (no snapshot).
///
/// 3-stating "denied" vs. "no live target" matters: POSIX kill returns
/// `EPERM` for the former and `ESRCH`/`0-delivered` for the latter,
/// and the scripts route them to different `SyscallResult` branches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use = "authorization outcomes must drive a commit-or-skip decision"]
pub enum AuthOutcome {
    /// Cred check passed — caller may commit the side-effect.
    Authorized,
    /// Target has no payload (zombie / dropped Weak). Skip commit;
    /// translate to the caller's "no live target" outcome.
    NoLiveTarget,
}

/// Authorise `source` to send `sig` to `target`.
///
/// Captures a fresh epoch guard for the cred check and drops it
/// before returning, so the caller's commit phase (which may take
/// its own guards via `post_signal` / `upgrade_owner_proc` / etc.)
/// never nests under the auth guard.
///
/// Folds the recurring snapshot + target-facts + `require_signal_send`
/// sequence into one call.
pub fn authorize_signal_send(
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
    sig: Signum,
) -> Result<AuthOutcome, Errno> {
    let guard = fresh_guard();
    let source_snapshot = source.cred_snapshot().ok_or(Errno::ESRCH)?;
    let Some(target_facts) = target.target_proc_cred_for(source) else {
        return Ok(AuthOutcome::NoLiveTarget);
    };
    let _w = require_signal_send(&source_snapshot, &target_facts, sig, &guard)?;
    Ok(AuthOutcome::Authorized)
}

/// Per-iteration variant of [`authorize_signal_send`] for fanout
/// callers (e.g. `script_kill_pgrp`) that:
/// - already captured the source's snapshot once outside the loop,
///   avoiding N `AtomicSlot::load`s, and
/// - already hold the iteration's `Guard<'g>` for
///   `pgrp.members.snapshot_live(guard)`.
///
/// Returns the same 3-state outcome as the bare form. The caller is
/// responsible for the lifetime of `guard`; this function takes no
/// new guard.
pub fn authorize_signal_send_under_guard(
    source_snapshot: &CredSnapshot,
    source: &Cap<ProcessIdentity>,
    target: &Cap<ProcessIdentity>,
    sig: Signum,
    guard: &Guard<'_>,
) -> Result<AuthOutcome, Errno> {
    let Some(target_facts) = target.target_proc_cred_for(source) else {
        return Ok(AuthOutcome::NoLiveTarget);
    };
    let _w = require_signal_send(source_snapshot, &target_facts, sig, guard)?;
    Ok(AuthOutcome::Authorized)
}
