use super::*;
use crate::execution::Errno;
use crate::io_manager::page::{service::PageCompletionRoute, PageIoCompletion};
use crate::mount::{DevId, MountOptions, MountPayload, MountPayloadPin, SourceLabel};
use crate::page_backed::adapter::step_engine::{
    self as step_engine, NoProgress, PageProgress as V3PageProgress, PlaceholderProcessSubject,
    ScriptCtx, StepOp, StepOutcome as V3Outcome, StepOutcome as V3Out, YieldShape as V3YieldShape,
};
use crate::vfs::{Credential, DirCursor, DirEntry, FsObjectId, InodeKind, InodeMeta};
use alloc::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

fn setup_host_substrate() {
    tx_test_support::init_host();
    crate::zones::register_all().expect("kernel zones");
    match step_engine::page_allocator::claim_zero_frame() {
        Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for PageBacked lifecycle tests: {error:?}"),
    }
}

struct LifecycleFs {
    flushes: AtomicUsize,
    fsyncs: AtomicUsize,
    truncates: AtomicUsize,
    fallocates: AtomicUsize,
    last_object: AtomicU64,
    last_offset: AtomicU64,
    last_truncate_size: AtomicU64,
    last_fallocate_size: AtomicU64,
    block_flush_after: Option<usize>,
    truncate_outcome: V3Outcome<(), NoProgress>,
    truncate_continues_remaining: AtomicUsize,
    fallocate_outcome: V3Outcome<(), NoProgress>,
}

impl LifecycleFs {
    fn new() -> Self {
        Self {
            flushes: AtomicUsize::new(0),
            fsyncs: AtomicUsize::new(0),
            truncates: AtomicUsize::new(0),
            fallocates: AtomicUsize::new(0),
            last_object: AtomicU64::new(0),
            last_offset: AtomicU64::new(0),
            last_truncate_size: AtomicU64::new(0),
            last_fallocate_size: AtomicU64::new(0),
            block_flush_after: None,
            truncate_outcome: V3Outcome::done(()),
            truncate_continues_remaining: AtomicUsize::new(0),
            fallocate_outcome: V3Outcome::done(()),
        }
    }

    fn failing_fallocate(errno: Errno) -> Self {
        Self {
            fallocate_outcome: V3Outcome::err(errno.into()),
            ..Self::new()
        }
    }

    fn blocking_after(first_done_count: usize) -> Self {
        Self {
            block_flush_after: Some(first_done_count),
            ..Self::new()
        }
    }

    fn failing_truncate(errno: Errno) -> Self {
        Self {
            truncate_outcome: V3Outcome::err(errno.into()),
            ..Self::new()
        }
    }

    fn advancing_truncate_once() -> Self {
        Self {
            truncate_continues_remaining: AtomicUsize::new(1),
            ..Self::new()
        }
    }
}

fn file_page_container(fs: Arc<LifecycleFs>, fs_object_id: FsObjectId) -> PageContainer {
    let mount = MountPayload::new_cap(
        fs.clone(),
        fs,
        None,
        DevId::new(8),
        MountOptions::default(),
        "mockfs",
        SourceLabel::Static("mock"),
    )
    .expect("mount payload");
    PageContainer::new(
        PageContainerKind::File {
            mount: MountPayloadPin::acquire(&step_engine::PayloadCap::from_cap(mount)),
            fs_object_id,
        },
        4,
    )
}

fn cached_frame_for_test() -> CachedFrame {
    setup_host_substrate();
    allocate_cached_frame().expect("cached frame")
}

fn seed_dirty_file_page(pc: &PageContainer, page: PageIndex) -> (tx_hal::Ppn, PageGeneration) {
    let frame = cached_frame_for_test();
    let ppn = frame.ppn;
    let mut state = pc.state.lock();
    state
        .install_resident_if_absent(page, frame)
        .expect("seed cached page");
    let slot = state.page_slots.entry(page).or_default();
    let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
        panic!("test must own fetch generation");
    };
    slot.complete_fetch(generation, Ok(ppn))
        .expect("make page resident");
    let dirty = slot.mark_dirty().expect("mark page dirty");
    (ppn, dirty.generation)
}

fn admitted_writeback_request(pc: &PageContainer, range: PageIoRange) -> PageIoRequest {
    pc.page_submission
        .find_submission(pc.io_manager_key(), range, PageIoOp::Writeback)
        .expect("admitted writeback request")
}

#[test]
fn two_page_writeback_admits_one_range_and_retains_both_frames() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x80));
    let first = PageIndex::new(0);
    let second = PageIndex::new(1);
    let (first_ppn, _) = seed_dirty_file_page(&pc, first);
    let (second_ppn, _) = seed_dirty_file_page(&pc, second);

    assert_eq!(pc.queue_dirty_file_writeback(), 2);
    assert_eq!(pc.file_io_request_count_for_test(), 1);
    assert_eq!(pc.file_io_owner_count_for_test(), 1);
    let request = admitted_writeback_request(&pc, PageIoRange::new(0, 2));
    let (source, target) = pc.prepare_owned_file_io_request(&request);
    let IoDataSource::PageCacheSegments { segments, .. } = source else {
        panic!("two-page writeback must project page-cache segments");
    };
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[0].frame.ppn(), first_ppn);
    assert_eq!(segments[1].frame.ppn(), second_ppn);
    assert_eq!(target, IoDataTarget::None);
}

