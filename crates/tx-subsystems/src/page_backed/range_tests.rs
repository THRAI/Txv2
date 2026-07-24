use super::{
    PageIndex, PageRange, RangeReservationError, RangeReservationKind, RangeReservationTable,
};

#[test]
fn range_reservation_rejects_overlapping_exclusive_ranges() {
    let mut table = RangeReservationTable::new();
    let first = table
        .try_reserve(
            PageRange::new(PageIndex::new(4), 4),
            RangeReservationKind::Truncate,
        )
        .expect("first range should reserve");

    let error = table
        .try_reserve(
            PageRange::new(PageIndex::new(6), 2),
            RangeReservationKind::DirectWrite,
        )
        .expect_err("overlapping direct write must conflict with truncate");

    assert_eq!(
        error,
        RangeReservationError::Conflict {
            existing: first.id(),
            kind: RangeReservationKind::Truncate,
        }
    );
    assert_eq!(table.len(), 1);
}

#[test]
fn range_reservation_allows_non_overlapping_and_shared_reads() {
    let mut table = RangeReservationTable::new();
    table
        .try_reserve(
            PageRange::new(PageIndex::new(0), 4),
            RangeReservationKind::BufferedRead,
        )
        .expect("first buffered read should reserve");
    table
        .try_reserve(
            PageRange::new(PageIndex::new(2), 2),
            RangeReservationKind::BufferedRead,
        )
        .expect("overlapping buffered reads should share");
    table
        .try_reserve(
            PageRange::new(PageIndex::new(8), 2),
            RangeReservationKind::DirectWrite,
        )
        .expect("non-overlapping direct write should reserve");

    assert_eq!(table.len(), 3);
}

#[test]
fn range_reservation_release_before_yield_allows_retry() {
    let mut table = RangeReservationTable::new();
    let reservation = table
        .try_reserve(
            PageRange::new(PageIndex::new(10), 2),
            RangeReservationKind::Fsync,
        )
        .expect("fsync range should reserve");

    assert!(table.release(reservation.id()));
    assert!(table.is_empty());

    table
        .try_reserve(
            PageRange::new(PageIndex::new(10), 2),
            RangeReservationKind::DirectWrite,
        )
        .expect("retry after release-before-yield should reserve");
}
