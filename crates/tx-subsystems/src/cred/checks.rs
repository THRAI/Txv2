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