#[test]
fn two_page_writeback_pre_admission_failure_rolls_back_first_page() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x81));
    let first = PageIndex::new(0);
    let missing = PageIndex::new(1);
    let (first_ppn, _) = seed_dirty_file_page(&pc, first);

    assert_eq!(pc.queue_file_writeback_batch(&[first, missing]), None);
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(pc.file_io_request_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(first)
            .expect("first slot")
            .state,
        PageSlotState::Dirty { ppn: first_ppn }
    );
}

#[test]
fn two_page_writeback_submit_failure_rolls_back_both_pages_once() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x82));
    let first = PageIndex::new(0);
    let second = PageIndex::new(1);
    let (first_ppn, _) = seed_dirty_file_page(&pc, first);
    let (second_ppn, _) = seed_dirty_file_page(&pc, second);
    assert_eq!(pc.queue_dirty_file_writeback(), 2);
    let request = admitted_writeback_request(&pc, PageIoRange::new(0, 2));

    assert!(pc
        .finish_owned_file_io_request(&request, FileIoTerminalResult::SubmitFailure)
        .is_some());
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(pc.file_io_lease_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(first).expect("first").state,
        PageSlotState::Dirty { ppn: first_ppn }
    );
    assert_eq!(
        pc.page_slot_snapshot_for_test(second)
            .expect("second")
            .state,
        PageSlotState::Dirty { ppn: second_ppn }
    );
    assert_eq!(
        pc.finish_owned_file_io_request(&request, FileIoTerminalResult::SubmitFailure),
        None
    );
}

#[test]
fn two_page_writeback_success_preserves_redirtied_sibling() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x83));
    let first = PageIndex::new(0);
    let second = PageIndex::new(1);
    let (first_ppn, first_generation) = seed_dirty_file_page(&pc, first);
    let (second_ppn, _) = seed_dirty_file_page(&pc, second);
    assert_eq!(pc.queue_dirty_file_writeback(), 2);
    let request = admitted_writeback_request(&pc, PageIoRange::new(0, 2));
    pc.state
        .lock()
        .page_slots
        .get(&second)
        .expect("second slot")
        .mark_dirty()
        .expect("redirty second page");
    let completion = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Done,
            first_generation,
            PageIoCompletionKind::WritebackFinished,
        ),
        frame: None,
        waiters: alloc::vec![],
    };

    assert!(pc.apply_file_io_completion_route(&completion).is_some());
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(first).expect("first").state,
        PageSlotState::Resident { ppn: first_ppn }
    );
    assert_eq!(
        pc.page_slot_snapshot_for_test(second)
            .expect("second")
            .state,
        PageSlotState::Dirty { ppn: second_ppn }
    );
    assert_eq!(pc.apply_file_io_completion_route(&completion), None);
}

#[test]
fn two_page_writeback_settles_successful_sibling_when_other_slot_is_stale() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x84));
    let first = PageIndex::new(0);
    let second = PageIndex::new(1);
    let (first_ppn, first_generation) = seed_dirty_file_page(&pc, first);
    seed_dirty_file_page(&pc, second);
    assert_eq!(pc.queue_dirty_file_writeback(), 2);
    let request = admitted_writeback_request(&pc, PageIoRange::new(0, 2));
    pc.state
        .lock()
        .page_slots
        .get(&second)
        .expect("second slot")
        .invalidate();
    let completion = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Done,
            first_generation,
            PageIoCompletionKind::WritebackFinished,
        ),
        frame: None,
        waiters: alloc::vec![],
    };

    assert!(matches!(
        pc.apply_file_io_completion_route(&completion),
        Some(Err(PageSlotCompletionError::NotFetching { .. }))
    ));
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(first).expect("first").state,
        PageSlotState::Resident { ppn: first_ppn }
    );
    assert_eq!(
        pc.page_slot_snapshot_for_test(second)
            .expect("second")
            .state,
        PageSlotState::Empty
    );
}

#[test]
fn two_page_writeback_device_error_settles_both_pages() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x85));
    let first = PageIndex::new(0);
    let second = PageIndex::new(1);
    let (_, first_generation) = seed_dirty_file_page(&pc, first);
    seed_dirty_file_page(&pc, second);
    assert_eq!(pc.queue_dirty_file_writeback(), 2);
    let request = admitted_writeback_request(&pc, PageIoRange::new(0, 2));
    let completion = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Err(Errno::EIO),
            first_generation,
            PageIoCompletionKind::WritebackFinished,
        ),
        frame: None,
        waiters: alloc::vec![],
    };

    assert!(pc.apply_file_io_completion_route(&completion).is_some());
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(first).expect("first").state,
        PageSlotState::Error { errno: Errno::EIO }
    );
    assert_eq!(
        pc.page_slot_snapshot_for_test(second)
            .expect("second")
            .state,
        PageSlotState::Error { errno: Errno::EIO }
    );
}

