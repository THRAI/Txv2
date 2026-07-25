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

pub mod adapter;
pub mod checks;

use adapter::step_engine::{
    self, Cap, CredentialView, Guard, NoProgress, OneShotStepOp, PayloadCap,
    RestrictionStackHandle, ScriptCtx, StepOp, StepOutcome, SubjectIdentity, Zone, ZoneAllocated,
    ZoneError,
};

use crate::execution::Errno;
use crate::process::structure::{
    ExecCredReservationToken, ProcessIdentity, ProcessPayload, TargetProcCred,
};
use crate::signal::Signum;
use crate::vfs::structure::{S_ISGID, S_ISUID};

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
    /// `CAP_DAC_READ_SEARCH` — bypass file read/search permission checks.
    pub const DAC_READ_SEARCH: Self = Self(2);
    /// `CAP_FOWNER` — bypass file-owner-only checks (chmod, chown,
    /// utimes, etc.) for files the caller does not own. Per Linux's
    /// POSIX cap-FOWNER number (3).
    pub const FOWNER: Self = Self(3);
    /// `CAP_KILL` — send signals to processes with different uids.
    pub const KILL: Self = Self(5);
    /// `CAP_SETGID` — arbitrary `setgid` family ops.
    pub const SETGID: Self = Self(6);
    /// `CAP_SETUID` — arbitrary `setuid` family ops.
    pub const SETUID: Self = Self(7);
    /// `CAP_NET_ADMIN` — network administration.
    pub const NET_ADMIN: Self = Self(12);
    /// `CAP_NET_RAW` — raw and packet sockets.
    pub const NET_RAW: Self = Self(13);
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
/// `ProcessPayload` is an `AtomicSlot<Cap<Cred>>` (PR-9 phase 5 / D5
/// Path A — was `SpinMutex<Cred>`). Readers load the cap, `Deref` to
/// `&Cred`, and the snapshot is independent of the slot once cloned
/// (`Cred: Copy`).
///
/// Implements [`adapter::step_engine::CredentialView`] per
/// [D1](../../../../docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md):
/// `step_v3` declares the trait shape; the concrete `Cred` lives
/// here in the subsystem layer. The trait body is intentionally
/// empty — step-level authority helpers consume the rich Cred
/// surface (uid/gid/effective caps) via inherent methods and the
/// `From<&Cred> for step_v3::Credential` bridge, not via the
/// abstract view.
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

impl CredentialView for Cred {}

// PR-9 phase 5 — D5 Path A. `Cred` is zone-allocated so that
// `ProcessPayload.cred` can hold `AtomicSlot<Cap<Cred>>` and mutators
// publish a fresh cap atomically per cred-mutation syscall. The COW
// model (one cap per setuid/setgid/...) replaces the old
// `SpinMutex<Cred>` lock-and-mutate-in-place pattern.
//
// Registration lives in `crate::zones::cred::register_zones()`. The
// zone is registered alongside the other subsystem zones; tests pick
// it up via `zones::register_all()`.
//
// `Cred` is a small POD struct (`Copy`) — moving it into a zone slot
// is a value-copy with no interior `Drop` to worry about. Senders/
// receivers cross threads via `Cap<Cred>` clones (atomic retain-count
// bumps on the cap key), not by value, so no `Send`/`Sync` markers
// are needed beyond what `Cred` already gets (its fields are all
// `Copy`).
static CRED_ZONE: Zone<Cred> = Zone::const_new();

unsafe impl ZoneAllocated for Cred {
    fn zone() -> &'static Zone<Self> {
        &CRED_ZONE
    }
}

/// Reserve a slot in the `Cred` zone and sign `cred` into it,
/// producing a fresh `Cap<Cred>`. Used by:
///
/// - `sign_process_payload` (process construction in
///   `bootstrap_init_process` and `step_fork`).
/// - The 7 cred-mutator wraps (`step_setuid`, ...) — each mutation
///   reserves a fresh cap, the slot's `AtomicSlot::swap` publishes
///   it, and the previous cap drops at the end of the call (EBR-
///   deferred reclamation).
/// - The 3 test helpers (`*_for_test`).
///
/// Returns `ZoneError` only on slab exhaustion; tests reset the slab
/// at `setup()`.
pub fn sign_cred(cred: Cred) -> Result<Cap<Cred>, ZoneError> {
    #[cfg(any(test, feature = "test-support"))]
    if FAIL_NEXT_CRED_SIGN.swap(false, core::sync::atomic::Ordering::AcqRel) {
        return Err(ZoneError::AllocationFailed);
    }
    step_engine::sign(cred)
}

