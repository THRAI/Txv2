//! Directory-enumeration progress accumulator.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` txdoc:STEP-V2-PROGRESS-TYPED-1.
//! Enumeration ops (`getdents`, …) are cursor-driven: each step reads
//! more entries starting from the previous cursor. The accumulator's
//! two fields play different roles under `StepProgress::extend`:
//!
//! - `count` accumulates additively across `extend` calls (mirrors
//!   `ByteProgress::bytes`).
//! - `cursor` is a high-water mark, not a sum. When two progress
//!   chunks combine, the cursor advances to the rhs cursor *iff* the
//!   rhs has progress (`other.count > 0`); otherwise the lhs cursor
//!   wins. This matches how `getdents64` composes when the kernel
//!   returns partial enumerations across multiple steps: counts add,
//!   cursor monotonically moves forward.

use super::StepProgress;

/// Opaque directory-enumeration cursor.
///
/// PR-1 step 1 placeholder: this is a newtype over `u64` carrying an
/// opaque position. Later PRs of the v3 TDD migration replace this
/// with a typed dcache cursor handle once the directory subsystem
/// moves to v3.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DirCursor(u64);

impl DirCursor {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Progress accumulator for directory enumeration.
///
/// `count` is the running total of entries enumerated; `cursor` is
/// the position at which the next step should resume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EntryProgress {
    count: u32,
    cursor: DirCursor,
}

impl EntryProgress {
    pub const fn new(count: u32, cursor: DirCursor) -> Self {
        Self { count, cursor }
    }
    pub const fn count(self) -> u32 {
        self.count
    }
    pub const fn cursor(self) -> DirCursor {
        self.cursor
    }
}

impl StepProgress for EntryProgress {
    type Output = ();
    const EMPTY: Self = EntryProgress {
        count: 0,
        cursor: DirCursor(0),
    };

    fn is_empty(&self) -> bool {
        // Cursor is irrelevant for emptiness: only the running count
        // determines whether any progress has been made.
        self.count == 0
    }

    fn into_output(self) -> Option<()> {
        None
    }

    fn trace_kind(&self) -> u8 {
        3
    }
    fn trace_value(&self) -> u32 {
        self.count
    }

    fn extend(&mut self, other: Self) {
        // Count: saturating add for parity with `ByteProgress`.
        self.count = self.count.saturating_add(other.count);
        // Cursor: rhs cursor wins iff rhs has progress; otherwise
        // leave the lhs cursor untouched. This implements the
        // "high-water mark advances forward" semantics pinned by the
        // monoid-law tests.
        if other.count > 0 {
            self.cursor = other.cursor;
        }
    }
}