#[test]
fn writeback_completion_with_read_kind_rolls_back_before_consuming_owner() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x86));
    let page = PageIndex::new(0);
    let (ppn, generation) = seed_dirty_file_page(&pc, page);
    let request_id = pc.queue_file_page_writeback(page).expect("admit writeback");
    let request = admitted_writeback_request(&pc, PageIoRange::new(0, 1));
    assert_eq!(request.id, request_id);
    let wrong_kind = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Done,
            generation,
            PageIoCompletionKind::ReadInstalled,
        ),
        frame: None,
        waiters: alloc::vec![],
    };

    assert!(pc.apply_file_io_completion_route(&wrong_kind).is_some());
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(page)
            .expect("writeback slot")
            .state,
        PageSlotState::Dirty { ppn }
    );
}

#[test]
fn owned_request_submit_failure_restores_writeback_and_releases_owner() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x72));
    let page = PageIndex::new(0);
    let frame = cached_frame_for_test();
    let ppn = frame.ppn;
    let generation = {
        let mut state = pc.state.lock();
        state
            .install_resident_if_absent(page, frame)
            .expect("seed cached page");
        let generation = {
            let slot = state.page_slots.entry(page).or_default();
            let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
                panic!("test must own fetch generation");
            };
            slot.complete_fetch(generation, Ok(ppn))
                .expect("make page resident");
            slot.mark_dirty().expect("mark page dirty");
            slot.generation()
        };
        generation
    };
    let request_id = pc.queue_file_page_writeback(page).expect("admit writeback");
    let request = pc
        .state
        .lock()
        .file_io_service
        .find_submission(
            pc.io_manager_key(),
            PageIoRange::new(page.as_u64(), 1),
            PageIoOp::Writeback,
        )
        .filter(|request| request.id == request_id)
        .expect("admitted writeback request");

    let (source, target) = pc.prepare_owned_file_io_request(&request);
    assert!(matches!(source, IoDataSource::PageCache { frame, .. } if frame.ppn() == ppn));
    assert_eq!(target, IoDataTarget::None);
    assert_eq!(pc.file_io_owner_count_for_test(), 1);

    pc.finish_owned_file_io_request(&request, FileIoTerminalResult::SubmitFailure);

    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(pc.file_io_lease_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(page)
            .expect("writeback slot")
            .state,
        PageSlotState::Dirty { ppn }
    );
    assert!(!pc.page_marks(page).expect("page marks").writeback);

    let duplicate = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Done,
            generation,
            PageIoCompletionKind::WritebackFinished,
        ),
        frame: None,
        waiters: alloc::vec![],
    };
    assert_eq!(pc.apply_file_io_completion_route(&duplicate), None);
    assert_eq!(
        pc.page_slot_snapshot_for_test(page)
            .expect("writeback slot")
            .state,
        PageSlotState::Dirty { ppn }
    );
}

#[test]
fn duplicate_writeback_completion_has_no_second_terminal_action() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x74));
    let page = PageIndex::new(0);
    let frame = cached_frame_for_test();
    let ppn = frame.ppn;
    let generation = {
        let mut state = pc.state.lock();
        state
            .install_resident_if_absent(page, frame)
            .expect("seed cached page");
        let generation = {
            let slot = state.page_slots.entry(page).or_default();
            let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
                panic!("test must own fetch generation");
            };
            slot.complete_fetch(generation, Ok(ppn))
                .expect("make page resident");
            slot.mark_dirty().expect("mark page dirty");
            slot.generation()
        };
        generation
    };
    let request_id = pc.queue_file_page_writeback(page).expect("admit writeback");
    let request = pc
        .state
        .lock()
        .file_io_service
        .find_submission(
            pc.io_manager_key(),
            PageIoRange::new(page.as_u64(), 1),
            PageIoOp::Writeback,
        )
        .filter(|request| request.id == request_id)
        .expect("admitted writeback request");
    let completion = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Done,
            generation,
            PageIoCompletionKind::WritebackFinished,
        ),
        frame: None,
        waiters: alloc::vec![],
    };

    assert!(pc.apply_file_io_completion_route(&completion).is_some());
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(page)
            .expect("writeback slot")
            .state,
        PageSlotState::Resident { ppn }
    );
    assert_eq!(pc.apply_file_io_completion_route(&completion), None);
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(page)
            .expect("writeback slot")
            .state,
        PageSlotState::Resident { ppn }
    );
}