#[cfg(any(test, feature = "test-support"))]
static FAIL_NEXT_CRED_SIGN: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Force the next credential zone/sign operation to report allocation failure.
///
/// Test-only seam for proving syscall-visible ENOMEM paths without exhausting
/// the shared host page allocator.
#[cfg(any(test, feature = "test-support"))]
pub fn fail_next_cred_sign_for_test() {
    FAIL_NEXT_CRED_SIGN.store(true, core::sync::atomic::Ordering::Release);
}

fn with_cred_mutation(
    target: &Cap<ProcessIdentity>,
    mutation: impl FnOnce(&ProcessPayload) -> CredChange,
) -> CredChange {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return CredChange::Zombie;
    };
    payload
        .with_unreserved_cred_mutation(|| mutation(payload))
        .unwrap_or(CredChange::Again)
}

/// Replace the current process capability masks.
///
/// The syscall layer performs Linux ABI validation for `capset(2)` before
/// reaching this helper; this primitive only publishes the already-checked
/// effective/permitted masks.
pub fn step_set_capability_sets(
    target: &Cap<ProcessIdentity>,
    effective_caps: CapabilitySet,
    permitted_caps: CapabilitySet,
) -> CredChange {
    with_cred_mutation(target, |payload| {
        let prev_cap = payload.cred_cap();
        let prev = *prev_cap;
        let mut new = prev;
        new.effective_caps = effective_caps;
        new.permitted_caps = permitted_caps;
        let Ok(new_cap) = sign_cred(new) else {
            return CredChange::Zombie;
        };
        let _old_cap = payload.replace_cred(new_cap);
        CredChange::Replaced { prev, new }
    })
}

/// PR-9 phase 5 — D5 §7. Mint a placeholder
/// `Cap<RestrictionStackHandle>` for `SubjectAuthority::new` calls in
/// the syscall arms.
///
/// The real append-only restriction stack lands in PR-K alongside the
/// seccomp / landlock surfaces; until then `SubjectAuthority` just
/// needs *some* cap to satisfy the type signature. The
/// `RestrictionStackHandle` placeholder is a unit-typed
/// `ZoneAllocated` struct in `adapter::step_engine`, so each call
/// reserves a fresh slot in the placeholder zone and signs the
/// unit-typed value into it. The resulting cap is short-lived (the
/// syscall arm drops it at script-frame exit; EBR retires the slab).
///
/// Cost is one zone reservation per syscall entry — acceptable for
/// the placeholder; PR-K replaces with the proper slot-style append-
/// only stack.
pub fn placeholder_restrictions_cap() -> Result<Cap<RestrictionStackHandle>, ZoneError> {
    step_engine::sign(RestrictionStackHandle::placeholder())
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

fn apply_uid_capability_transition(prev: Cred, new: &mut Cred) {
    let prev_had_root_uid = prev.uid.is_root() || prev.euid.is_root() || prev.suid.is_root();
    let new_has_root_uid = new.uid.is_root() || new.euid.is_root() || new.suid.is_root();

    if prev.euid.is_root() && !new.euid.is_root() {
        new.effective_caps = CapabilitySet::EMPTY;
    } else if !prev.euid.is_root() && new.euid.is_root() {
        new.effective_caps = new.permitted_caps;
    }

    if prev_had_root_uid && !new_has_root_uid {
        new.effective_caps = CapabilitySet::EMPTY;
        new.permitted_caps = CapabilitySet::EMPTY;
    }
}

/// Syscall-entry credential snapshot — the by-value metadata copy a
/// script carries from prelude through commit.
///
/// Per `cred_service_v_1` §"In flight": the canonical credential lives
/// in `ProcessPayload.cred` as `AtomicSlot<Cap<Cred>>`, and the script
/// holds only a by-value copy captured once at syscall entry. Authoring
/// the snapshot as its own type (rather than a bare `Cred`) gives the
/// architectural distinction a name and lets future fields (a
/// generation tag for racing-setuid detection, a NOSUID mount hint, a
/// `Cap<Cred>` retention handle if PR-K wants one) land without
/// touching every check signature.
///
/// `Copy` so it can sit in `SyscallCtx` and be passed by value to
/// authorization predicates. Construction goes through
/// [`CredSnapshot::from_cred`] (or [`CredSnapshot::root`]); the inner
/// `Cred` is read via [`CredSnapshot::cred`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[must_use = "the snapshot is the syscall-entry credential — discarding it forces a live re-read elsewhere"]
pub struct CredSnapshot {
    cred: Cred,
}

impl CredSnapshot {
    /// Wrap a `Cred` value as a snapshot. Captured once at syscall
    /// entry (`ProcessPayload::cred_snapshot`) or in test setup.
    pub const fn from_cred(cred: Cred) -> Self {
        Self { cred }
    }

    /// Root-credential snapshot. Used as the defensive fallback when a
    /// `SyscallCtx` is constructed against a zombie (impossible in
    /// practice from inside a live syscall arm) and by tests that need
    /// a known-root subject without touching a `ProcessPayload`.
    pub const fn root() -> Self {
        Self { cred: Cred::root() }
    }

    /// The captured `Cred` value. `Copy`; safe to hold across `.await`.
    pub const fn cred(self) -> Cred {
        self.cred
    }

    /// Borrowed view into the captured `Cred`. Useful when the caller
    /// already owns a `&CredSnapshot` and wants to feed a `&Cred`
    /// directly into a `cred::checks::require_*` signature.
    pub const fn as_cred(&self) -> &Cred {
        &self.cred
    }

    /// `true` if the snapshot's effective uid is 0 *or* the requested
    /// capability is in the effective set. Mirrors
    /// [`Cred::is_privileged_for`] so call sites that already hold a
    /// `CredSnapshot` don't have to unwrap to `Cred` for the common
    /// privilege test.
    pub fn is_privileged_for(self, cap: Capability) -> bool {
        self.cred.is_privileged_for(cap)
    }
}

impl From<Cred> for CredSnapshot {
    fn from(cred: Cred) -> Self {
        Self::from_cred(cred)
    }
}

impl AsRef<Cred> for CredSnapshot {
    fn as_ref(&self) -> &Cred {
        &self.cred
    }
}

/// Outcome of a credential mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredChange {
    Replaced { prev: Cred, new: Cred },
    Zombie,
    Again,
    PermissionDenied,
}

