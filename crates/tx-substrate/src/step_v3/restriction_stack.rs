//! Closed catalog of restriction kinds and the append-only stack
//! held in `SubjectAuthority::restrictions`.
//!
//! Per `docs/Txv3/01_CONCEPTS_v5.md` and `docs/Txv3/04_SYSCALL_SHAPE_v1.md`:
//! seccomp filters, Landlock rules, and LSM hook stacks compose in the
//! upper-half observe phase as members of `SubjectAuthority::restrictions`.
//! The catalog is closed (ARCH-3-gated extension) and the stack is
//! append-only — `no_new_privs` is a structural property, not an
//! enforcement check, because the only mutator is `append`. The only
//! way to shrink a restriction set is to publish a fresh authority
//! (suid-exec / SUBJ-3 authority replacement), which is a separate
//! publication-boundary operation handled at a different layer.
//!
//! Real per-kind walkers (seccomp BPF VM, Landlock rule eval, LSM
//! stack dispatch) are deferred per the v3 plan; this PR lands the
//! catalog and the append-only stack so `SubjectAuthority::restrictions`
//! has a concrete shape that later PRs cannot silently widen.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (referenced by the upper-half
//!   restriction-stack walk that participates in the step algebra)

use alloc::vec::Vec;

/// Closed catalog of restriction kinds.
///
/// Per `docs/Txv3/01_CONCEPTS_v5.md`: this is a closed catalog;
/// extending it requires ARCH-3 review. Real per-kind walkers
/// (seccomp BPF VM, Landlock rule eval, LSM stack dispatch) are
/// deferred per the v3 plan — only the catalog itself is closed
/// here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestrictionKind {
    /// Seccomp BPF filter program.
    SeccompFilter,
    /// Landlock rule (path-based access restriction).
    LandlockRule,
    /// LSM hook stack (SELinux, AppArmor, Smack, …).
    LsmStack,
}

/// Append-only stack of restrictions installed on a `SubjectAuthority`.
///
/// Per the v3 invariants: restrictions are append-only; once installed,
/// a kind cannot be rewritten or removed. The only way to "shrink" a
/// restriction set is to publish a fresh authority (suid-exec / the
/// SUBJ-3 authority-replacement path), which is a separate
/// publication-boundary operation handled at a different layer.
///
/// Walks proceed in install order from oldest to newest; this matches
/// how seccomp filter chains and Landlock rule additions compose in
/// the existing Linux ABI: older filters/rules run first, newer rules
/// layer on top.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct RestrictionStack {
    kinds: Vec<RestrictionKind>,
}

impl RestrictionStack {
    /// Construct an empty restriction stack.
    pub const fn new() -> Self {
        Self { kinds: Vec::new() }
    }

    /// Append a restriction. The only mutation operation. There is
    /// intentionally no `remove`, `clear`, `pop`, `truncate`, or
    /// `replace` API — the stack is append-only.
    pub fn append(&mut self, kind: RestrictionKind) {
        self.kinds.push(kind);
    }

    /// Number of restrictions currently installed.
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    /// Returns `true` iff no restrictions are installed.
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// Iterate restrictions in install order (oldest to newest).
    ///
    /// The walk-order pin: a downstream walker must encounter older
    /// restrictions first so that newer rules apply on top. The
    /// iterator borrows; calling `walk` again on the same stack yields
    /// the same sequence.
    pub fn walk(&self) -> impl Iterator<Item = RestrictionKind> + '_ {
        self.kinds.iter().copied()
    }
}