#[test]
fn control_only_writeback_completion_cannot_settle_page_slot() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x73));
    let page = PageIndex::new(0);
    let frame = cached_frame_for_test();
    let ppn = frame.ppn;
    let generation = {
        let mut state = pc.state.lock();
        state
            .install_resident_if_absent(page, frame)
            .expect("seed cached page");
        let slot = state.page_slots.entry(page).or_default();
        let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
            panic!("test must own fetch generation");
        };
        slot.complete_fetch(generation, Ok(ppn))
            .expect("make page resident");
        slot.mark_dirty().expect("mark page dirty");
        let writeback = slot.begin_writeback().expect("begin writeback");
        writeback.generation
    };
    let request = PageIoRequest::new(
        PageIoRequestId::new(0x74),
        pc.io_manager_key(),
        PageIoRange::new(page.as_u64(), 1),
        PageIoOp::Writeback,
        PageIoPriority::BackgroundWriteback,
        PageIoFlags::WRITEBACK,
        Some(generation),
    );
    assert!(pc
        .page_submission
        .admit_file_request(OwnedFileIoRequest::control(request.clone())));
    let completion = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Done,
            generation,
            PageIoCompletionKind::WritebackFinished,
        ),
        frame: None,
        waiters: alloc::vec![],
    };

    assert_eq!(pc.apply_file_io_completion_route(&completion), None);
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(page)
            .expect("writeback slot")
            .state,
        PageSlotState::Writeback {
            ppn,
            submitted_generation: generation,
            redirtied: false,
        }
    );
    assert!(pc.page_marks(page).expect("page marks").dirty);
    assert!(pc.page_marks(page).expect("page marks").writeback);
}

#[test]
fn stale_writeback_completion_consumes_owner_without_mutating_slot() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let pc = file_page_container(Arc::new(LifecycleFs::new()), FsObjectId::new(0x73));
    let page = PageIndex::new(0);
    let frame = cached_frame_for_test();
    let ppn = frame.ppn;
    let generation = {
        let mut state = pc.state.lock();
        state
            .install_resident_if_absent(page, frame)
            .expect("seed cached page");
        let generation = {
            let slot = state.page_slots.entry(page).or_default();
            let PageSlotFetch::Owner { generation } = slot.begin_fetch() else {
                panic!("test must own fetch generation");
            };
            slot.complete_fetch(generation, Ok(ppn))
                .expect("make page resident");
            slot.mark_dirty().expect("mark page dirty");
            slot.generation()
        };
        generation
    };
    let request_id = pc.queue_file_page_writeback(page).expect("admit writeback");
    let request = pc
        .state
        .lock()
        .file_io_service
        .find_submission(
            pc.io_manager_key(),
            PageIoRange::new(page.as_u64(), 1),
            PageIoOp::Writeback,
        )
        .filter(|request| request.id == request_id)
        .expect("admitted writeback request");
    let stale = PageCompletionRoute {
        completion: PageIoCompletion::new(
            request.id,
            request.range,
            PageIoResult::Done,
            PageGeneration::new(generation.raw().saturating_add(1)),
            PageIoCompletionKind::WritebackFinished,
        ),
        frame: None,
        waiters: alloc::vec![],
    };

    assert!(matches!(
        pc.apply_file_io_completion_route(&stale),
        Some(Err(PageSlotCompletionError::GenerationMismatch { .. }))
    ));
    assert_eq!(pc.file_io_owner_count_for_test(), 0);
    assert_eq!(pc.file_io_lease_count_for_test(), 0);
    assert_eq!(
        pc.page_slot_snapshot_for_test(page)
            .expect("writeback slot")
            .state,
        PageSlotState::Writeback {
            ppn,
            submitted_generation: generation,
            redirtied: false,
        }
    );
}

#[test]
fn pagebacked_step_truncate_withdraws_pages_at_or_beyond_new_size() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );
    for page in 0..4 {
        assert!(pc
            .install_resident_if_absent_published(PageIndex::new(page), cached_frame_for_test())
            .expect("publish page"));
    }

    assert_eq!(
        step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64 + 1, &guard),
        V3Out::Done(())
    );

    assert_eq!(pc.size_bytes(), crate::vm::USER_PAGE_SIZE as u64 + 1);
    assert!(pc.lookup(PageIndex::new(0)).is_some());
    assert!(pc.lookup(PageIndex::new(1)).is_some());
    assert_eq!(pc.lookup(PageIndex::new(2)), None);
    assert_eq!(pc.lookup(PageIndex::new(3)), None);
}

#[test]
fn pagebacked_step_truncate_can_grow_visible_size_without_materializing_pages() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );
    assert_eq!(step_truncate(&pc, 8, &guard), V3Out::Done(()));

    assert_eq!(
        step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64 + 11, &guard),
        V3Out::Done(())
    );

    assert_eq!(pc.size_bytes(), 2 * crate::vm::USER_PAGE_SIZE as u64 + 11);
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn pagebacked_step_truncate_asks_file_backing_before_withdrawal() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(44));
    assert!(pc
        .install_resident_if_absent_published(PageIndex::new(3), cached_frame_for_test())
        .expect("publish page"));

    assert_eq!(
        step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        V3Out::Done(())
    );

    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_object.load(Ordering::Acquire), 44);
    assert_eq!(
        fs.last_truncate_size.load(Ordering::Acquire),
        2 * crate::vm::USER_PAGE_SIZE as u64
    );
    assert_eq!(pc.lookup(PageIndex::new(3)), None);
}

