//! v3 `PageProgress` monoid pin tests.
//!
//! These tests pin the `PageProgress` `StepProgress` impl described in
//! `docs/Txv3/03_STEP_MODEL_v2.md`. `PageProgress { pages: u32 }` is the
//! per-step accumulator for page-moving ops (fault materialization,
//! mlock-population, mmap-population). Mirrors the `byte_progress_*`
//! shape from `tests/v3_algebra.rs`, just for pages instead of bytes.
//!
//! txdoc cross-refs:
//! - txdoc:TXV3-STEP-MODEL-V2
//! - txdoc:STEP-V2-PROGRESS-TYPED-1

use tx_substrate::step::{PageProgress, StepProgress};

const PAGE_SEEDS: [u32; 7] = [0, 1, 4, 16, 256, 4096, 1_000_000];

#[test]
fn page_progress_empty_is_zero_pages_and_is_empty() {
    assert!(PageProgress::EMPTY.is_empty());
    assert_eq!(PageProgress::EMPTY.pages(), 0);
    assert!(!PageProgress::new(1).is_empty());
}

#[test]
fn page_progress_left_identity() {
    // EMPTY.extend(x) == x.pages()
    for &s in &PAGE_SEEDS {
        let mut acc = PageProgress::EMPTY;
        acc.extend(PageProgress::new(s));
        assert_eq!(acc.pages(), s, "left identity broke at seed {s}");
    }
}

#[test]
fn page_progress_right_identity() {
    // x.extend(EMPTY) == x.pages()
    for &s in &PAGE_SEEDS {
        let mut acc = PageProgress::new(s);
        acc.extend(PageProgress::EMPTY);
        assert_eq!(acc.pages(), s, "right identity broke at seed {s}");
    }
}

#[test]
fn page_progress_extend_is_associative() {
    // (a . b) . c == a . (b . c)
    for &a in &PAGE_SEEDS {
        for &b in &PAGE_SEEDS {
            for &c in &PAGE_SEEDS {
                let mut left = PageProgress::new(a);
                left.extend(PageProgress::new(b));
                left.extend(PageProgress::new(c));

                let mut right_inner = PageProgress::new(b);
                right_inner.extend(PageProgress::new(c));
                let mut right = PageProgress::new(a);
                right.extend(right_inner);

                assert_eq!(
                    left.pages(),
                    right.pages(),
                    "associativity broke at ({a}, {b}, {c})"
                );
            }
        }
    }
}

#[test]
fn page_progress_extend_accumulates() {
    let mut p = PageProgress::new(7);
    p.extend(PageProgress::new(13));
    assert_eq!(p.pages(), 20);
}

#[test]
fn page_progress_satisfies_step_progress_bound() {
    // Compile-only assertion: PageProgress satisfies the StepProgress
    // trait bound. If the bound regresses, this test stops compiling.
    fn _bound<P: tx_substrate::step::StepProgress>() {}
    _bound::<PageProgress>();
}
