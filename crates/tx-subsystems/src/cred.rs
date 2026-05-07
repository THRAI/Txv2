//! Credential service: per-process identity used by permission checks.
//!
//! Per `MODULE_MAP_v1` §11 and the `cred_service_v_1` draft, cred is a
//! *service subsystem*: it owns shared state on `ProcessPayload`
//! (`cred`) and offers helpers (`step_setuid`, `step_setgid`) that
//! mutate that state under the project's POSIX credential semantics.
//!
//! Day-1 surface:
//!
//! - Types: [`Uid`], [`Gid`], [`Capability`], [`CapabilitySet`], [`Cred`].
//! - Defaults: [`Cred::root`] for `bootstrap_init_process`; fork copies
//!   the parent's cred unchanged.
//! - Mutators: [`step_setuid`], [`step_setgid`].
//!
//! Deliberately deferred:
//!
//! - Supplementary group list. The full Cred carries a bounded
//!   `Vec<Gid>` per POSIX; day-1 elides it.
//! - `fsuid` / `fsgid`. Linux-specific filesystem-uid distinction; not
//!   required for the day-1 surface.
//! - Capability bounding set, inheritable set, ambient set. Day-1
//!   models only `effective_caps` and `permitted_caps`.
//! - `step_capset`, `step_seteuid`, `step_setfsuid` — land when the
//!   syscall script driver consumes them.
//!
//! Landed in the DAC + setuid slice (Wave 1):
//!
//! - Saved-set IDs (`suid`, `sgid`). Used by `setresuid`-family.
//! - `step_setresuid`, `step_setresgid`, `step_setreuid`,
//!   `step_setregid` helpers covering the full Linux privilege rule.

use core::marker::PhantomData;
use core::sync::atomic::Ordering;

use tx_substrate::epoch::Guard;
use tx_substrate::zone::Cap;

use crate::execution::Errno;
use crate::process::structure::{ProcessIdentity, TargetProcCred};
use crate::signal::Signum;

/// POSIX user identifier. `Uid::ROOT` (0) carries privilege.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Uid(pub u32);

impl Uid {
    pub const ROOT: Self = Self(0);

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn is_root(self) -> bool {
        self.0 == 0
    }
}

/// POSIX group identifier.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Gid(pub u32);

impl Gid {
    pub const ROOT: Self = Self(0);

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// POSIX capability bit (CAP_*). Day-1 carries the small subset that
/// matters for the existing surface; the rest land alongside their
/// callers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capability(u8);

impl Capability {
    /// `CAP_CHOWN` — change file ownership.
    pub const CHOWN: Self = Self(0);
    /// `CAP_DAC_OVERRIDE` — bypass discretionary access control.
    pub const DAC_OVERRIDE: Self = Self(1);
    /// `CAP_KILL` — send signals to processes with different uids.
    pub const KILL: Self = Self(5);
    /// `CAP_SETGID` — arbitrary `setgid` family ops.
    pub const SETGID: Self = Self(6);
    /// `CAP_SETUID` — arbitrary `setuid` family ops.
    pub const SETUID: Self = Self(7);
    /// `CAP_NET_ADMIN` — network administration.
    pub const NET_ADMIN: Self = Self(12);
    /// `CAP_SYS_ADMIN` — generic privileged operations.
    pub const SYS_ADMIN: Self = Self(21);

    pub const fn raw(self) -> u8 {
        self.0
    }

    pub const fn bit(self) -> u64 {
        1u64 << self.0
    }
}

/// Bitset of [`Capability`] bits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapabilitySet(u64);

impl CapabilitySet {
    pub const EMPTY: Self = Self(0);
    /// Every capability set; granted to root by default.
    pub const FULL: Self = Self(u64::MAX);

