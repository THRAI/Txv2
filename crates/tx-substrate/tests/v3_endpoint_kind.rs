//! v3 `EndpointKind` closed-catalog pin tests.
//!
//! These tests pin the wave-3 closed catalog of delegate endpoint
//! *kinds*. The catalog names the classes of userspace agent the kernel
//! may delegate to. The typed per-kind request/reply layer (a
//! `K: EndpointKind` trait with `K::Request` and `K::Result` associated
//! types) lands in PR-4 of the v3 TDD migration plan; today's catalog
//! is just the enum naming the rows.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2 (parent step-model anchor)
//! - txdoc:DELEGATE-V1-ENDPOINT-1 (endpoint kind catalog)
//! - txdoc:DELEGATE-V1-USES-1 (worked-uses sections naming each kind)
//!
//! See also `docs/Txv3/02_INVARIANTS_v5.md` DELEGATE-2 (endpoint kind
//! is type-discriminated; kills covert channels).

use tx_substrate::step_v3::EndpointKind;

// -- closed catalog smoke ----------------------------------------------------

#[test]
fn endpoint_kind_has_exactly_five_variants_via_exhaustive_match() {
    // Build every variant, then exhaustively destructure them. The
    // absence of a wildcard arm is the test: if a sixth real kind
    // lands later (e.g. `EvioctlPerm`) without an ARCH-3 review, this
    // stops compiling. The `Synthetic` member is part of the catalog
    // by design — it is the test-only kind that lets the delegate
    // machinery be exercised without standing up a real userfaultfd /
    // FUSE / fanotify / ptrace handler.
    let cases: [EndpointKind; 5] = [
        EndpointKind::Ufd,
        EndpointKind::Fuse,
        EndpointKind::FanotifyPerm,
        EndpointKind::Ptrace,
        EndpointKind::Synthetic,
    ];

    for kind in cases {
        match kind {
            EndpointKind::Ufd => {}
            EndpointKind::Fuse => {}
            EndpointKind::FanotifyPerm => {}
            EndpointKind::Ptrace => {}
            EndpointKind::Synthetic => {}
        }
    }
}

// -- is_real table -----------------------------------------------------------

#[test]
fn endpoint_kind_is_real_table() {
    // Per `docs/Txv3/05_DELEGATE_v1.md` worked-uses section: Ufd,
    // Fuse, FanotifyPerm, and Ptrace each correspond to a real Linux
    // userspace ABI; only `Synthetic` is a non-real test scaffold.
    let table: [(EndpointKind, bool); 5] = [
        (EndpointKind::Ufd, true),
        (EndpointKind::Fuse, true),
        (EndpointKind::FanotifyPerm, true),
        (EndpointKind::Ptrace, true),
        (EndpointKind::Synthetic, false),
    ];
    for (kind, expected) in table {
        assert_eq!(kind.is_real(), expected, "is_real mismatch for {:?}", kind);
    }
}

// -- permits_fd_injection table ----------------------------------------------

#[test]
fn endpoint_kind_permits_fd_injection_table() {
    // Per `docs/Txv3/05_DELEGATE_v1.md` §8.2 (FUSE), only FUSE replies
    // legitimately carry backing fds for OPEN replies. Ufd and
    // FanotifyPerm use no fd injection; Ptrace is reserved for
    // syscall control. `Synthetic` is conservatively false.
    let table: [(EndpointKind, bool); 5] = [
        (EndpointKind::Ufd, false),
        (EndpointKind::Fuse, true),
        (EndpointKind::FanotifyPerm, false),
        (EndpointKind::Ptrace, false),
        (EndpointKind::Synthetic, false),
    ];
    for (kind, expected) in table {
        assert_eq!(
            kind.permits_fd_injection(),
            expected,
            "permits_fd_injection mismatch for {:?}",
            kind
        );
    }
}

// -- helpers are const -------------------------------------------------------

#[test]
fn endpoint_kind_helpers_are_const() {
    // Compile-time check: both helpers must be usable in `const`
    // contexts so callers (e.g. `const fn` validators in PR-4) can
    // gate on kind without runtime cost.
    const UFD_REAL: bool = EndpointKind::Ufd.is_real();
    const SYN_REAL: bool = EndpointKind::Synthetic.is_real();
    const FUSE_FD: bool = EndpointKind::Fuse.permits_fd_injection();
    const UFD_FD: bool = EndpointKind::Ufd.permits_fd_injection();

    const { assert!(UFD_REAL) };
    const { assert!(!SYN_REAL) };
    const { assert!(FUSE_FD) };
    const { assert!(!UFD_FD) };
}