/// `setuid`-family update. Privileged callers (root or `CAP_SETUID`)
/// can set arbitrary `uid` and have all four of `uid`, `euid`, `suid`
/// change to `new_uid`. Non-privileged callers may only swap `euid`
/// among `(uid, euid, suid)`; any other target value returns
/// `PermissionDenied`. Non-privileged calls preserve `suid` (Linux
/// semantics: only privileged callers update the saved-set).
pub fn step_setuid(target: &Cap<ProcessIdentity>, new_uid: Uid) -> CredChange {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    with_cred_mutation(target, |payload| {
        // PR-9 phase 5 (D5 Path A): load the current cred cap, derive the
        // new value, sign a fresh cap, and publish via `AtomicSlot::swap`.
        let prev_cap = payload.cred_cap();
        let prev = *prev_cap;
        let mut new = prev;

        if prev.is_privileged_for(Capability::SETUID) {
            new.uid = new_uid;
            new.euid = new_uid;
            new.suid = new_uid;
        } else if new_uid == prev.uid || new_uid == prev.euid || new_uid == prev.suid {
            new.euid = new_uid;
        } else {
            return CredChange::PermissionDenied;
        }
        apply_uid_capability_transition(prev, &mut new);

        let new_cap = match sign_cred(new) {
            Ok(cap) => cap,
            Err(_) => return CredChange::PermissionDenied,
        };
        let _old_cap = payload.replace_cred(new_cap);

        CredChange::Replaced { prev, new }
    })
}

/// `setgid`-family update with the same privilege rules as
/// [`step_setuid`]. Privileged callers update `gid`, `egid`, and
/// `sgid` to `new_gid`. Non-privileged callers may swap `egid`
/// among `(gid, egid, sgid)`; `sgid` is preserved.
pub fn step_setgid(target: &Cap<ProcessIdentity>, new_gid: Gid) -> CredChange {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    with_cred_mutation(target, |payload| {
        let prev_cap = payload.cred_cap();
        let prev = *prev_cap;
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

        let new_cap = match sign_cred(new) {
            Ok(cap) => cap,
            Err(_) => return CredChange::PermissionDenied,
        };
        let _old_cap = payload.replace_cred(new_cap);

        CredChange::Replaced { prev, new }
    })
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    with_cred_mutation(target, |payload| {
        let prev_cap = payload.cred_cap();
        let prev = *prev_cap;

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
        apply_uid_capability_transition(prev, &mut new);

        let new_cap = match sign_cred(new) {
            Ok(cap) => cap,
            Err(_) => return CredChange::PermissionDenied,
        };
        let _old_cap = payload.replace_cred(new_cap);

        CredChange::Replaced { prev, new }
    })
}

/// `setresgid`-family analog of [`step_setresuid`]. Same rule applied
/// to `(gid, egid, sgid)` with `CAP_SETGID`.
pub fn step_setresgid(
    target: &Cap<ProcessIdentity>,
    rgid: Option<Gid>,
    egid: Option<Gid>,
    sgid: Option<Gid>,
) -> CredChange {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    with_cred_mutation(target, |payload| {
        let prev_cap = payload.cred_cap();
        let prev = *prev_cap;

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

        let new_cap = match sign_cred(new) {
            Ok(cap) => cap,
            Err(_) => return CredChange::PermissionDenied,
        };
        let _old_cap = payload.replace_cred(new_cap);

        CredChange::Replaced { prev, new }
    })
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
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    with_cred_mutation(target, |payload| {
        let prev_cap = payload.cred_cap();
        let prev = *prev_cap;

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
        apply_uid_capability_transition(prev, &mut new);

        let new_cap = match sign_cred(new) {
            Ok(cap) => cap,
            Err(_) => return CredChange::PermissionDenied,
        };
        let _old_cap = payload.replace_cred(new_cap);

        CredChange::Replaced { prev, new }
    })
}

