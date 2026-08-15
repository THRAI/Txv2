use super::*;

fn setup_host_substrate() {
    tx_test_support::init_host();
    crate::zones::register_all().expect("kernel zones");
    match step_engine::page_allocator::claim_zero_frame() {
        Ok(_) | Err(step_engine::page_allocator::AllocError::AlreadyInstalled) => {}
        Err(error) => panic!("claim zero frame for resident cell tests: {error:?}"),
    }
}

#[test]
fn private_anon_pool_preallocation_publishes_one_complete_resident_root() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Persistent,
        },
        32,
    );
    let guard = step_engine::guard();

    pc.materialize_private_anon_pages_batch()
        .expect("batch preallocate private anonymous pool");

    for raw_page in 0..32 {
        let page = PageIndex::new(raw_page);
        assert!(
            pc.lookup_resident_with_guard(&guard, page).is_some(),
            "the batch publication must contain page {raw_page}"
        );
        assert!(matches!(
            pc.export_page_lease(page, &guard),
            StepOutcome::Done(_)
        ));
    }
}

#[test]
fn resident_publication_maintains_retire_credit_across_large_install_batch() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let page_count = 96;
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_0000),
            page_count,
        },
        page_count,
    );

    for raw_page in 0..page_count {
        let guard = step_engine::guard();
        pc.materialize_page_now(PageIndex::new(raw_page), MaterializeAccess::Read, &guard)
            .expect("a large sequential install must replenish retire credit");
    }
    assert_eq!(pc.resident_pages(), page_count as usize);
}

#[test]
fn resident_radix_depth_tracks_the_highest_significant_page_nibble() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xfac0_0000),
            page_count: 17,
        },
        17,
    );

    pc.publish_device_resident_for_test(PageIndex::new(0), Ppn(0xfac0_0000))
        .expect("publish low page");
    pc.publish_device_resident_for_test(PageIndex::new(15), Ppn(0xfac0_000f))
        .expect("publish last one-nibble page");
    {
        let guard = step_engine::guard();
        let root = pc.resident.read(&guard);
        assert_eq!(root.levels(), 1);
        assert!(
            root.lookup(PageIndex::new(16)).is_none(),
            "a key outside the published depth must miss instead of aliasing page zero"
        );
    }

    pc.publish_device_resident_for_test(PageIndex::new(16), Ppn(0xfac0_0010))
        .expect("publish first two-nibble page");
    let guard = step_engine::guard();
    let root = pc.resident.read(&guard);
    assert_eq!(root.levels(), 2);
    assert!(root.lookup(PageIndex::new(0)).is_some());
    assert!(root.lookup(PageIndex::new(15)).is_some());
    assert!(root.lookup(PageIndex::new(16)).is_some());

    let shrunk = root.clone().without_cell(PageIndex::new(16));
    assert_eq!(shrunk.levels(), 1);
    assert!(shrunk.lookup(PageIndex::new(0)).is_some());
    assert!(shrunk.lookup(PageIndex::new(15)).is_some());
    assert!(shrunk.lookup(PageIndex::new(16)).is_none());
}

#[test]
fn resident_radix_never_aliases_a_higher_nibble_at_depth_boundaries() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xfac1_0000),
            page_count: 257,
        },
        257,
    );

    pc.publish_device_resident_for_test(PageIndex::new(0), Ppn(0xfac1_0000))
        .expect("publish page zero");
    let guard = step_engine::guard();
    let root = pc.resident.read(&guard);
    assert!(root.lookup(PageIndex::new(16)).is_none());
    assert!(root.lookup(PageIndex::new(256)).is_none());
    drop(guard);

    pc.publish_device_resident_for_test(PageIndex::new(16), Ppn(0xfac1_0010))
        .expect("publish first two-nibble page");
    let guard = step_engine::guard();
    let root = pc.resident.read(&guard);
    assert_eq!(
        root.lookup(PageIndex::new(0)).map(ResidentCell::ppn),
        Some(Ppn(0xfac1_0000))
    );
    assert_eq!(
        root.lookup(PageIndex::new(16)).map(ResidentCell::ppn),
        Some(Ppn(0xfac1_0010))
    );
    assert!(root.lookup(PageIndex::new(256)).is_none());
}

