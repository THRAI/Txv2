//! `IoVecProgress` — per-step accumulator for scatter/gather ops.
//!
//! Per `docs/Txv3/03_STEP_MODEL_v2.md` (txdoc:STEP-V2-PROGRESS-TYPED-1),
//! scatter/gather ops (`readv`, `writev`, `preadv`, `pwritev`) walk an
//! array of iovecs. At any step boundary the cursor has two parts:
//! `iovecs_complete` (how many iovecs have been fully consumed) and
//! `partial_bytes_in_current` (bytes already moved into the iovec
//! currently being filled).
//!
//! The `(IoVecProgress, EMPTY, extend)` triple is monoid-shaped per
//! STEP-3. The composition rule under `extend` is cursor-style:
//!
//! - When `other.iovecs_complete > 0`, the rhs has finished at least
//!   one more iovec; its `partial_bytes_in_current` therefore refers
//!   to a later iovec than the lhs's. The lhs's partial cursor is
//!   absorbed into one of the iovecs the rhs counted as complete, so
//!   the combined `partial_bytes_in_current` is `other`'s value and
//!   `iovecs_complete` is the (saturating) sum.
//! - When `other.iovecs_complete == 0`, the rhs only made progress
//!   within the same iovec, so the partial-byte counters add
//!   (saturating) and `iovecs_complete` is unchanged.
//!
//! `is_empty` is true iff both fields are zero.

use super::StepProgress;

/// Scatter/gather ops: `readv`, `writev`, `preadv`, `pwritev`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoVecProgress {
    iovecs_complete: u32,
    partial_bytes_in_current: usize,
}

impl IoVecProgress {
    pub const fn new(iovecs_complete: u32, partial_bytes_in_current: usize) -> Self {
        Self {
            iovecs_complete,
            partial_bytes_in_current,
        }
    }

    pub const fn iovecs_complete(self) -> u32 {
        self.iovecs_complete
    }

    pub const fn partial_bytes_in_current(self) -> usize {
        self.partial_bytes_in_current
    }
}

impl StepProgress for IoVecProgress {
    const EMPTY: Self = IoVecProgress {
        iovecs_complete: 0,
        partial_bytes_in_current: 0,
    };

    fn is_empty(&self) -> bool {
        self.iovecs_complete == 0 && self.partial_bytes_in_current == 0
    }

    fn extend(&mut self, other: Self) {
        if other.iovecs_complete > 0 {
            self.iovecs_complete = self.iovecs_complete.saturating_add(other.iovecs_complete);
            self.partial_bytes_in_current = other.partial_bytes_in_current;
        } else {
            self.partial_bytes_in_current = self
                .partial_bytes_in_current
                .saturating_add(other.partial_bytes_in_current);
        }
    }
}