/// `setregid`-family analog of [`step_setreuid`]. Same rule applied to
/// `(gid, egid, sgid)` with `CAP_SETGID`.
pub fn step_setregid(
    target: &Cap<ProcessIdentity>,
    rgid: Option<Gid>,
    egid: Option<Gid>,
) -> CredChange {
    // observe
    // upgrade
    // reserve
    // commit
    // publish
    with_cred_mutation(target, |payload| {
        let prev_cap = payload.cred_cap();
        let prev = *prev_cap;

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

        let new_cap = match sign_cred(new) {
            Ok(cap) => cap,
            Err(_) => return CredChange::PermissionDenied,
        };
        let _old_cap = payload.replace_cred(new_cap);

        CredChange::Replaced { prev, new }
    })
}

// ----- Exec-time setuid/setgid recompute -----

/// Outcome of [`step_apply_suid_for_exec`].
///
/// Surfaces facts from a completed credential commit. Pre-PoNR callers use
/// [`PreparedExecCred`] instead and drop it on failure without publishing.
///
/// Cites: `txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`
/// and `txdoc:EXEC-12-3-INSTALL-NEW-CREDENTIAL`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecCredOutcome {
    /// `true` iff the binary's `S_ISUID` bit triggered an effective-uid
    /// change, **or** the binary's `S_ISGID` bit (in the presence of a
    /// group-X bit) triggered an effective-gid change. Maps to
    /// `AT_SECURE = 1` in the auxv table when `true`. Per Q5
    /// (DECIDED 2026-05-06): short-form rule — the slice does not yet
    /// model file capabilities or `nosuid` mounts, so the effective-id
    /// delta is the only signal.
    pub at_secure: bool,
    /// Snapshot of the cred before the committed replacement. This is an
    /// audit/result fact; pre-PoNR rollback is structural because the prepared
    /// replacement is not installed until commit.
    pub previous_cred: Cred,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecSetidPolicy {
    Apply,
    Suppress,
}

/// Linear exec reservation over one process payload's credential lane.
pub struct ExecCredReservation {
    payload: PayloadCap<ProcessPayload>,
    token: ExecCredReservationToken,
    active: bool,
}

impl ExecCredReservation {
    fn commit(mut self, replacement: Option<Cap<Cred>>) -> Result<Cap<Cred>, ()> {
        let previous = self
            .payload
            .commit_exec_cred_reservation(self.token, replacement);
        if previous.is_ok() {
            self.active = false;
        }
        previous
    }
}

impl Drop for ExecCredReservation {
    fn drop(&mut self) {
        if self.active {
            self.payload.release_exec_cred_reservation(self.token);
        }
    }
}

/// Credential replacement and mutation reservation fully prepared before
/// exec's point of no return.
pub struct PreparedExecCred {
    process_prep: crate::process::ProcessExecPrep,
    reservation: ExecCredReservation,
    new_cred: Option<Cap<Cred>>,
    credential_snapshot: Cred,
    at_secure: bool,
}

impl PreparedExecCred {
    pub const fn credential_snapshot(&self) -> Cred {
        self.credential_snapshot
    }

    pub fn credential(&self) -> &Cred {
        &self.credential_snapshot
    }

    pub const fn at_secure(&self) -> bool {
        self.at_secure
    }

    pub const fn has_replacement(&self) -> bool {
        self.new_cred.is_some()
    }

    pub fn process_prep_mut(&mut self) -> &mut crate::process::ProcessExecPrep {
        &mut self.process_prep
    }
}

/// Reserve the credential lane and prepare the optional setid replacement.
pub fn prepare_exec_cred(
    target: &Cap<ProcessIdentity>,
    file_uid: Uid,
    file_gid: Gid,
    file_mode: u16,
    setid_policy: ExecSetidPolicy,
) -> Result<PreparedExecCred, Errno> {
    let initiator = target
        .payload
        .lock()
        .as_ref()
        .and_then(|payload| payload.threads.snapshot().into_iter().next())
        .ok_or(Errno::ESRCH)?;
    let process_prep = crate::process::ProcessExecPrep::begin(target, &initiator).map_err(
        |error| match error {
            crate::process::ExecPrepError::Again => Errno::EAGAIN,
            crate::process::ExecPrepError::OutOfMemory => Errno::ENOMEM,
            crate::process::ExecPrepError::Zombie | crate::process::ExecPrepError::StaleBinding => {
                Errno::ESRCH
            }
        },
    )?;
    prepare_exec_cred_in(process_prep, file_uid, file_gid, file_mode, setid_policy)
}

