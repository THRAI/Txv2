//! Range reservation staging for PageBacked coherency operations.
//!
//! The table is deliberately owner-locked by its caller in this phase. A
//! reservation token is not an async guard: callers that will yield must release
//! the token first and retry after wake.

use alloc::vec::Vec;

use super::PageIndex;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RangeReservationId(u64);

impl RangeReservationId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageRange {
    start: PageIndex,
    page_count: u64,
}

impl PageRange {
    pub const fn new(start: PageIndex, page_count: u64) -> Self {
        Self { start, page_count }
    }

    pub const fn start(self) -> PageIndex {
        self.start
    }

    pub const fn page_count(self) -> u64 {
        self.page_count
    }

    pub fn end(self) -> Option<PageIndex> {
        self.start
            .as_u64()
            .checked_add(self.page_count)
            .map(PageIndex::new)
    }

    pub const fn is_empty(self) -> bool {
        self.page_count == 0
    }

    pub fn overlaps(self, other: Self) -> bool {
        let (Some(self_end), Some(other_end)) = (self.end(), other.end()) else {
            return false;
        };
        self.start < other_end && other.start < self_end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeReservationKind {
    BufferedRead,
    BufferedWrite,
    Fsync,
    Truncate,
    Fallocate,
    DirectRead,
    DirectWrite,
    Writeback,
}

impl RangeReservationKind {
    const fn conflicts_with(self, other: Self) -> bool {
        !matches!(
            (self, other),
            (Self::BufferedRead, Self::BufferedRead) | (Self::DirectRead, Self::DirectRead)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RangeReservation {
    id: RangeReservationId,
    range: PageRange,
    kind: RangeReservationKind,
}

impl RangeReservation {
    pub const fn id(self) -> RangeReservationId {
        self.id
    }

    pub const fn range(self) -> PageRange {
        self.range
    }

    pub const fn kind(self) -> RangeReservationKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeReservationError {
    EmptyRange,
    Overflow,
    Conflict {
        existing: RangeReservationId,
        kind: RangeReservationKind,
    },
}

#[derive(Debug, Default)]
pub struct RangeReservationTable {
    next_id: u64,
    reservations: Vec<RangeReservation>,
}

impl RangeReservationTable {
    pub const fn new() -> Self {
        Self {
            next_id: 1,
            reservations: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.reservations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.reservations.is_empty()
    }

    pub fn try_reserve(
        &mut self,
        range: PageRange,
        kind: RangeReservationKind,
    ) -> Result<RangeReservation, RangeReservationError> {
        if range.is_empty() {
            return Err(RangeReservationError::EmptyRange);
        }
        if range.end().is_none() {
            return Err(RangeReservationError::Overflow);
        }
        for existing in &self.reservations {
            if range.overlaps(existing.range) && kind.conflicts_with(existing.kind) {
                return Err(RangeReservationError::Conflict {
                    existing: existing.id,
                    kind: existing.kind,
                });
            }
        }

        let reservation = RangeReservation {
            id: RangeReservationId::new(self.next_id),
            range,
            kind,
        };
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.reservations.push(reservation);
        Ok(reservation)
    }

    pub fn release(&mut self, id: RangeReservationId) -> bool {
        let Some(index) = self
            .reservations
            .iter()
            .position(|reservation| reservation.id == id)
        else {
            return false;
        };
        self.reservations.swap_remove(index);
        true
    }

    pub fn contains(&self, id: RangeReservationId) -> bool {
        self.reservations
            .iter()
            .any(|reservation| reservation.id == id)
    }
}
