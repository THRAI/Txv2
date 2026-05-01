//! Reactor-local preemption markers.
//!
//! These markers are the mechanism-level surface for deferred rescheduling
//! decisions. Trap and scheduler code can set them later; the reactor consumes
//! them at poll boundaries. The markers do not encode scheduling policy and do
//! not touch userspace trap-return mechanics.

use core::sync::atomic::{AtomicU8, Ordering};

const NEED_RESCHED: u8 = 0b0000_0001;
const SLICE_EXPIRED: u8 = 0b0000_0010;

/// One preemption marker kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreemptMarker {
    /// A scheduling decision is deferred until the next poll boundary.
    NeedResched,
    /// The currently dispatched slice expired.
    SliceExpired,
}

impl PreemptMarker {
    const fn bit(self) -> u8 {
        match self {
            Self::NeedResched => NEED_RESCHED,
            Self::SliceExpired => SLICE_EXPIRED,
        }
    }
}

/// Snapshot of preemption markers consumed at a poll boundary.
///
/// Marker order is not significant: this is a coalescing bitset. Repeated sets
/// before a consume produce one bit in the snapshot.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PreemptMarkers {
    bits: u8,
}

impl PreemptMarkers {
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    pub const fn need_resched(self) -> bool {
        self.bits & NEED_RESCHED != 0
    }

    pub const fn slice_expired(self) -> bool {
        self.bits & SLICE_EXPIRED != 0
    }

    pub const fn contains(self, marker: PreemptMarker) -> bool {
        self.bits & marker.bit() != 0
    }

    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    pub const fn bits(self) -> u8 {
        self.bits
    }

    const fn from_bits(bits: u8) -> Self {
        Self {
            bits: bits & (NEED_RESCHED | SLICE_EXPIRED),
        }
    }
}

/// Atomic preemption marker slot for one reactor poll boundary.
///
/// The natural owner is per-hart scheduling state, or a narrow per-dispatch
/// test harness. Setting a marker is idempotent. `consume` atomically drains the
/// current snapshot; markers set after the swap are observed by a later consume.
#[derive(Debug)]
pub struct PreemptionPoint {
    bits: AtomicU8,
}

impl PreemptionPoint {
    pub const fn new() -> Self {
        Self {
            bits: AtomicU8::new(0),
        }
    }

    pub fn mark(&self, marker: PreemptMarker) {
        self.bits.fetch_or(marker.bit(), Ordering::Release);
    }

    pub fn mark_need_resched(&self) {
        self.mark(PreemptMarker::NeedResched);
    }

    pub fn mark_slice_expired(&self) {
        self.mark(PreemptMarker::SliceExpired);
    }

    pub fn is_marked(&self, marker: PreemptMarker) -> bool {
        self.snapshot().contains(marker)
    }

    pub fn snapshot(&self) -> PreemptMarkers {
        PreemptMarkers::from_bits(self.bits.load(Ordering::Acquire))
    }

    pub fn consume(&self) -> PreemptMarkers {
        PreemptMarkers::from_bits(self.bits.swap(0, Ordering::AcqRel))
    }

    pub fn clear(&self) {
        self.bits.store(0, Ordering::Release);
    }
}

impl Default for PreemptionPoint {
    fn default() -> Self {
        Self::new()
    }
}