#[test]
fn resident_hit_baseline_takes_pagecontainer_state_lock() {
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0),
            page_count: 1,
        },
        1,
    );
    let page = PageIndex::new(0);

    pc.state
        .lock()
        .install_resident_if_absent(
            page,
            CachedFrame {
                ppn: Ppn(0),
                pin: PageCachePin::Device(DeviceFrame::new(Ppn(0))),
            },
        )
        .expect("seed resident device entry");
    reset_page_container_lock_service_observations_for_test();

    assert_eq!(pc.lookup(page), Some(Ppn(0)));
    assert!(
        page_container_state_lock_acquisitions_for_test() > 0,
        "the staged resident hit must take PageContainerState before the RCU cutover"
    );
}

#[test]
fn resident_root_replacement_preserves_old_guard_binding() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_8000),
            page_count: 1,
        },
        1,
    );
    let page = PageIndex::new(0);
    let old_ppn = Ppn(0xface_8000);
    let replacement_ppn = Ppn(0xface_9000);
    pc.publish_device_resident_for_test(page, old_ppn)
        .expect("publish initial resident root");

    let old_guard = step_engine::guard();
    let old = pc
        .lookup_resident_with_guard_for_test(&old_guard, page)
        .expect("old root binding");
    pc.try_replace_resident_for_test(page, replacement_ppn)
        .expect("publish replacement root");

    assert_eq!(old.ppn(), old_ppn, "old guard keeps the old root binding");
    let new_guard = step_engine::borrow_current_guard().expect("active epoch guard");
    assert_eq!(
        pc.lookup_resident_with_guard_for_test(&new_guard, page)
            .expect("replacement root binding")
            .ppn(),
        replacement_ppn,
        "new guard observes the replacement binding"
    );
}

#[test]
fn resident_root_backpressure_leaves_old_root_published() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_a000),
            page_count: 1,
        },
        1,
    );
    let page = PageIndex::new(0);
    let old_ppn = Ppn(0xface_a000);
    pc.publish_device_resident_for_test(page, old_ppn)
        .expect("publish initial resident root");
    let before = pc.page_slot_snapshot_for_test(page).expect("resident slot");

    PageContainer::force_resident_retire_backpressure_for_test();
    assert!(pc
        .try_replace_resident_for_test(page, Ppn(0xface_b000))
        .is_err());

    let guard = step_engine::guard();
    assert_eq!(
        pc.lookup_resident_with_guard_for_test(&guard, page)
            .expect("old root remains visible")
            .ppn(),
        old_ppn
    );
    assert_eq!(pc.lookup(page), Some(old_ppn));
    assert_eq!(
        pc.page_slot_snapshot_for_test(page).expect("resident slot"),
        before,
        "failed publication does not advance the resident slot"
    );
}

#[test]
fn guarded_resident_hit_avoids_pagecontainer_state_lock() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_c000),
            page_count: 1,
        },
        1,
    );
    let page = PageIndex::new(0);
    let ppn = Ppn(0xface_c000);
    pc.publish_device_resident_for_test(page, ppn)
        .expect("publish resident root");

    reset_page_container_lock_service_observations_for_test();
    crate::io_manager::page::manager::reset_page_io_submission_manager_lock_acquisitions_for_test();
    super::block_runtime::reset_block_submission_manager_lock_acquisitions_for_test();
    let guard = step_engine::guard();
    assert_eq!(
        pc.materialize_page_now(page, MaterializeAccess::Read, &guard)
            .expect("guarded resident materialization")
            .ppn,
        ppn
    );
    assert_eq!(
        page_container_state_lock_acquisitions_for_test(),
        0,
        "guarded resident hits do not enter PageContainerState"
    );
    assert_eq!(
        crate::io_manager::page::manager::page_io_submission_manager_lock_acquisitions_for_test(),
        0,
        "guarded resident hits do not enter the L4 submission manager"
    );
    assert_eq!(
        super::block_runtime::block_submission_manager_lock_acquisitions_for_test(),
        0,
        "guarded resident hits do not enter the L6 submission manager"
    );
}