#[test]
fn pagebacked_truncate_op_retries_only_commit_after_root_backpressure() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(0x4a));
    let page = PageIndex::new(3);
    assert!(pc
        .install_resident_if_absent_published(page, cached_frame_for_test())
        .expect("publish page"));
    let original_ppn = pc.lookup(page).expect("resident compatibility row");
    let original_size = pc.size_bytes();
    let original_slot = pc.page_slot_snapshot_for_test(page).expect("resident slot");
    let new_size = 2 * crate::vm::USER_PAGE_SIZE as u64;
    let mut op = TruncateOp::new(&pc, new_size);
    let mut ctx = ScriptCtx::<PlaceholderProcessSubject>::new();

    PageContainer::force_resident_retire_backpressure_for_test();
    assert_eq!(
        op.step(&mut ctx),
        V3Out::Continue {
            progress: V3PageProgress::EMPTY
        }
    );
    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);
    assert_eq!(pc.size_bytes(), original_size);
    assert_eq!(pc.lookup(page), Some(original_ppn));
    assert_eq!(
        pc.page_slot_snapshot_for_test(page).expect("resident slot"),
        original_slot,
        "batch root prepare must leave the slot unchanged on EAGAIN"
    );
    let guard = step_engine::guard();
    assert_eq!(
        pc.lookup_resident_with_guard_for_test(&guard, page)
            .expect("old root binding")
            .ppn(),
        original_ppn,
        "batch root prepare must leave the published root unchanged on EAGAIN"
    );
    drop(guard);

    assert_eq!(op.step(&mut ctx), V3Out::Done(()));
    assert_eq!(
        fs.truncates.load(Ordering::Acquire),
        1,
        "commit retry must not replay the completed filesystem truncate"
    );
    assert_eq!(pc.size_bytes(), new_size);
    assert_eq!(pc.lookup(page), None);
}

#[test]
fn pagebacked_step_truncate_leaves_state_unchanged_when_file_backing_fails() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::failing_truncate(Errno::EROFS));
    let pc = file_page_container(fs.clone(), FsObjectId::new(45));
    assert!(pc
        .install_resident_if_absent_published(PageIndex::new(3), cached_frame_for_test())
        .expect("publish page"));
    let original_size = pc.size_bytes();

    assert_eq!(
        step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
        V3Out::Err(Errno::EROFS.into())
    );

    assert_eq!(pc.size_bytes(), original_size);
    assert!(pc.lookup(PageIndex::new(3)).is_some());
    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);
}

#[test]
fn pagebacked_step_truncate_rejects_device_and_capacity_growth() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_1000),
            page_count: 1,
        },
        1,
    );
    let anon = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );

    assert_eq!(
        step_truncate(&device, 0, &guard),
        V3Out::Err(Errno::EINVAL.into())
    );
    assert_eq!(
        step_truncate(&anon, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        V3Out::Err(Errno::EINVAL.into())
    );
}

#[test]
fn close_visibility_publishes_exact_eof_without_flushing_page_data() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(0x54));
    pc.set_size_bytes_persisted(0);
    pc.set_size_bytes(83);
    seed_dirty_file_page(&pc, PageIndex::new(0));

    assert_eq!(
        pc.publish_exact_size_for_close_visibility(&guard),
        V3Out::Done(())
    );
    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_truncate_size.load(Ordering::Acquire), 83);
    assert_eq!(
        fs.flushes.load(Ordering::Acquire),
        0,
        "close-time EOF publication must leave page data on the async path"
    );
    assert!(pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
}

#[test]
fn pagebacked_step_fsync_flushes_dirty_file_pages_in_order_and_cleans_marks() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(51));
    for page in [2, 0] {
        let mut state = pc.state.lock();
        let page = PageIndex::new(page);
        let frame = cached_frame_for_test();
        let ppn = frame.ppn;
        state
            .install_resident_if_absent(page, frame)
            .expect("seed page");
        state
            .ensure_resident_page_slot(page, ppn)
            .expect("install resident slot");
        state
            .page_slots
            .get(&page)
            .expect("resident slot")
            .mark_dirty()
            .expect("mark dirty");
    }

    assert_eq!(step_fsync(&pc, &guard), V3Out::Done(()));

    assert_eq!(fs.flushes.load(Ordering::Acquire), 2);
    assert_eq!(fs.fsyncs.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_object.load(Ordering::Acquire), 51);
    assert_eq!(
        fs.last_offset.load(Ordering::Acquire),
        2 * crate::vm::USER_PAGE_SIZE as u64
    );
    assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(!pc.page_marks(PageIndex::new(2)).expect("page 2").dirty);
}