/// Prepare the credential child reservation under an already-owned process
/// exec lifecycle episode. Production exec passes its actual calling thread
/// to `ProcessExecPrep::begin` before reaching this function.
pub fn prepare_exec_cred_in(
    process_prep: crate::process::ProcessExecPrep,
    file_uid: Uid,
    file_gid: Gid,
    file_mode: u16,
    setid_policy: ExecSetidPolicy,
) -> Result<PreparedExecCred, Errno> {
    process_prep.validate_binding().map_err(|_| Errno::EAGAIN)?;
    let payload = process_prep.payload().clone();
    let (token, prev) = payload.reserve_exec_cred().ok_or(Errno::EAGAIN)?;
    let reservation = ExecCredReservation {
        payload,
        token,
        active: true,
    };
    let mut new = prev;

    let apply_setid = setid_policy == ExecSetidPolicy::Apply;
    let setuid = apply_setid && (file_mode & S_ISUID) != 0;
    let setgid = apply_setid && (file_mode & S_ISGID) != 0 && (file_mode & 0o010) != 0;

    if setuid {
        new.euid = file_uid;
        new.suid = new.euid;
    }
    if setgid {
        new.egid = file_gid;
        new.sgid = new.egid;
    }

    let at_secure = (setuid && new.euid != prev.euid) || (setgid && new.egid != prev.egid);
    let new_cred = match setid_policy {
        ExecSetidPolicy::Apply => Some(sign_cred(new).map_err(|_| Errno::ENOMEM)?),
        ExecSetidPolicy::Suppress => None,
    };

    Ok(PreparedExecCred {
        process_prep,
        reservation,
        new_cred,
        credential_snapshot: new,
        at_secure,
    })
}

pub fn prepare_setid_for_exec(
    target: &Cap<ProcessIdentity>,
    file_uid: Uid,
    file_gid: Gid,
    file_mode: u16,
) -> Result<PreparedExecCred, Errno> {
    prepare_exec_cred(
        target,
        file_uid,
        file_gid,
        file_mode,
        ExecSetidPolicy::Apply,
    )
}

/// Publish a credential whose allocation and computation completed pre-PoNR.
///
/// Exec calls this only after replacing the address space. The authoritative
/// identity binding is checked before the credential token is consumed; a
/// stale binding returns without mutating the detached payload.
pub fn commit_prepared_exec_cred(
    prepared: PreparedExecCred,
) -> Result<ExecCredOutcome, crate::process::ExecPrepError> {
    let PreparedExecCred {
        process_prep,
        reservation,
        new_cred,
        credential_snapshot: _,
        at_secure,
    } = prepared;
    let previous_cred = *process_prep.commit_authoritative(move |authoritative_payload| {
        if authoritative_payload.key() != reservation.payload.key() {
            return Err(crate::process::ExecPrepError::StaleBinding);
        }
        reservation
            .commit(new_cred)
            .map_err(|_| crate::process::ExecPrepError::StaleBinding)
    })?;

    Ok(ExecCredOutcome {
        at_secure,
        previous_cred,
    })
}

