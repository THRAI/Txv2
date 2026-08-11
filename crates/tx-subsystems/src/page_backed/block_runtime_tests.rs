use super::block_runtime::BlockSubmissionHandle;
use crate::fs_iface::IoDataLeaseId;
use crate::io_manager::backend::BioPlanList;
use crate::io_manager::block::{
    BioPlan, BioVec, BlockDeviceCompletion, BlockFlags, BlockOp, DeviceKey, LbaRange,
};
use crate::io_manager::page::{
    service::{
        PageService, PageServiceBackendOutcome, PageServiceBackendPrepared,
        PageServiceBackendSubmitOutcome,
    },
    PageContainerKey, PageGeneration, PageIoFlags, PageIoOp, PageIoPriority, PageIoRange,
    PageIoRequest, PageIoRequestId,
};
use crate::io_manager::runtime::ServiceBudget;
use alloc::vec::Vec;

fn read_plan(lba: u64) -> BioPlan {
    BioPlan::new(
        DeviceKey::new(1),
        BlockOp::Read,
        LbaRange::new(lba, 1),
        alloc::vec![BioVec::new(0x1000 + lba, 0, 512)],
        BlockFlags::EMPTY,
    )
}

fn page_request(id: u64) -> PageIoRequest {
    PageIoRequest::new(
        PageIoRequestId::new(id),
        PageContainerKey::new(1),
        PageIoRange::new(id, 1),
        PageIoOp::Writeback,
        PageIoPriority::Demand,
        PageIoFlags::DEMAND,
        Some(PageGeneration::new(id)),
    )
}

fn page_bio_action(
    service: &mut PageService,
    request: PageIoRequest,
    bios: Vec<BioPlan>,
) -> crate::io_manager::page::PageL6Action {
    let prepared = service
        .prepare_backend_outcome(
            PageServiceBackendOutcome::BlockBios(BioPlanList::from_vec(bios)),
            request,
        )
        .expect("prepare page BIO action");
    let PageServiceBackendPrepared::Submit(action) = prepared else {
        panic!("page BIOs must require L6 admission");
    };
    action
}

#[test]
fn direct_completion_route_is_removed_before_pagecontainer_terminalization() {
    let manager = BlockSubmissionHandle::new(4, 2);
    let first = IoDataLeaseId::new(11);
    let replacement = IoDataLeaseId::new(12);
    manager
        .submit_direct(first, read_plan(8))
        .expect("first direct submission");
    manager
        .submit_direct(replacement, read_plan(32))
        .expect("replacement direct submission");

    let driven = manager.drive(ServiceBudget::new(2), |_| false);
    assert_eq!(driven.dispatches.len(), 2);
    let first_tag = driven.dispatches[0].tag;
    let replacement_tag = driven.dispatches[1].tag;
    let mut service = PageService::new(4);

    let first_completion = manager
        .complete(
            &mut service,
            BlockDeviceCompletion::new(first_tag, Ok(())),
            |_| None,
        )
        .expect("first tagged completion");
    assert_eq!(first_completion.direct, alloc::vec![(first, Ok(()))]);
    assert_eq!(manager.direct_tracker_len_for_test(), 1);

    assert!(manager
        .complete(
            &mut service,
            BlockDeviceCompletion::new(first_tag, Ok(())),
            |_| None,
        )
        .is_err());
    assert_eq!(manager.direct_tracker_len_for_test(), 1);

    let replacement_completion = manager
        .complete(
            &mut service,
            BlockDeviceCompletion::new(replacement_tag, Ok(())),
            |_| None,
        )
        .expect("replacement tagged completion");
    assert_eq!(
        replacement_completion.direct,
        alloc::vec![(replacement, Ok(()))]
    );
    assert_eq!(manager.direct_tracker_len_for_test(), 0);
}