#[test]
fn pagebacked_step_fsync_retries_exact_eof_after_truncate_advances() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::advancing_truncate_once());
    let pc = file_page_container(fs.clone(), FsObjectId::new(0x53));
    pc.set_size_bytes_persisted(80);
    seed_dirty_file_page(&pc, PageIndex::new(0));

    assert_eq!(
        step_fsync(&pc, &guard),
        V3Out::Continue {
            progress: V3PageProgress::EMPTY,
        }
    );
    assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert_eq!(fs.flushes.load(Ordering::Acquire), 1);
    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);
    assert_eq!(fs.fsyncs.load(Ordering::Acquire), 0);

    assert_eq!(step_fsync(&pc, &guard), V3Out::Done(()));
    assert_eq!(
        fs.truncates.load(Ordering::Acquire),
        2,
        "the exact EOF correction must survive after the page becomes clean"
    );
    assert_eq!(fs.last_truncate_size.load(Ordering::Acquire), 80);
    assert_eq!(fs.fsyncs.load(Ordering::Acquire), 1);
}

#[test]
fn pagebacked_step_fsync_returns_advanced_then_blocked_after_flush_progress() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::blocking_after(1));
    let pc = file_page_container(fs.clone(), FsObjectId::new(52));
    for page in 0..2 {
        let mut state = pc.state.lock();
        let page = PageIndex::new(page);
        let frame = cached_frame_for_test();
        let ppn = frame.ppn;
        state
            .install_resident_if_absent(page, frame)
            .expect("seed page");
        state
            .ensure_resident_page_slot(page, ppn)
            .expect("install resident slot");
        state
            .page_slots
            .get(&page)
            .expect("resident slot")
            .mark_dirty()
            .expect("mark dirty");
    }

    assert_eq!(
        step_fsync(&pc, &guard),
        V3Out::Yield {
            progress: V3PageProgress::new(1),
            shape: V3YieldShape::on_wait_source(13, 0x55),
        }
    );

    assert!(!pc.page_marks(PageIndex::new(0)).expect("page 0").dirty);
    assert!(pc.page_marks(PageIndex::new(1)).expect("page 1").dirty);
    assert_eq!(fs.fsyncs.load(Ordering::Acquire), 0);
}

#[test]
fn pagebacked_step_fsync_is_noop_for_anon_and_device() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let anon = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_2000),
            page_count: 1,
        },
        1,
    );

    assert_eq!(step_fsync(&anon, &guard), V3Out::Done(()));
    assert_eq!(step_fsync(&device, &guard), V3Out::Done(()));
}

#[test]
fn pagebacked_step_truncate_zeros_partial_eof_tail_in_cached_page() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );
    for page in 0..4 {
        assert!(pc
            .install_resident_if_absent_published(PageIndex::new(page), cached_frame_for_test())
            .expect("publish page"));
    }
    let ppn_page1 = pc.lookup(PageIndex::new(1)).expect("page 1 cached");

    let pattern: alloc::vec::Vec<u8> = (0..crate::vm::USER_PAGE_SIZE)
        .map(|i| ((i & 0xff) | 0x40) as u8)
        .collect();
    step_engine::page_allocator::testing::write_frame_bytes_for_test(ppn_page1, 0, &pattern);

    let new_size = crate::vm::USER_PAGE_SIZE as u64 + 4;
    assert_eq!(step_truncate(&pc, new_size, &guard), V3Out::Done(()));

    let mut head = [0u8; 4];
    step_engine::page_allocator::testing::read_frame_bytes_for_test(ppn_page1, 0, &mut head);
    assert_eq!(&head, &pattern[..4]);

    let mut tail = alloc::vec![0xCCu8; crate::vm::USER_PAGE_SIZE - 4];
    step_engine::page_allocator::testing::read_frame_bytes_for_test(ppn_page1, 4, &mut tail);
    assert!(
        tail.iter().all(|b| *b == 0),
        "tail bytes must be zeroed after truncate-shrink past mid-page"
    );
}

#[test]
fn pagebacked_step_truncate_does_not_touch_surviving_pages_at_page_aligned_shrink() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );
    for page in 0..4 {
        assert!(pc
            .install_resident_if_absent_published(PageIndex::new(page), cached_frame_for_test())
            .expect("publish page"));
    }
    let ppn_page0 = pc.lookup(PageIndex::new(0)).expect("page 0 cached");
    let pattern = alloc::vec![0xAFu8; crate::vm::USER_PAGE_SIZE];
    step_engine::page_allocator::testing::write_frame_bytes_for_test(ppn_page0, 0, &pattern);

    assert_eq!(
        step_truncate(&pc, crate::vm::USER_PAGE_SIZE as u64, &guard),
        V3Out::Done(())
    );

    let mut readback = alloc::vec![0u8; crate::vm::USER_PAGE_SIZE];
    step_engine::page_allocator::testing::read_frame_bytes_for_test(ppn_page0, 0, &mut readback);
    assert_eq!(readback, pattern);
}