/// Immediately apply the binary's `S_ISUID` / `S_ISGID` mode bits to `target`'s
/// effective and saved-set IDs at exec time. Per Linux semantics:
///
/// - If `file_mode & S_ISUID` is set: `cred.euid := file_uid` and
///   `cred.suid := cred.euid` (post-recompute saved-set tracks the new
///   effective uid).
/// - If `file_mode & S_ISGID` is set **AND** the binary has a group-X
///   bit (`S_IXGRP = 0o010`) set: `cred.egid := file_gid` and
///   `cred.sgid := cred.egid`. Linux's quirk: `S_ISGID` without
///   group-X means mandatory locking, not setgid; the slice honours
///   only the standard meaning.
/// - The real `cred.uid` / `cred.gid` are NEVER changed at exec time
///   (Linux preserves them so `getuid()` returns the calling user's
///   real id even when running a setuid binary).
/// - `cred.effective_caps` / `cred.permitted_caps` are NOT modified —
///   the slice does not yet model file capabilities (`xattr`-driven
///   `CAP_FILE_CAP_*` discipline lands in a future capabilities slice).
///
/// Compatibility surface for non-transactional callers. It performs
/// [`prepare_setid_for_exec`] and [`commit_prepared_exec_cred`] back to back,
/// returning `None` for a zombie or allocation failure. The exec script must
/// use those two operations separately so all fallible work stays pre-PoNR.
///
/// Cites: `txdoc:EXEC-3-3-CRED-AUTHORIZES-EXECUTE-COMPUTES-NEW-CREDENTIALS`,
/// `txdoc:EXEC-12-3-INSTALL-NEW-CREDENTIAL`,
/// `txdoc:EXEC-12-PHASE-7-INFALLIBLE-POST-SWAP-COMMITS`.
pub fn step_apply_suid_for_exec(
    target: &Cap<ProcessIdentity>,
    file_uid: Uid,
    file_gid: Gid,
    file_mode: u16,
) -> Option<ExecCredOutcome> {
    let prepared = prepare_setid_for_exec(target, file_uid, file_gid, file_mode).ok()?;
    Some(
        commit_prepared_exec_cred(prepared)
            .expect("fresh exec credential reservation remains authoritative"),
    )
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

/// Pure permission rule: may a caller carrying `source` send `sig` to
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
///
/// Takes `&CredSnapshot` rather than `Cred` by value so the syscall-
/// entry snapshot threads through the cred → signal check boundary
/// per `cred_service_v_1` §"In flight". Callers that hold the raw
/// `Cred` can wrap via `CredSnapshot::from_cred(cred)`.
pub fn signal_permitted(source: &CredSnapshot, target: &TargetProcCred, sig: Signum) -> bool {
    let source = source.as_cred();
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
///
/// `source` is the caller's syscall-entry [`CredSnapshot`]; the check
/// runs against that snapshot, not a fresh load of the canonical cred
/// — matching the "scripts hold a by-value metadata copy" rule in
/// `cred_service_v_1` §"In flight".
pub fn require_signal_send<'g>(
    source: &CredSnapshot,
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
    let prev_cap = payload.cred_cap();
    let mut new = *prev_cap;
    new.effective_caps = CapabilitySet::EMPTY;
    new.permitted_caps = CapabilitySet::EMPTY;
    let new_cap = sign_cred(new).expect("zone slab has capacity in tests");
    let _old_cap = payload.replace_cred(new_cap);
}

/// Test-only: install the given `caps` as both `effective_caps` and
/// `permitted_caps` on a process. Used by tx-shims' DAC + setuid
/// slice tests (Wave 4) to set up a non-root caller carrying
/// **just** `CAP_DAC_OVERRIDE` (or some other narrow cap) without
/// going through a file-cap exec or `prctl` flow that the slice
/// doesn't ship.
///
/// Hidden behind `cfg(any(test, feature = "test-support"))` so it
/// never reaches release builds; re-exported through
/// `crate::cross_crate_test_support` for cross-crate consumers.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn install_caps_for_test(target: &Cap<ProcessIdentity>, caps: CapabilitySet) {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return;
    };
    let prev_cap = payload.cred_cap();
    let mut new = *prev_cap;
    new.effective_caps = caps;
    new.permitted_caps = caps;
    let new_cap = sign_cred(new).expect("zone slab has capacity in tests");
    let _old_cap = payload.replace_cred(new_cap);
}

/// Test-only: overwrite the (uid, gid, euid, egid, suid, sgid)
/// fields on a process's credential. Used by tx-shims' DAC + setuid
/// slice tests (Wave 4) to set up the AT_EACCESS test where the
/// caller's **real** uid differs from its **effective** uid (a state
/// the shipping `step_set*` family cannot assemble in one shot
/// without an unrelated chain of calls).
///
/// Hidden behind `cfg(any(test, feature = "test-support"))` so it
/// never reaches release builds; re-exported through
/// `crate::cross_crate_test_support` for cross-crate consumers.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn set_cred_ids_for_test(
    target: &Cap<ProcessIdentity>,
    uid: u32,
    euid: u32,
    suid: u32,
    gid: u32,
    egid: u32,
    sgid: u32,
) {
    let payload_guard = target.payload.lock();
    let Some(payload) = payload_guard.as_ref() else {
        return;
    };
    let prev_cap = payload.cred_cap();
    let mut new = *prev_cap;
    new.uid = Uid(uid);
    new.euid = Uid(euid);
    new.suid = Uid(suid);
    new.gid = Gid(gid);
    new.egid = Gid(egid);
    new.sgid = Gid(sgid);
    let new_cap = sign_cred(new).expect("zone slab has capacity in tests");
    let _old_cap = payload.replace_cred(new_cap);
}

// -- PR-2 StepOp wraps -------------------------------------------------
//
// Per `docs/Txv3/03_STEP_MODEL_v2.md` §2.1, PR-2 wraps each free
// `step_*` fn in an `impl StepOp for FooOp` shell. The cred mutators
// take no `Guard` argument (they swap the per-payload
// `AtomicSlot<Cap<Cred>>` internally — PR-9 phase 5 / D5 Path A,
// previously a `SpinMutex<Cred>`), so the wraps need no lifetime
// parameter — `Cap` is `Clone` and stored by value. The `step()`
// body delegates to the free fn unchanged and lifts the `CredChange`
// return into `StepOutcome::Done`.
//
// PR-2 pilot wraps (`SetuidOp`, `SetgidOp`, `SetreuidOp`) validated the
// pattern across the simplest scalar-arg shape and an `Option<_>`-pair
// shape. PR-2 cleanup extends coverage to the remaining cred mutators:
// `SetresuidOp`, `SetresgidOp`, `SetregidOp`, and `ApplySuidForExecOp`.
// `step_apply_suid_for_exec` returns `Option<ExecCredOutcome>` (no
// `Result`, no `StepOutcome`); the `Option` is lifted unchanged into
// `StepOutcome::Done(_)`.

