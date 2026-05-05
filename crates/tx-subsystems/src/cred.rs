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
//! - Saved-set IDs (`suid`, `sgid`). Used by `setresuid`-family;
//!   inferable from current uid/euid via the day-1 simplified rule
//!   "non-privileged setuid only swaps among (uid, euid)".
//! - Capability bounding set, inheritable set, ambient set. Day-1
//!   models only `effective_caps` and `permitted_caps`.
//! - `step_setresuid`, `step_setreuid`, `step_capset`, `step_seteuid`,
//!   `step_setfsuid` — land when the syscall script driver consumes
//!   them.

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
    pub gid: Gid,
    pub egid: Gid,
    pub effective_caps: CapabilitySet,
    pub permitted_caps: CapabilitySet,
}

impl Cred {
    /// Root credentials with all capabilities granted.
    pub const fn root() -> Self {
        Self {
            uid: Uid::ROOT,
            euid: Uid::ROOT,
            gid: Gid::ROOT,
            egid: Gid::ROOT,
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
/// can set arbitrary `uid` and have all three of `uid`, `euid`,
/// `(saved)` change. Non-privileged callers may only swap among
/// `(uid, euid)`; any other target value returns
/// `PermissionDenied`.
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
    } else if new_uid == prev.uid || new_uid == prev.euid {
        // Non-privileged: allowed to swap effective among existing IDs.
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
/// [`step_setuid`].
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
    } else if new_gid == prev.gid || new_gid == prev.egid {
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

#[cfg(test)]
mod tests;