#[test]
fn page_route_commit_makes_l6_dispatchable_and_stale_tag_cannot_consume_replacement() {
    let manager = BlockSubmissionHandle::new(4, 1);
    let mut service = PageService::new(4);
    let first_request = page_request(21);
    let receipt = manager.submit_page_action(page_bio_action(
        &mut service,
        first_request.clone(),
        alloc::vec![read_plan(64)],
    ));

    assert!(manager
        .drive(ServiceBudget::new(1), |_| false)
        .dispatches
        .is_empty());

    let applied = service
        .apply_l6_receipt(receipt)
        .expect("accept the L6 receipt");
    manager.record_page_outcome(&applied.outcome);
    let first_dispatch = manager
        .drive(ServiceBudget::new(1), |_| false)
        .dispatches
        .into_iter()
        .next()
        .expect("route commit releases the BIO");
    let first_tag = first_dispatch.tag;

    let first_completion = manager
        .complete_receipt(BlockDeviceCompletion::new(first_tag, Ok(())))
        .expect("first completion");
    assert_eq!(first_completion.page.len(), 1);
    assert_eq!(first_completion.page[0].request(), &first_request);
    service
        .prepare_block_completion_routes(
            first_completion.block,
            first_completion.page,
            false,
            |_| None,
        )
        .expect("route enters L4 once");

    let replacement_request = page_request(22);
    let replacement_receipt = manager.submit_page_action(page_bio_action(
        &mut service,
        replacement_request.clone(),
        alloc::vec![read_plan(96)],
    ));
    let replacement_applied = service
        .apply_l6_receipt(replacement_receipt)
        .expect("accept replacement receipt");
    manager.record_page_outcome(&replacement_applied.outcome);
    assert_eq!(manager.tracker_len_for_test(), 1);

    assert!(manager
        .complete_receipt(BlockDeviceCompletion::new(first_tag, Ok(())))
        .is_err());
    assert_eq!(manager.tracker_len_for_test(), 1);

    let replacement_dispatch = manager
        .drive(ServiceBudget::new(1), |_| false)
        .dispatches
        .into_iter()
        .next()
        .expect("replacement dispatch");
    let replacement_completion = manager
        .complete_receipt(BlockDeviceCompletion::new(replacement_dispatch.tag, Ok(())))
        .expect("replacement completion");
    assert_eq!(replacement_completion.page.len(), 1);
    assert_eq!(
        replacement_completion.page[0].request(),
        &replacement_request
    );
    assert_eq!(manager.tracker_len_for_test(), 0);
}

#[test]
fn partial_page_receipt_commits_only_accepted_l6_routes() {
    let manager = BlockSubmissionHandle::new(1, 1);
    let mut service = PageService::new(4);
    let request = page_request(31);
    let receipt = manager.submit_page_action(page_bio_action(
        &mut service,
        request.clone(),
        alloc::vec![read_plan(128), read_plan(192)],
    ));
    assert_eq!(receipt.submitted.len(), 1);
    assert!(receipt.failure.is_some());
    assert!(manager
        .drive(ServiceBudget::new(1), |_| false)
        .dispatches
        .is_empty());

    let applied = service
        .apply_l6_receipt(receipt)
        .expect("partial receipt remains valid for its accepted prefix");
    assert!(applied.failure.is_some());
    assert!(matches!(
        &applied.outcome,
        PageServiceBackendSubmitOutcome::BlockBiosQueued { submitted, .. } if submitted.len() == 1
    ));
    manager.record_page_outcome(&applied.outcome);

    let dispatch = manager.drive(ServiceBudget::new(2), |_| false).dispatches;
    assert_eq!(dispatch.len(), 1);
    let completion = manager
        .complete_receipt(BlockDeviceCompletion::new(dispatch[0].tag, Ok(())))
        .expect("accepted prefix completion");
    assert_eq!(completion.page.len(), 1);
    assert_eq!(completion.page[0].request(), &request);
    assert_eq!(manager.tracker_len_for_test(), 0);
    assert!(manager
        .drive(ServiceBudget::new(1), |_| false)
        .dispatches
        .is_empty());
}