/// StepOp wrap for [`step_setuid`]. PR-2 pilot.
pub struct SetuidOp {
    pub target: Cap<ProcessIdentity>,
    pub new_uid: Uid,
}

impl<I: SubjectIdentity> StepOp<I> for SetuidOp {
    type Output = CredChange;
    type Progress = NoProgress;

    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_setuid(&self.target, self.new_uid))
    }
}

// PR-3: OneShotStepOp marker — setuid is a one-shot transition
// (observe → commit → publish, never yields).
// Use the concrete ProcessIdentity type matching KernelScriptCtx
// (tx_subsystems::process::ProcessIdentity).
impl OneShotStepOp<crate::process::ProcessIdentity> for SetuidOp {}

/// StepOp wrap for [`step_setgid`]. PR-2 pilot.
pub struct SetgidOp {
    pub target: Cap<ProcessIdentity>,
    pub new_gid: Gid,
}

impl<I: SubjectIdentity> StepOp<I> for SetgidOp {
    type Output = CredChange;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_setgid(&self.target, self.new_gid))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SetgidOp {}

/// StepOp wrap for [`step_setreuid`]. PR-2 pilot. Demonstrates the
/// `Option<_>`-pair arg shape; the wrap stores each option by value
/// (`Uid: Copy`).
pub struct SetreuidOp {
    pub target: Cap<ProcessIdentity>,
    pub ruid: Option<Uid>,
    pub euid: Option<Uid>,
}

impl<I: SubjectIdentity> StepOp<I> for SetreuidOp {
    type Output = CredChange;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_setreuid(&self.target, self.ruid, self.euid))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SetreuidOp {}

/// StepOp wrap for [`step_setresuid`].
pub struct SetresuidOp {
    pub target: Cap<ProcessIdentity>,
    pub ruid: Option<Uid>,
    pub euid: Option<Uid>,
    pub suid: Option<Uid>,
}

impl<I: SubjectIdentity> StepOp<I> for SetresuidOp {
    type Output = CredChange;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_setresuid(
            &self.target,
            self.ruid,
            self.euid,
            self.suid,
        ))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SetresuidOp {}

/// StepOp wrap for [`step_setresgid`].
pub struct SetresgidOp {
    pub target: Cap<ProcessIdentity>,
    pub rgid: Option<Gid>,
    pub egid: Option<Gid>,
    pub sgid: Option<Gid>,
}

impl<I: SubjectIdentity> StepOp<I> for SetresgidOp {
    type Output = CredChange;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_setresgid(
            &self.target,
            self.rgid,
            self.egid,
            self.sgid,
        ))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SetresgidOp {}

/// StepOp wrap for [`step_setregid`].
pub struct SetregidOp {
    pub target: Cap<ProcessIdentity>,
    pub rgid: Option<Gid>,
    pub egid: Option<Gid>,
}

impl<I: SubjectIdentity> StepOp<I> for SetregidOp {
    type Output = CredChange;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_setregid(&self.target, self.rgid, self.egid))
    }
}

impl OneShotStepOp<crate::process::ProcessIdentity> for SetregidOp {}

/// StepOp wrap for [`step_apply_suid_for_exec`]. The free fn returns
/// `Option<ExecCredOutcome>` (no `Result`, no `StepOutcome`), so the
/// `Option` is lifted unchanged into `StepOutcome::Done(_)`.
pub struct ApplySuidForExecOp {
    pub target: Cap<ProcessIdentity>,
    pub file_uid: Uid,
    pub file_gid: Gid,
    pub file_mode: u16,
}

impl<I: SubjectIdentity> StepOp<I> for ApplySuidForExecOp {
    type Output = Option<ExecCredOutcome>;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<I>) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(step_apply_suid_for_exec(
            &self.target,
            self.file_uid,
            self.file_gid,
            self.file_mode,
        ))
    }
}

