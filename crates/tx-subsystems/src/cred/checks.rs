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
use crate::vfs::structure::{Credential, InodeMeta, OpenFileFlags};

use super::adapter::step_engine::Guard;
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

// Re-export the existing signal-send check so the cred::checks::* surface
// is the single discovery point for authorization predicates.
pub use super::{require_signal_send, signal_permitted, SignalAuthorized};