    pub const fn new(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn raw_bits(self) -> u64 {
        self.0
    }

    pub fn contains(self, cap: Capability) -> bool {
        (self.0 & cap.bit()) != 0
    }

    pub fn add(&mut self, cap: Capability) {
        self.0 |= cap.bit();
    }

    pub fn remove(&mut self, cap: Capability) {
        self.0 &= !cap.bit();
    }
}

/// Per-process credential snapshot.
///
/// Mutations go through [`step_setuid`] / [`step_setgid`] which enforce
/// POSIX privilege rules. Reads are atomic-snapshot: the field on
/// `ProcessPayload` is a `SpinMutex<Cred>` and `Cred` is `Copy`, so
/// readers clone a complete view under one lock acquisition.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Cred {
    pub uid: Uid,
    pub euid: Uid,
    /// Saved-set UID. Per Linux semantics, copied from `euid` at
    /// `fork`/`exec` and only updated by privileged callers via
    /// [`step_setuid`] (or explicitly via [`step_setresuid`] /
    /// [`step_setreuid`]).
    pub suid: Uid,
    pub gid: Gid,
    pub egid: Gid,
    /// Saved-set GID. Mirror of `suid` for the gid family.
    pub sgid: Gid,
    pub effective_caps: CapabilitySet,
    pub permitted_caps: CapabilitySet,
}

impl Cred {
    /// Root credentials with all capabilities granted.
    pub const fn root() -> Self {
        Self {
            uid: Uid::ROOT,
            euid: Uid::ROOT,
            suid: Uid::ROOT,
            gid: Gid::ROOT,
            egid: Gid::ROOT,
            sgid: Gid::ROOT,
            effective_caps: CapabilitySet::FULL,
            permitted_caps: CapabilitySet::FULL,
        }
    }

    /// `true` if the current effective uid is 0 *or* the requested
    /// capability is in the effective set. POSIX-style privilege test.
    pub fn is_privileged_for(self, cap: Capability) -> bool {
        self.euid.is_root() || self.effective_caps.contains(cap)
    }

    /// `true` if both processes share an effective uid. Used by the
    /// (future) signal-delivery permission check.
    pub fn shares_euid(self, other: Cred) -> bool {
        self.euid == other.euid
    }
}

/// Outcome of a credential mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredChange {
    Replaced { prev: Cred, new: Cred },
    Zombie,
    PermissionDenied,
}

/// `setuid`-family update. Privileged callers (root or `CAP_SETUID`)
/// can set arbitrary `uid` and have all four of `uid`, `euid`, `suid`
/// change to `new_uid`. Non-privileged callers may only swap `euid`
/// among `(uid, euid, suid)`; any other target value returns
/// `PermissionDenied`. Non-privileged calls preserve `suid` (Linux
/// semantics: only privileged callers update the saved-set).
pub fn step_setuid(target: &Cap<ProcessIdentity>, new_uid: Uid) -> CredChange {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return CredChange::Zombie;
    };
    let mut cred_guard = payload.cred.lock();
    let prev = *cred_guard;
    let mut new = prev;

    if prev.is_privileged_for(Capability::SETUID) {
        new.uid = new_uid;
        new.euid = new_uid;
        new.suid = new_uid;
    } else if new_uid == prev.uid || new_uid == prev.euid || new_uid == prev.suid {
        // Non-privileged: allowed to swap effective among existing IDs.
        // `suid` is preserved per Linux semantics.
        new.euid = new_uid;
    } else {
        return CredChange::PermissionDenied;
    }

    *cred_guard = new;
    drop(cred_guard);
    drop(payload_guard);

    // Force-publish so concurrent readers see the new cred immediately.
    core::sync::atomic::fence(Ordering::SeqCst);

    CredChange::Replaced { prev, new }
}

/// `setgid`-family update with the same privilege rules as
/// [`step_setuid`]. Privileged callers update `gid`, `egid`, and
/// `sgid` to `new_gid`. Non-privileged callers may swap `egid`
/// among `(gid, egid, sgid)`; `sgid` is preserved.
pub fn step_setgid(target: &Cap<ProcessIdentity>, new_gid: Gid) -> CredChange {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return CredChange::Zombie;
    };
    let mut cred_guard = payload.cred.lock();
    let prev = *cred_guard;
    let mut new = prev;

    if prev.is_privileged_for(Capability::SETGID) {
        new.gid = new_gid;
        new.egid = new_gid;
        new.sgid = new_gid;
    } else if new_gid == prev.gid || new_gid == prev.egid || new_gid == prev.sgid {
        new.egid = new_gid;
    } else {
        return CredChange::PermissionDenied;
    }

    *cred_guard = new;
    drop(cred_guard);
    drop(payload_guard);

    core::sync::atomic::fence(Ordering::SeqCst);

    CredChange::Replaced { prev, new }
}

