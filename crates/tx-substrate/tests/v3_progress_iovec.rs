//! v3 `IoVecProgress` monoid pin tests.
//!
//! `IoVecProgress` is the per-step accumulator for scatter/gather ops
//! (`readv`, `writev`, `preadv`, `pwritev`). It carries TWO fields:
//! `iovecs_complete` (number of fully-consumed iovecs in the array)
//! and `partial_bytes_in_current` (bytes already written into the
//! iovec currently being filled). Composition under `extend` follows
//! a cursor-style rule: when the rhs reports any iovecs complete, the
//! lhs's partial bytes have been absorbed into one of the iovecs the
//! rhs counted, so the combined `partial_bytes_in_current` is the
//! rhs's value (it refers to a later iovec). When the rhs only made
//! progress within the same iovec, the partial-byte counters add.
//!
//! txdoc cross-refs (canonical anchors from `docs/Txv3/03_STEP_MODEL_v2.md`):
//! - txdoc:TXV3-STEP-MODEL-V2 (entire algebra)
//! - txdoc:STEP-V2-PROGRESS-TYPED-1 (StepProgress is a monoid; scatter/gather variant)

use tx_substrate::step_v3::{IoVecProgress, StepProgress};

// -- Empty + constructors -----------------------------------------------------

#[test]
fn iovec_progress_empty_is_double_zero_and_is_empty() {
    assert_eq!(IoVecProgress::EMPTY.iovecs_complete(), 0);
    assert_eq!(IoVecProgress::EMPTY.partial_bytes_in_current(), 0);
    assert!(IoVecProgress::EMPTY.is_empty());

    // Either field alone is enough to make is_empty false.
    assert!(!IoVecProgress::new(0, 1).is_empty());
    assert!(!IoVecProgress::new(1, 0).is_empty());
}

// -- Within-same-iovec composition (rhs.iovecs_complete == 0) -----------------

#[test]
fn iovec_progress_extend_within_same_iovec_sums_partial_bytes() {
    let mut acc = IoVecProgress::new(0, 100);
    acc.extend(IoVecProgress::new(0, 50));
    assert_eq!(acc.iovecs_complete(), 0);
    assert_eq!(acc.partial_bytes_in_current(), 150);
}

// -- Cross-iovec composition (rhs.iovecs_complete > 0) ------------------------

#[test]
fn iovec_progress_extend_advancing_iovecs_resets_partial_to_rhs() {
    // lhs: 2 iovecs done plus 999 partial bytes into the 3rd.
    // rhs: 1 more iovec done plus 17 partial bytes into the next.
    // The rhs finishing an iovec means the lhs's 999 bytes were
    // absorbed into that completion; the surviving partial cursor is
    // the rhs's 17.
    let mut acc = IoVecProgress::new(2, 999);
    acc.extend(IoVecProgress::new(1, 17));
    assert_eq!(acc.iovecs_complete(), 3);
    assert_eq!(acc.partial_bytes_in_current(), 17);
}

// -- Identity laws ------------------------------------------------------------

const ID_SEEDS: [(u32, usize); 5] = [(0, 0), (0, 7), (1, 0), (1, 99), (1024, 4096)];

#[test]
fn iovec_progress_left_identity() {
    // EMPTY.extend(x) == x
    for &(c, p) in &ID_SEEDS {
        let mut acc = IoVecProgress::EMPTY;
        acc.extend(IoVecProgress::new(c, p));
        assert_eq!(
            acc.iovecs_complete(),
            c,
            "left identity broke iovecs at ({c}, {p})"
        );
        assert_eq!(
            acc.partial_bytes_in_current(),
            p,
            "left identity broke partial at ({c}, {p})"
        );
    }
}

#[test]
fn iovec_progress_right_identity() {
    // x.extend(EMPTY) == x. EMPTY has iovecs_complete == 0 so the
    // within-same-iovec rule applies; lhs's partial is preserved and
    // rhs adds zero.
    for &(c, p) in &ID_SEEDS {
        let mut acc = IoVecProgress::new(c, p);
        acc.extend(IoVecProgress::EMPTY);
        assert_eq!(
            acc.iovecs_complete(),
            c,
            "right identity broke iovecs at ({c}, {p})"
        );
        assert_eq!(
            acc.partial_bytes_in_current(),
            p,
            "right identity broke partial at ({c}, {p})"
        );
    }
}

// -- Associativity ------------------------------------------------------------

const PARTIAL_SEEDS: [usize; 4] = [0, 1, 64, 4096];

#[test]
fn iovec_progress_extend_associative_within_same_iovec() {
    // (0, a).extend((0, b)).extend((0, c)) == (0, a).extend((0, b).extend((0, c)))
    for &a in &PARTIAL_SEEDS {
        for &b in &PARTIAL_SEEDS {
            for &c in &PARTIAL_SEEDS {
                let mut left = IoVecProgress::new(0, a);
                left.extend(IoVecProgress::new(0, b));
                left.extend(IoVecProgress::new(0, c));

                let mut right_inner = IoVecProgress::new(0, b);
                right_inner.extend(IoVecProgress::new(0, c));
                let mut right = IoVecProgress::new(0, a);
                right.extend(right_inner);

                assert_eq!(
                    left.iovecs_complete(),
                    right.iovecs_complete(),
                    "within-iovec associativity broke iovecs at ({a}, {b}, {c})"
                );
                assert_eq!(
                    left.partial_bytes_in_current(),
                    right.partial_bytes_in_current(),
                    "within-iovec associativity broke partial at ({a}, {b}, {c})"
                );
            }
        }
    }
}

#[test]
fn iovec_progress_extend_associative_across_iovec_advance() {
    // Load-bearing: composition that crosses the iovec-advance
    // boundary must associate. Inputs are picked so each pairing
    // exercises the cross-iovec reset rule.
    //
    // Triple: (2, 100), (1, 50), (3, 7)
    //
    // Left fold: ((2, 100) . (1, 50)) . (3, 7)
    //   step 1: (2, 100) . (1, 50) -> rhs.iovecs_complete > 0 => (3, 50)
    //   step 2: (3, 50)  . (3, 7)  -> rhs.iovecs_complete > 0 => (6, 7)
    //
    // Right fold: (2, 100) . ((1, 50) . (3, 7))
    //   inner:  (1, 50) . (3, 7)   -> rhs.iovecs_complete > 0 => (4, 7)
    //   outer:  (2, 100) . (4, 7)  -> rhs.iovecs_complete > 0 => (6, 7)
    //
    // Both folds must yield (6, 7).
    let a = IoVecProgress::new(2, 100);
    let b = IoVecProgress::new(1, 50);
    let c = IoVecProgress::new(3, 7);

    let mut left = a;
    left.extend(b);
    left.extend(c);

    let mut right_inner = b;
    right_inner.extend(c);
    let mut right = a;
    right.extend(right_inner);

    assert_eq!(
        left.iovecs_complete(),
        right.iovecs_complete(),
        "cross-iovec associativity broke iovecs"
    );
    assert_eq!(
        left.partial_bytes_in_current(),
        right.partial_bytes_in_current(),
        "cross-iovec associativity broke partial"
    );
    assert_eq!(left.iovecs_complete(), 6);
    assert_eq!(left.partial_bytes_in_current(), 7);
}

// -- Trait bound --------------------------------------------------------------

#[test]
fn iovec_progress_satisfies_step_progress_bound() {
    fn _bound<P: tx_substrate::step_v3::StepProgress>() {}
    _bound::<IoVecProgress>();
}
