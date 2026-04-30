//! Reactor-local asynchronous-trap markers.
//!
//! AST markers are temporal task-runtime facts. They are consumed by the
//! reactor/thread-runtime boundary between two polls of the same task. This
//! module intentionally stops at mechanism: it does not route POSIX signals,
//! build handler frames, touch `ThreadPayload`, or integrate with trap return.

use alloc::vec::Vec;

/// A reactor-local reason to run the AST pass before the next task poll or
/// userspace re-entry.
///
/// These variants are marker kinds, not payloads. The code that consumes an AST
/// batch must re-observe the real subsystem/thread-runtime state before acting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AstMarker {
    /// Thread-runtime interrupt summary should be re-observed.
    InterruptCheck,
    /// A reactor-local fault-injection/checkpoint pass is pending.
    FaultInjection,
    /// Preemption-related task-local bookkeeping should be re-observed.
    PreemptCheck,
    /// The task should run its reactor drain/teardown check.
    Drain,
    /// Reserved local marker space for narrow reactor experiments/tests.
    Local(u8),
}

/// Result of queueing an AST marker into a slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AstQueueEffect {
    /// The marker was absent and was appended to the pending order.
    Queued,
    /// The marker was already pending and no new queue entry was added.
    Coalesced,
}

/// A consumed AST batch.
///
/// Batches preserve first-seen marker order. Consuming a slot clears its
/// coalescing state, so the same marker can be queued again for a later
/// between-polls pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AstBatch {
    markers: Vec<AstMarker>,
}

impl AstBatch {
    pub fn is_empty(&self) -> bool {
        self.markers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.markers.len()
    }

    pub fn as_slice(&self) -> &[AstMarker] {
        &self.markers
    }

    pub fn contains(&self, marker: AstMarker) -> bool {
        self.markers.contains(&marker)
    }

    pub fn into_vec(self) -> Vec<AstMarker> {
        self.markers
    }
}

impl IntoIterator for AstBatch {
    type Item = AstMarker;
    type IntoIter = alloc::vec::IntoIter<AstMarker>;

    fn into_iter(self) -> Self::IntoIter {
        self.markers.into_iter()
    }
}

/// Per-task AST marker slot.
///
/// A slot queues distinct marker kinds in first-seen order. Re-queueing a
/// marker already pending in the same slot coalesces into the existing entry.
/// The slot is meant to be stored on reactor task-table state, not in any
/// semantic subsystem object or zone entity.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AstSlot {
    pending: Vec<AstMarker>,
}

impl AstSlot {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn pending(&self) -> &[AstMarker] {
        &self.pending
    }

    pub fn has_pending(&self, marker: AstMarker) -> bool {
        self.pending.contains(&marker)
    }

    /// Queue a marker for the next AST consumption point.
    ///
    /// Ordering is first-seen within this slot. A duplicate marker coalesces
    /// and keeps the original position.
    pub fn queue(&mut self, marker: AstMarker) -> AstQueueEffect {
        if self.pending.contains(&marker) {
            AstQueueEffect::Coalesced
        } else {
            self.pending.push(marker);
            AstQueueEffect::Queued
        }
    }

    /// Consume all currently pending AST markers.
    ///
    /// The returned batch owns the drained markers. New markers queued after
    /// this call belong to a later between-polls pass.
    pub fn consume(&mut self) -> AstBatch {
        AstBatch {
            markers: core::mem::take(&mut self.pending),
        }
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}