#[test]
fn pagebacked_step_fallocate_grows_anon_visible_size_without_materializing_pages() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        4,
    );
    assert_eq!(step_truncate(&pc, 8, &guard), V3Out::Done(()));
    assert_eq!(pc.size_bytes(), 8);

    assert_eq!(
        step_fallocate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64 + 17, &guard),
        V3Out::Done(())
    );

    assert_eq!(pc.size_bytes(), 2 * crate::vm::USER_PAGE_SIZE as u64 + 17);
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn pagebacked_step_fallocate_calls_file_backing_before_publishing_size() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(91));
    assert_eq!(step_truncate(&pc, 16, &guard), V3Out::Done(()));
    assert_eq!(fs.truncates.load(Ordering::Acquire), 1);

    let new_size = 3 * crate::vm::USER_PAGE_SIZE as u64;
    assert_eq!(step_fallocate(&pc, new_size, &guard), V3Out::Done(()));

    assert_eq!(fs.fallocates.load(Ordering::Acquire), 1);
    assert_eq!(fs.last_fallocate_size.load(Ordering::Acquire), new_size);
    assert_eq!(fs.last_object.load(Ordering::Acquire), 91);
    assert_eq!(pc.size_bytes(), new_size);
    assert_eq!(pc.resident_pages(), 0);
}

#[test]
fn pagebacked_step_fallocate_leaves_state_unchanged_when_file_backing_fails() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::failing_fallocate(Errno::EDQUOT));
    let pc = file_page_container(fs.clone(), FsObjectId::new(92));
    assert_eq!(step_truncate(&pc, 16, &guard), V3Out::Done(()));
    let baseline_size = pc.size_bytes();

    assert_eq!(
        step_fallocate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        V3Out::Err(Errno::EDQUOT.into())
    );

    assert_eq!(fs.fallocates.load(Ordering::Acquire), 1);
    assert_eq!(pc.size_bytes(), baseline_size);
}

#[test]
fn pagebacked_step_fallocate_rejects_device_and_capacity_growth() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let device = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_3000),
            page_count: 1,
        },
        1,
    );

    assert_eq!(
        step_fallocate(&device, 16, &guard),
        V3Out::Err(Errno::EINVAL.into())
    );

    let anon = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        2,
    );
    let beyond = 3 * crate::vm::USER_PAGE_SIZE as u64;
    assert_eq!(
        step_fallocate(&anon, beyond, &guard),
        V3Out::Err(Errno::EINVAL.into())
    );
    assert_eq!(anon.size_bytes(), 2 * crate::vm::USER_PAGE_SIZE as u64);
}

#[test]
fn pagebacked_step_fallocate_is_noop_when_target_size_does_not_grow() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = Arc::new(LifecycleFs::new());
    let pc = file_page_container(fs.clone(), FsObjectId::new(93));
    assert_eq!(
        step_truncate(&pc, 2 * crate::vm::USER_PAGE_SIZE as u64, &guard),
        V3Out::Done(())
    );
    let stable_size = pc.size_bytes();

    assert_eq!(step_fallocate(&pc, stable_size, &guard), V3Out::Done(()));
    assert_eq!(
        step_fallocate(&pc, stable_size - 1, &guard),
        V3Out::Done(())
    );

    assert_eq!(fs.fallocates.load(Ordering::Acquire), 0);
    assert_eq!(pc.size_bytes(), stable_size);
}

// === FsOps impl + tests ==============================================
//
// `LifecycleFs` is a small test-only `FsOps` impl that lives next to
// `step_truncate`/`step_fsync` so the trait surface is validated
// against the same fixture exercising those step fns.
//
// Every method picks `NoProgress`: the trait surface is one-shot
// identity-side queries / mutations; page accounting belongs to
// `FsPageBacking`.

use crate::page_backed::adapter::step_engine::Errno as V3Errno;
use crate::vfs::FsOps;

impl FsOps for LifecycleFs {
    fn lookup(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Outcome<FsObjectId, NoProgress> {
        V3Outcome::err(V3Errno::ENOSYS)
    }

    fn load_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<InodeMeta, NoProgress> {
        V3Outcome::done(InodeMeta::new(InodeKind::Regular, 0o100644))
    }

    fn serialize_inode_meta(
        &self,
        _fs_object_id: FsObjectId,
        _meta: &InodeMeta,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::done(())
    }

    fn create_inode(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
        V3Outcome::err(V3Errno::EROFS)
    }

    fn unlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(V3Errno::EROFS)
    }

    fn rename(
        &self,
        _old_parent: FsObjectId,
        _old_name: &[u8],
        _new_parent: FsObjectId,
        _new_name: &[u8],
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(V3Errno::EROFS)
    }

    fn link(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(V3Errno::EROFS)
    }