/// `setresuid`-family update. `(ruid, euid, suid)` triple, each `None`
/// meaning "leave alone". Privileged callers (root or `CAP_SETUID`)
/// may set any combination. Non-privileged callers may only set each
/// non-`None` argument to a value that currently equals one of
/// `(uid, euid, suid)` — atomically: if any one of the three fails the
/// rule, no field changes and the call returns
/// `CredChange::PermissionDenied`.
pub fn step_setresuid(
    target: &Cap<ProcessIdentity>,
    ruid: Option<Uid>,
    euid: Option<Uid>,
    suid: Option<Uid>,
) -> CredChange {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return CredChange::Zombie;
    };
    let mut cred_guard = payload.cred.lock();
    let prev = *cred_guard;

    if !prev.is_privileged_for(Capability::SETUID) {
        let allowed = |candidate: Uid| -> bool {
            candidate == prev.uid || candidate == prev.euid || candidate == prev.suid
        };
        if let Some(r) = ruid {
            if !allowed(r) {
                return CredChange::PermissionDenied;
            }
        }
        if let Some(e) = euid {
            if !allowed(e) {
                return CredChange::PermissionDenied;
            }
        }
        if let Some(s) = suid {
            if !allowed(s) {
                return CredChange::PermissionDenied;
            }
        }
    }

    let mut new = prev;
    if let Some(r) = ruid {
        new.uid = r;
    }
    if let Some(e) = euid {
        new.euid = e;
    }
    if let Some(s) = suid {
        new.suid = s;
    }

    *cred_guard = new;
    drop(cred_guard);
    drop(payload_guard);
    core::sync::atomic::fence(Ordering::SeqCst);

    CredChange::Replaced { prev, new }
}

/// `setresgid`-family analog of [`step_setresuid`]. Same rule applied
/// to `(gid, egid, sgid)` with `CAP_SETGID`.
pub fn step_setresgid(
    target: &Cap<ProcessIdentity>,
    rgid: Option<Gid>,
    egid: Option<Gid>,
    sgid: Option<Gid>,
) -> CredChange {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return CredChange::Zombie;
    };
    let mut cred_guard = payload.cred.lock();
    let prev = *cred_guard;

    if !prev.is_privileged_for(Capability::SETGID) {
        let allowed = |candidate: Gid| -> bool {
            candidate == prev.gid || candidate == prev.egid || candidate == prev.sgid
        };
        if let Some(r) = rgid {
            if !allowed(r) {
                return CredChange::PermissionDenied;
            }
        }
        if let Some(e) = egid {
            if !allowed(e) {
                return CredChange::PermissionDenied;
            }
        }
        if let Some(s) = sgid {
            if !allowed(s) {
                return CredChange::PermissionDenied;
            }
        }
    }

    let mut new = prev;
    if let Some(r) = rgid {
        new.gid = r;
    }
    if let Some(e) = egid {
        new.egid = e;
    }
    if let Some(s) = sgid {
        new.sgid = s;
    }

    *cred_guard = new;
    drop(cred_guard);
    drop(payload_guard);
    core::sync::atomic::fence(Ordering::SeqCst);

    CredChange::Replaced { prev, new }
}

/// `setreuid`-family update. `(ruid, euid)` pair, each `None` meaning
/// "leave alone". Privileged callers may set any value. Non-privileged
/// callers must each (when `Some`) supply a value currently in
/// `{uid, euid, suid}`.
///
/// Linux quirk: when `ruid` is `Some` (i.e. real uid is being changed)
/// **or** the new `euid` differs from the old `prev.uid`, the
/// saved-set `suid` is updated to the post-call effective uid.
pub fn step_setreuid(
    target: &Cap<ProcessIdentity>,
    ruid: Option<Uid>,
    euid: Option<Uid>,
) -> CredChange {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return CredChange::Zombie;
    };
    let mut cred_guard = payload.cred.lock();
    let prev = *cred_guard;

    if !prev.is_privileged_for(Capability::SETUID) {
        let allowed = |candidate: Uid| -> bool {
            candidate == prev.uid || candidate == prev.euid || candidate == prev.suid
        };
        if let Some(r) = ruid {
            if !allowed(r) {
                return CredChange::PermissionDenied;
            }
        }
        if let Some(e) = euid {
            if !allowed(e) {
                return CredChange::PermissionDenied;
            }
        }
    }

    let mut new = prev;
    if let Some(r) = ruid {
        new.uid = r;
    }
    if let Some(e) = euid {
        new.euid = e;
    }

    // Linux quirk: if ruid was set OR the post-call euid differs from
    // the pre-call real uid, the saved-set is bumped to post-call euid.
    if ruid.is_some() || new.euid != prev.uid {
        new.suid = new.euid;
    }

    *cred_guard = new;
    drop(cred_guard);
    drop(payload_guard);
    core::sync::atomic::fence(Ordering::SeqCst);

    CredChange::Replaced { prev, new }
}

