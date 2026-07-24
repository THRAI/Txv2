use super::{PageSlot, PageSlotCompletionError, PageSlotFetch, PageSlotFsyncStatus, PageSlotState};
use crate::execution::Errno;
use tx_hal::Ppn;

#[test]
fn page_slot_deduplicates_same_page_fetch() {
    let slot = PageSlot::new();

    let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
        panic!("first miss must own the fetch");
    };
    assert_eq!(slot.begin_fetch(), PageSlotFetch::Joined { generation });

    let snapshot = slot
        .complete_fetch(generation, Ok(Ppn(7)))
        .expect("owner completion should install resident page");
    assert_eq!(snapshot.state, PageSlotState::Resident { ppn: Ppn(7) });
    assert_eq!(
        slot.begin_fetch(),
        PageSlotFetch::Resident {
            ppn: Ppn(7),
            generation,
        }
    );
}

#[test]
fn page_slot_rejects_stale_generation_completion() {
    let slot = PageSlot::new();
    let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
        panic!("first miss must own the fetch");
    };

    slot.invalidate();
    let error = slot
        .complete_fetch(generation, Ok(Ppn(9)))
        .expect_err("stale completion must not publish a frame");

    assert!(matches!(
        error,
        PageSlotCompletionError::GenerationMismatch { completed, .. }
            if completed == generation
    ));
    assert_eq!(slot.snapshot().state, PageSlotState::Empty);
}

#[test]
fn page_slot_tracks_dirty_writeback_and_error_states() {
    let slot = PageSlot::new();
    let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
        panic!("first miss must own the fetch");
    };
    slot.complete_fetch(generation, Ok(Ppn(11)))
        .expect("fetch completion should install");

    let dirty = slot.mark_dirty().expect("resident page can become dirty");
    assert_eq!(dirty.state, PageSlotState::Dirty { ppn: Ppn(11) });

    let writeback = slot
        .begin_writeback()
        .expect("dirty page can enter writeback");
    assert_eq!(
        writeback.state,
        PageSlotState::Writeback {
            ppn: Ppn(11),
            submitted_generation: dirty.generation,
            redirtied: false,
        }
    );

    let completed = slot
        .complete_writeback(writeback.generation, Err(Errno::EIO))
        .expect("matching writeback generation should complete");
    assert_eq!(completed.state, PageSlotState::Error { errno: Errno::EIO });
}

#[test]
fn page_slot_redirty_during_writeback_survives_successful_old_completion() {
    let slot = PageSlot::new();
    let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
        panic!("setup fetch owner");
    };
    slot.complete_fetch(generation, Ok(Ppn(12)))
        .expect("setup resident page");
    let submitted = slot
        .mark_dirty()
        .expect("initial dirty generation")
        .generation;
    let writeback = slot.begin_writeback().expect("start writeback");
    assert_eq!(writeback.generation, submitted);

    let redirtied = slot
        .mark_dirty()
        .expect("writer can redirty writeback page");
    assert_eq!(
        redirtied.state,
        PageSlotState::Writeback {
            ppn: Ppn(12),
            submitted_generation: submitted,
            redirtied: true,
        }
    );

    let completed = slot
        .complete_writeback(submitted, Ok(()))
        .expect("old completion is still valid");
    assert_eq!(completed.state, PageSlotState::Dirty { ppn: Ppn(12) });
    assert_eq!(completed.generation, redirtied.generation);
}

#[test]
fn page_slot_fsync_frontier_waits_for_old_writeback_before_resubmitting_redirty() {
    let slot = PageSlot::new();
    let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
        panic!("setup fetch owner");
    };
    slot.complete_fetch(generation, Ok(Ppn(13)))
        .expect("setup resident page");
    let first = slot
        .mark_dirty()
        .expect("first dirty generation")
        .generation;
    let writeback = slot.begin_writeback().expect("start writeback");
    assert_eq!(writeback.generation, first);
    let second = slot.mark_dirty().expect("redirty generation").generation;

    assert_eq!(
        slot.fsync_status(second),
        PageSlotFsyncStatus::WaitingForEarlierWriteback {
            submitted_generation: first,
            frontier: second,
        }
    );

    slot.complete_writeback(first, Ok(()))
        .expect("old writeback completion");
    assert_eq!(
        slot.fsync_status(second),
        PageSlotFsyncStatus::NeedsWriteback { generation: second }
    );
}

#[test]
fn page_slot_aborts_unsubmitted_writeback_back_to_dirty() {
    let slot = PageSlot::new();
    let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
        panic!("setup fetch owner");
    };
    slot.complete_fetch(generation, Ok(Ppn(14)))
        .expect("setup resident page");
    let dirty = slot.mark_dirty().expect("dirty generation");
    let writeback = slot.begin_writeback().expect("start writeback");

    let restored = slot
        .abort_writeback(writeback.generation)
        .expect("unsubmitted writeback can be aborted");

    assert_eq!(restored.generation, dirty.generation);
    assert_eq!(restored.state, PageSlotState::Dirty { ppn: Ppn(14) });
}