    fn mkdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _mode: u16,
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
        V3Outcome::err(V3Errno::EROFS)
    }

    fn rmdir(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _target: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::err(V3Errno::EROFS)
    }

    fn symlink(
        &self,
        _parent: FsObjectId,
        _name: &[u8],
        _link_target: &[u8],
        _cred: &Credential,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(FsObjectId, InodeMeta), NoProgress> {
        V3Outcome::err(V3Errno::EROFS)
    }

    fn readdir(
        &self,
        _fs_object_id: FsObjectId,
        _cursor: DirCursor,
        _guard: &Guard<'_>,
    ) -> V3Outcome<Option<(DirEntry, DirCursor)>, NoProgress> {
        V3Outcome::done(None)
    }

    fn destroy_inode(
        &self,
        _fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        V3Outcome::done(())
    }
}

// `FsPageBacking` impl so `MountPayload` can hold a `LifecycleFs`
// for the page-backing slot. Anything that would be `Blocked` becomes
// `Err(EAGAIN)`.
impl crate::page_backed::FsPageBacking for LifecycleFs {
    fn fetch_page(
        &self,
        _fs_object_id: FsObjectId,
        _offset: u64,
        _guard: &Guard<'_>,
    ) -> V3Outcome<Frame, NoProgress> {
        V3Outcome::done(Frame::new(
            step_engine::page_allocator::zero_frame_ppn().expect("zero frame"),
        ))
    }

    fn flush_page(
        &self,
        fs_object_id: FsObjectId,
        offset: u64,
        _frame: &Frame,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        let flush = self.flushes.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        self.last_offset.store(offset, Ordering::Release);
        if self.block_flush_after == Some(flush) {
            // Surface the wait carrier/interest pair so `step_fsync`
            // (which consumes `FsPageBacking`) can map this back to
            // the appropriate yield shape with the carrier/interest
            // values the fixture assertions check against.
            V3Outcome::yield_on_wait_source(NoProgress, 13, 0x55)
        } else {
            V3Outcome::done(())
        }
    }

    fn truncate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        self.truncates.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        self.last_truncate_size.store(new_size, Ordering::Release);
        if self
            .truncate_continues_remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return V3Outcome::continue_with(NoProgress);
        }
        self.truncate_outcome
    }

    fn fsync_file(
        &self,
        fs_object_id: FsObjectId,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        self.fsyncs.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        V3Outcome::done(())
    }

    fn fallocate(
        &self,
        fs_object_id: FsObjectId,
        new_size: u64,
        _guard: &Guard<'_>,
    ) -> V3Outcome<(), NoProgress> {
        self.fallocates.fetch_add(1, Ordering::AcqRel);
        self.last_object
            .store(fs_object_id.as_u64(), Ordering::Release);
        self.last_fallocate_size.store(new_size, Ordering::Release);
        self.fallocate_outcome
    }
}

// Tests pin the v3 outcome shape end-to-end through the `LifecycleFs`
// impl. They are red until both the `FsOps` trait declaration in
// `crates/tx-subsystems/src/vfs/execution.rs` and the `impl FsOps
// for LifecycleFs` block above are present.

#[test]
fn fsopsv3_load_inode_meta_returns_done_with_default_meta() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOps>::load_inode_meta(&fs, FsObjectId::new(7), &guard);
    let expected = InodeMeta::new(InodeKind::Regular, 0o100644);
    assert_eq!(outcome, V3Outcome::done(expected));
}

#[test]
fn fsopsv3_create_inode_returns_err_erofs_on_readonly_fixture() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOps>::create_inode(
        &fs,
        FsObjectId::new(1),
        b"foo",
        0o100644,
        &Credential::root(),
        &guard,
    );
    assert_eq!(
        outcome,
        V3Outcome::<(FsObjectId, InodeMeta), NoProgress>::err(V3Errno::EROFS)
    );
}

#[test]
fn fsopsv3_readdir_done_none_for_empty_directory() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = LifecycleFs::new();
    let outcome =
        <LifecycleFs as FsOps>::readdir(&fs, FsObjectId::new(1), DirCursor::START, &guard);
    assert_eq!(outcome, V3Outcome::done(None));
}

#[test]
fn fsopsv3_lookup_returns_err_enosys() {
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOps>::lookup(&fs, FsObjectId::new(1), b"missing", &guard);
    assert_eq!(
        outcome,
        V3Outcome::<FsObjectId, NoProgress>::err(V3Errno::ENOSYS)
    );
}

#[test]
fn fsopsv3_default_read_link_returns_enosys() {
    // Wave-8 design choice: defaults match `FsOps` exactly. `LifecycleFs`
    // does not override `read_link`, so the default `ENOSYS` answer
    // must round-trip through the v3 outcome shape.
    let _lock = EPOCH_TEST_LOCK
        .lock()
        .expect("page-backed lifecycle test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let fs = LifecycleFs::new();
    let outcome = <LifecycleFs as FsOps>::read_link(&fs, FsObjectId::new(1), &guard);
    assert_eq!(
        outcome,
        V3Outcome::<alloc::boxed::Box<[u8]>, NoProgress>::err(V3Errno::ENOSYS)
    );
}