/// `setregid`-family analog of [`step_setreuid`]. Same rule applied to
/// `(gid, egid, sgid)` with `CAP_SETGID`.
pub fn step_setregid(
    target: &Cap<ProcessIdentity>,
    rgid: Option<Gid>,
    egid: Option<Gid>,
) -> CredChange {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return CredChange::Zombie;
    };
    let mut cred_guard = payload.cred.lock();
    let prev = *cred_guard;

    if !prev.is_privileged_for(Capability::SETGID) {
        let allowed = |candidate: Gid| -> bool {
            candidate == prev.gid || candidate == prev.egid || candidate == prev.sgid
        };
        if let Some(r) = rgid {
            if !allowed(r) {
                return CredChange::PermissionDenied;
            }
        }
        if let Some(e) = egid {
            if !allowed(e) {
                return CredChange::PermissionDenied;
            }
        }
    }

    let mut new = prev;
    if let Some(r) = rgid {
        new.gid = r;
    }
    if let Some(e) = egid {
        new.egid = e;
    }

    if rgid.is_some() || new.egid != prev.gid {
        new.sgid = new.egid;
    }

    *cred_guard = new;
    drop(cred_guard);
    drop(payload_guard);
    core::sync::atomic::fence(Ordering::SeqCst);

    CredChange::Replaced { prev, new }
}

// ----- Authorization checks -----

/// Zero-sized provenance witness produced by [`require_signal_send`].
///
/// Per `cred_service_v_1` §"Cred witnesses": cred witnesses carry no
/// retention or live references — they are guard-phantom-typed proof
/// that the authorization check ran successfully under `'g`.
#[must_use = "the witness is the authorization receipt — drop it explicitly only if you really intend to throw away the proof"]
pub struct SignalAuthorized<'g> {
    _guard: PhantomData<&'g ()>,
    _priv: (),
}

impl SignalAuthorized<'_> {
    fn new() -> Self {
        Self {
            _guard: PhantomData,
            _priv: (),
        }
    }
}

/// Pure permission rule: may a caller with `source` cred send `sig` to
/// a target whose facts are `target`? Implements the day-1 simplified
/// shape of `SIGNAL_v1` §32:
///
/// - `SIGCONT` to a target in the same session is always allowed
///   (POSIX SIGCONT bypass).
/// - Privileged callers (`CAP_KILL` or `euid == 0`) bypass the uid
///   check.
/// - Otherwise, at least one of `(source.uid, source.euid)` must match
///   one of `(target.uid, target.euid)`.
///
/// Day-1 simplification: Cred has no `suid`/`ruid` distinction yet, so
/// the Linux 4-way `(uid,euid) × (uid,suid,ruid)` match collapses to
/// `(uid,euid) × (uid,euid)`. Extends without reshaping callers when
/// saved-set IDs land.
pub fn signal_permitted(source: Cred, target: &TargetProcCred, sig: Signum) -> bool {
    if sig == Signum::SIGCONT && target.same_session {
        return true;
    }
    if source.is_privileged_for(Capability::KILL) {
        return true;
    }
    source.uid == target.uid
        || source.euid == target.euid
        || source.uid == target.euid
        || source.euid == target.uid
}

/// Authorization check for `kill` / `tkill` / `tgkill`. Returns a
/// `SignalAuthorized` witness on success, `Errno::EPERM` on denial.
/// Live-checked per `cred_service_v_1` §"Not every operation is
/// tokenized" — kill mints no reusable grant.
pub fn require_signal_send<'g>(
    source: Cred,
    target: &TargetProcCred,
    sig: Signum,
    guard: &'g Guard<'_>,
) -> Result<SignalAuthorized<'g>, Errno> {
    let _ = guard;
    if signal_permitted(source, target, sig) {
        Ok(SignalAuthorized::new())
    } else {
        Err(Errno::EPERM)
    }
}

/// Test-only: zero out `effective_caps` and `permitted_caps` on a
/// process. Used by tx-shims' DAC + setuid slice tests (Wave 2) to
/// take a `bootstrap_init_process`-minted process from "root with
/// `CapabilitySet::FULL`" to "fully unprivileged" without forging a
/// chain of `step_setresuid` calls (the shipping mutators preserve
/// caps).
///
/// Hidden behind `cfg(any(test, feature = "test-support"))` so it
/// never reaches release builds; re-exported through
/// `crate::cross_crate_test_support` for cross-crate consumers.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn clear_caps_for_test(target: &Cap<ProcessIdentity>) {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return;
    };
    let mut cred_guard = payload.cred.lock();
    cred_guard.effective_caps = CapabilitySet::EMPTY;
    cred_guard.permitted_caps = CapabilitySet::EMPTY;
}

#[cfg(test)]
mod tests;