#[test]
fn device_materialization_publishes_a_guarded_read_hit() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident root test lock");
    setup_host_substrate();
    let ppn = Ppn(0xface_d000);
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: ppn,
            page_count: 1,
        },
        1,
    );
    let page = PageIndex::new(0);
    let guard = step_engine::guard();

    let first = pc
        .materialize_page_now(page, MaterializeAccess::Read, &guard)
        .expect("install and publish device resident page");
    assert!(first.newly_installed);
    drop(first);

    reset_page_container_lock_service_observations_for_test();
    let second = pc
        .materialize_page_now(page, MaterializeAccess::Read, &guard)
        .expect("guarded resident hit after installation");
    assert_eq!(second.ppn, ppn);
    assert!(!second.newly_installed);
    assert_eq!(page_container_state_lock_acquisitions_for_test(), 0);
}

#[test]
fn resident_cell_keeps_one_binding_pin_while_leases_come_and_go() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident cell test lock");
    setup_host_substrate();
    let guard = step_engine::guard();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let page = PageIndex::new(0);
    let materialized = pc
        .materialize_page_now(page, MaterializeAccess::Read, &guard)
        .expect("install resident anon page");
    let ppn = materialized.ppn;
    drop(materialized);

    assert_eq!(pc.resident_binding_pin_count_for_test(page), 1);
    let lease = match pc.export_page_lease(page, &guard) {
        StepOutcome::Done(lease) => lease,
        other => panic!("page lease: {other:?}"),
    };
    let dma = page_allocator::acquire_dma_pin(ppn).expect("independent DMA pin");
    drop((lease, dma));

    assert_eq!(pc.resident_binding_pin_count_for_test(page), 1);
    assert_eq!(pc.lookup(page), Some(ppn));
}

#[test]
fn remove_releases_the_binding_pin_once_after_slot_withdrawal() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident cell test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_6000),
            page_count: 1,
        },
        1,
    );
    let page = PageIndex::new(0);
    let guard = step_engine::guard();
    let materialized = pc
        .materialize_page_now(page, MaterializeAccess::Read, &guard)
        .expect("install resident device page");
    let ppn = materialized.ppn;
    drop(materialized);
    drop(guard);
    assert_eq!(pc.resident_binding_pin_count_for_test(page), 1);

    assert!(pc
        .withdraw_resident_if_match_published(page, ppn, false)
        .expect("publish resident withdrawal"));

    let guard = step_engine::guard();
    assert!(pc
        .lookup_resident_with_guard_for_test(&guard, page)
        .is_none());

    assert_eq!(pc.resident_binding_pin_count_for_test(page), 0);
    assert_eq!(pc.lookup(page), None);
}

#[test]
fn withdrawn_old_root_hit_cannot_reacquire_a_released_frame() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident cell test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Anon {
            swap_policy: AnonSwapPolicy::Reclaimable,
        },
        1,
    );
    let page = PageIndex::new(0);
    let guard = step_engine::guard();
    let materialized = pc
        .materialize_page_now(page, MaterializeAccess::Read, &guard)
        .expect("install allocated resident page");
    let ppn = materialized.ppn;
    drop(materialized);
    let old_hit = pc
        .lookup_resident_with_guard_for_test(&guard, page)
        .expect("old root hit");

    assert!(pc
        .withdraw_resident_if_match_published(page, ppn, false)
        .expect("withdraw resident binding"));

    assert!(matches!(old_hit.try_materialize(), Ok(None)));
    assert!(
        page_allocator::acquire_map_pin(ppn).is_err(),
        "the retired root must not retain cache-pin ownership"
    );
}

#[test]
fn device_cell_keeps_one_device_binding_evidence() {
    let _lock = EPOCH_TEST_LOCK.lock().expect("resident cell test lock");
    setup_host_substrate();
    let pc = PageContainer::new(
        PageContainerKind::Device {
            base_ppn: Ppn(0xface_7000),
            page_count: 1,
        },
        1,
    );
    let page = PageIndex::new(0);
    let guard = step_engine::guard();
    let materialized = pc
        .materialize_page_now(page, MaterializeAccess::Read, &guard)
        .expect("install device resident cell");
    drop(materialized);

    assert_eq!(pc.resident_binding_pin_count_for_test(page), 1);
    assert!(pc.resident_binding_is_device_for_test(page));
}
