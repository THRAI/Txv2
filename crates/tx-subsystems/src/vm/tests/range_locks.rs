// Auto-extracted from `crates/tx-subsystems/src/vm/tests.rs` (2026-05-08 jumbo split).
// User-range / range-lock unit tests; depends on shared setup in `super`.
#![cfg_attr(test, allow(unused_imports))]
use super::*;

#[test]
fn vm_user_range_rejects_zero_unaligned_and_overflow() {
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(0), 0),
        Err(UserRangeError::ZeroLength)
    );
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(1), USER_PAGE_SIZE),
        Err(UserRangeError::Unaligned)
    );
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(0), USER_PAGE_SIZE - 1),
        Err(UserRangeError::Unaligned)
    );
    assert_eq!(
        UserRange::new_aligned(UserVirtAddr(usize::MAX - 4095), USER_PAGE_SIZE),
        Err(UserRangeError::Overflow)
    );
}

#[test]
fn vm_user_range_iterates_pages_and_counts_them() {
    let pages = range(0x4000, 3);
    assert_eq!(pages.page_count(), 3);
    assert_eq!(pages.iter_pages().len(), 3);

    let mut iter = pages.iter_pages();
    assert_eq!(iter.next(), Some(UserPage(4)));
    assert_eq!(iter.next(), Some(UserPage(5)));
    assert_eq!(iter.next(), Some(UserPage(6)));
    assert_eq!(iter.next(), None);

    assert_eq!(UserVirtAddr(0x4123).containing_page(), UserPage(4));
    assert_eq!(
        UserRange::containing_page(UserVirtAddr(0x4123)),
        Ok(range(0x4000, 1))
    );
    assert_eq!(
        UserRange::containing_page(UserVirtAddr(usize::MAX)),
        Err(UserRangeError::Overflow)
    );
}

#[test]
fn vm_range_lock_conflict_matrix_matches_modes() {
    let lock = RangeLock::new();
    let first = range(0x1000, 2);
    let overlap = range(0x2000, 1);
    let disjoint = range(0x8000, 1);

    let writer = acquired(lock.acquire_step_rich(first, LockMode::ExclusiveWriter));
    would_block(lock.acquire_step_rich(overlap, LockMode::ExclusiveWriter));
    would_block(lock.acquire_step_rich(overlap, LockMode::Materializer));
    let disjoint_writer = acquired(lock.acquire_step_rich(disjoint, LockMode::ExclusiveWriter));
    drop(disjoint_writer);
    drop(writer);

    let materializer_a = acquired(lock.acquire_step_rich(first, LockMode::Materializer));
    would_block(lock.acquire_step_rich(overlap, LockMode::Materializer));
    let materializer_b = acquired(lock.acquire_step_rich(disjoint, LockMode::Materializer));
    would_block(lock.acquire_step_rich(overlap, LockMode::ExclusiveWriter));
    drop(materializer_b);
    drop(materializer_a);
}

