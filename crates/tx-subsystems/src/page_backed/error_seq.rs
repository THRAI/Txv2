use core::sync::atomic::{AtomicU64, Ordering};

use crate::execution::Errno;
use crate::sync::SpinMutex;

#[derive(Debug)]
pub struct ErrorSeq {
    sequence: AtomicU64,
    latest: SpinMutex<Option<Errno>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorCursor {
    observed: u64,
}

impl ErrorSeq {
    pub const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            latest: SpinMutex::new(None),
        }
    }

    pub fn record(&self, errno: Errno) {
        *self.latest.lock() = Some(errno);
        self.sequence.fetch_add(1, Ordering::AcqRel);
    }

    pub fn observe(&self, cursor: &mut ErrorCursor) -> Option<Errno> {
        let current = self.sequence.load(Ordering::Acquire);
        if current == cursor.observed {
            return None;
        }
        cursor.observed = current;
        *self.latest.lock()
    }

    pub fn current(&self) -> u64 {
        self.sequence.load(Ordering::Acquire)
    }

    pub fn snapshot_cursor(&self) -> ErrorCursor {
        ErrorCursor::from_observed(self.current())
    }
}

impl Default for ErrorSeq {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorCursor {
    pub const fn new() -> Self {
        Self { observed: 0 }
    }

    pub const fn from_observed(observed: u64) -> Self {
        Self { observed }
    }

    pub const fn observed(self) -> u64 {
        self.observed
    }
}
