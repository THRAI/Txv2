//! `PageProgress` — per-step accumulator for page-moving ops.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` (txdoc:STEP-V2-PROGRESS-TYPED-1),
//! page-moving operations (fault materialization, mlock-population,
//! mmap-population) carry a typed `PageProgress { pages: u32 }`
//! accumulator. The `(PageProgress, EMPTY, extend)` triple is monoid-
//! shaped per STEP-3, with `extend` using `saturating_add` for parity
//! with `ByteProgress`.

use super::StepProgress;

/// Page-moving ops: fault materialization, mlock-population,
/// mmap-population.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageProgress {
    pages: u32,
}

impl PageProgress {
    pub const fn new(pages: u32) -> Self {
        Self { pages }
    }
    pub const fn pages(self) -> u32 {
        self.pages
    }
    /// Inherent shorthand for `<PageProgress as StepProgress>::EMPTY`.
    /// Avoids requiring `use StepProgress;` at page-moving call sites
    /// (e.g. `step_v3::StepOutcome::yield_on_wait_source(PageProgress::EMPTY,
    /// source_id, interest_mask)`). Parallel to `ByteProgress::EMPTY`.
    pub const EMPTY: Self = Self { pages: 0 };
}

impl StepProgress for PageProgress {
    type Output = ();
    const EMPTY: Self = PageProgress { pages: 0 };
    fn is_empty(&self) -> bool {
        self.pages == 0
    }
    fn extend(&mut self, other: Self) {
        self.pages = self.pages.saturating_add(other.pages);
    }
    fn into_output(self) -> Option<()> {
        None
    }
}