#[cfg(test)]
mod step_op_wraps {
    //! PR-2 StepOp wrap pilot tests. Each test exercises one wrap
    //! against a `bootstrap_init_process`-minted cap (root cred by
    //! default), confirming the wrap delegates to the free fn and
    //! that the result lifts into `StepOutcome::Done`. Coverage of
    //! the privilege/permission rules themselves lives in the
    //! existing `cred::tests` module against the free fns.
    use super::*;
    use crate::cred::adapter::step_engine::{
        PlaceholderProcessSubject, ScriptCtx, StepOp, StepOutcome,
    };
    use crate::process::bootstrap_init_process;
    use crate::test_support::EPOCH_TEST_LOCK;
    use crate::vm::{AddressSpace, TestPmap};
    use crate::zones;
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        tx_test_support::init_host();
        let _ = zones::register_all();
        tx_test_support::drain_to_quiescence();
        crate::process::structure::reset_pid_counter_for_test();
        crate::thread_runtime::structure::reset_tid_counter_for_test();
        crate::process::execution::reset_init_process_for_test();
        guard
    }

    fn fresh_aspace() -> Cap<crate::vm::AddressSpace> {
        AddressSpace::new_cap_for_platform::<TestPmap>().expect("fresh aspace")
    }

    #[test]
    fn setuid_op_delegates_to_step_setuid() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SetuidOp {
            target: proc_cap.clone(),
            new_uid: Uid(1000),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(CredChange::Replaced { new, .. }) => {
                assert_eq!(new.uid, Uid(1000));
                assert_eq!(new.euid, Uid(1000));
                assert_eq!(new.suid, Uid(1000));
            }
            other => panic!("expected Done(Replaced), got {other:?}"),
        }
    }

    #[test]
    fn setgid_op_delegates_to_step_setgid() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SetgidOp {
            target: proc_cap.clone(),
            new_gid: Gid(2000),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(CredChange::Replaced { new, .. }) => {
                assert_eq!(new.gid, Gid(2000));
                assert_eq!(new.egid, Gid(2000));
                assert_eq!(new.sgid, Gid(2000));
            }
            other => panic!("expected Done(Replaced), got {other:?}"),
        }
    }

    #[test]
    fn setreuid_op_delegates_to_step_setreuid() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SetreuidOp {
            target: proc_cap.clone(),
            ruid: Some(Uid(1000)),
            euid: Some(Uid(1001)),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(CredChange::Replaced { new, .. }) => {
                assert_eq!(new.uid, Uid(1000));
                assert_eq!(new.euid, Uid(1001));
                // Privileged caller: per step_setreuid's Linux quirk
                // (ruid was set), suid is bumped to post-call euid.
                assert_eq!(new.suid, Uid(1001));
            }
            other => panic!("expected Done(Replaced), got {other:?}"),
        }
    }

    #[test]
    fn setresuid_op_delegates_to_step_setresuid() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SetresuidOp {
            target: proc_cap.clone(),
            ruid: Some(Uid(1000)),
            euid: Some(Uid(1001)),
            suid: Some(Uid(1002)),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(CredChange::Replaced { new, .. }) => {
                assert_eq!(new.uid, Uid(1000));
                assert_eq!(new.euid, Uid(1001));
                assert_eq!(new.suid, Uid(1002));
            }
            other => panic!("expected Done(Replaced), got {other:?}"),
        }
    }

    #[test]
    fn setresgid_op_delegates_to_step_setresgid() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SetresgidOp {
            target: proc_cap.clone(),
            rgid: Some(Gid(2000)),
            egid: Some(Gid(2001)),
            sgid: Some(Gid(2002)),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(CredChange::Replaced { new, .. }) => {
                assert_eq!(new.gid, Gid(2000));
                assert_eq!(new.egid, Gid(2001));
                assert_eq!(new.sgid, Gid(2002));
            }
            other => panic!("expected Done(Replaced), got {other:?}"),
        }
    }

    #[test]
    fn setregid_op_delegates_to_step_setregid() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        let mut op = SetregidOp {
            target: proc_cap.clone(),
            rgid: Some(Gid(2000)),
            egid: Some(Gid(2001)),
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(CredChange::Replaced { new, .. }) => {
                assert_eq!(new.gid, Gid(2000));
                assert_eq!(new.egid, Gid(2001));
                // Privileged caller: per step_setregid's Linux quirk
                // (rgid was set), sgid is bumped to post-call egid.
                assert_eq!(new.sgid, Gid(2001));
            }
            other => panic!("expected Done(Replaced), got {other:?}"),
        }
    }

    #[test]
    fn apply_suid_for_exec_op_lifts_option_into_done() {
        let _g = setup();
        let proc_cap = bootstrap_init_process(fresh_aspace()).expect("bootstrap");
        // file_mode without S_ISUID/S_ISGID: at_secure must be false,
        // and the wrap must lift the `Some(_)` into `StepOutcome::Done`.
        let mut op = ApplySuidForExecOp {
            target: proc_cap.clone(),
            file_uid: Uid(1000),
            file_gid: Gid(2000),
            file_mode: 0o755,
        };
        let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();
        let outcome = op.step(&mut ctx);
        match outcome {
            StepOutcome::Done(Some(o)) => {
                assert!(!o.at_secure);
            }
            other => panic!("expected Done(Some(_)), got {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests;