#[test]
fn vm_range_lock_pending_writer_blocks_new_materializers() {
    let lock = RangeLock::new();
    let first = range(0x1000, 1);

    let materializer = acquired(lock.acquire_step_rich(first, LockMode::Materializer));
    let pending = would_block(lock.acquire_step_rich(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("blocked writer should declare pending range");

    would_block(lock.acquire_step_rich(first, LockMode::Materializer));
    drop(materializer);

    let writer = acquired(pending.try_acquire());
    would_block(lock.acquire_step_rich(first, LockMode::Materializer));
    drop(writer);

    let materializer_after_drop = acquired(lock.acquire_step_rich(first, LockMode::Materializer));
    drop(materializer_after_drop);
}

#[test]
fn vm_range_lock_overlapping_pending_writers_are_fifo() {
    let lock = RangeLock::new();
    let first = range(0x1000, 1);

    let materializer = acquired(lock.acquire_step_rich(first, LockMode::Materializer));
    let pending_a = would_block(lock.acquire_step_rich(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("first writer queues");
    let pending_b = would_block(lock.acquire_step_rich(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("second writer queues");
    drop(materializer);

    let blocked_b = would_block(pending_b.try_acquire());
    let writer_a = acquired(pending_a.try_acquire());
    drop(writer_a);

    let writer_b = acquired(
        blocked_b
            .pending_writer()
            .expect("second writer remains queued")
            .try_acquire(),
    );
    drop(writer_b);
}

#[test]
fn vm_range_lock_acquires_two_ranges_atomically() {
    let lock = RangeLock::new();
    let a = range(0x1000, 1);
    let b = range(0x4000, 1);

    let pair = pair_acquired(lock.acquire_pair_step_rich(
        (a, LockMode::ExclusiveWriter),
        (b, LockMode::ExclusiveWriter),
    ));
    would_block(lock.acquire_step_rich(a, LockMode::Materializer));
    would_block(lock.acquire_step_rich(b, LockMode::Materializer));
    drop(pair);

    let after = acquired(lock.acquire_step_rich(a, LockMode::Materializer));
    drop(after);
}

#[test]
fn vm_range_lock_tree_removal_clears_only_removed_overlap() {
    let lock = RangeLock::new();
    let low = range(0x1000, 1);
    let middle = range(0x5000, 1);
    let high = range(0x9000, 1);

    let low_writer = acquired(lock.acquire_step_rich(low, LockMode::ExclusiveWriter));
    let middle_writer = acquired(lock.acquire_step_rich(middle, LockMode::ExclusiveWriter));
    let high_writer = acquired(lock.acquire_step_rich(high, LockMode::ExclusiveWriter));

    would_block(lock.acquire_step_rich(middle, LockMode::Materializer));
    drop(middle_writer);

    let middle_materializer = acquired(lock.acquire_step_rich(middle, LockMode::Materializer));
    would_block(lock.acquire_step_rich(low, LockMode::Materializer));
    would_block(lock.acquire_step_rich(high, LockMode::Materializer));

    drop(middle_materializer);
    drop(low_writer);
    drop(high_writer);
}

#[test]
fn vm_range_lock_tree_keeps_disjoint_reservations_independent() {
    let lock = RangeLock::new();
    let a = range(0x1000, 1);
    let b = range(0x8000, 1);
    let c = range(0x10000, 1);

    let writer_a = acquired(lock.acquire_step_rich(a, LockMode::ExclusiveWriter));
    let writer_b = acquired(lock.acquire_step_rich(b, LockMode::ExclusiveWriter));
    let materializer_c = acquired(lock.acquire_step_rich(c, LockMode::Materializer));

    would_block(lock.acquire_step_rich(a, LockMode::Materializer));
    would_block(lock.acquire_step_rich(b, LockMode::Materializer));
    would_block(lock.acquire_step_rich(c, LockMode::Materializer));
    drop(materializer_c);
    drop(writer_b);
    drop(writer_a);
}

#[test]
fn vm_range_lock_tree_pending_fifo_is_range_scoped() {
    let lock = RangeLock::new();
    let first = range(0x2000, 1);
    let disjoint = range(0xa000, 1);

    let materializer = acquired(lock.acquire_step_rich(first, LockMode::Materializer));
    let pending_a = would_block(lock.acquire_step_rich(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("first overlapping writer queues");
    let disjoint_writer = acquired(lock.acquire_step_rich(disjoint, LockMode::ExclusiveWriter));
    let pending_b = would_block(lock.acquire_step_rich(first, LockMode::ExclusiveWriter))
        .pending_writer()
        .expect("second overlapping writer queues");

    drop(materializer);

    let blocked_b = would_block(pending_b.try_acquire());
    let writer_a = acquired(pending_a.try_acquire());
    drop(writer_a);

    let writer_b = acquired(
        blocked_b
            .pending_writer()
            .expect("second overlapping writer remains queued")
            .try_acquire(),
    );

    drop(writer_b);
    drop(disjoint_writer);
}

#[test]
fn vm_range_lock_tree_active_writer_blocks_materializer_overlap_only() {
    let lock = RangeLock::new();
    let writer_range = range(0x7000, 2);
    let overlap = range(0x8000, 1);
    let disjoint = range(0xb000, 1);

    let writer = acquired(lock.acquire_step_rich(writer_range, LockMode::ExclusiveWriter));

    would_block(lock.acquire_step_rich(overlap, LockMode::Materializer));
    let disjoint_materializer = acquired(lock.acquire_step_rich(disjoint, LockMode::Materializer));

    drop(disjoint_materializer);
    drop(writer);
}
