//! v3 `EntryProgress` monoid pin tests.
//!
//! Pins the directory-enumeration progress accumulator from
//! `docs/Txv3/03_STEP_MODEL_v2.md`. Enumeration ops (`getdents`,
//! …) are cursor-driven: the `count` field accumulates additively,
//! while the `cursor` field is a high-water mark that monotonically
//! advances to the right-hand-side cursor when rhs has progress and
//! is otherwise left untouched.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2
//! - txdoc:STEP-V2-PROGRESS-TYPED-1

use tx_substrate::step_v3::{DirCursor, EntryProgress, StepProgress};

const COUNT_SEEDS: [u32; 5] = [0, 1, 7, 64, 1024];
const CURSOR_SEEDS: [u64; 5] = [0, 16, 4096, 1_000_000, u64::MAX];

#[test]
fn entry_progress_empty_is_zero_count_and_is_empty() {
    assert!(EntryProgress::EMPTY.is_empty());
    assert_eq!(EntryProgress::EMPTY.count(), 0);
    // A non-zero count is not empty regardless of cursor.
    assert!(!EntryProgress::new(1, DirCursor::new(0)).is_empty());
    assert!(!EntryProgress::new(1, DirCursor::new(u64::MAX)).is_empty());
}

#[test]
fn entry_progress_left_identity_preserves_count_and_cursor() {
    // EMPTY.extend(x) yields x's count and x's cursor — identity from
    // the left advances the cursor forward to the rhs's value.
    for &(c, p) in &[(0u32, 0u64), (1, 16), (256, 4096), (1_000_000, u64::MAX)] {
        let mut acc = EntryProgress::EMPTY;
        acc.extend(EntryProgress::new(c, DirCursor::new(p)));
        assert_eq!(acc.count(), c, "left identity count broke at ({c}, {p})");
        assert_eq!(
            acc.cursor().raw(),
            p,
            "left identity cursor broke at ({c}, {p})"
        );
    }
}

#[test]
fn entry_progress_right_identity_preserves_count_and_cursor() {
    // x.extend(EMPTY) does not rewind the cursor: EMPTY.count == 0
    // means rhs has no progress, so the lhs cursor wins.
    for &(c, p) in &[(0u32, 0u64), (1, 16), (256, 4096), (1_000_000, u64::MAX)] {
        let mut acc = EntryProgress::new(c, DirCursor::new(p));
        acc.extend(EntryProgress::EMPTY);
        assert_eq!(acc.count(), c, "right identity count broke at ({c}, {p})");
        assert_eq!(
            acc.cursor().raw(),
            p,
            "right identity cursor broke at ({c}, {p})"
        );
    }
}

#[test]
fn entry_progress_extend_count_is_associative() {
    // (a . b) . c == a . (b . c) on the count field. Cursors are
    // arbitrary per element; we only assert count associativity.
    for (i, &a) in COUNT_SEEDS.iter().enumerate() {
        for (j, &b) in COUNT_SEEDS.iter().enumerate() {
            for (k, &c) in COUNT_SEEDS.iter().enumerate() {
                let pa = CURSOR_SEEDS[i];
                let pb = CURSOR_SEEDS[j];
                let pc = CURSOR_SEEDS[k];

                let mut left = EntryProgress::new(a, DirCursor::new(pa));
                left.extend(EntryProgress::new(b, DirCursor::new(pb)));
                left.extend(EntryProgress::new(c, DirCursor::new(pc)));

                let mut right_inner = EntryProgress::new(b, DirCursor::new(pb));
                right_inner.extend(EntryProgress::new(c, DirCursor::new(pc)));
                let mut right = EntryProgress::new(a, DirCursor::new(pa));
                right.extend(right_inner);

                assert_eq!(
                    left.count(),
                    right.count(),
                    "count associativity broke at ({a}, {b}, {c})"
                );
            }
        }
    }
}

#[test]
fn entry_progress_extend_cursor_takes_the_later_position() {
    // Non-empty rhs cursor replaces the lhs cursor; counts add.
    let mut acc = EntryProgress::new(3, DirCursor::new(100));
    acc.extend(EntryProgress::new(5, DirCursor::new(250)));
    assert_eq!(acc.count(), 8);
    assert_eq!(acc.cursor().raw(), 250);
}

#[test]
fn entry_progress_satisfies_step_progress_bound() {
    // Compile-only: `EntryProgress: StepProgress`.
    fn _bound<P: tx_substrate::step_v3::StepProgress>() {}
    _bound::<EntryProgress>();
}
