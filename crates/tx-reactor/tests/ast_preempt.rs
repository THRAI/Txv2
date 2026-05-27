use tx_reactor::{
    ast::{AstMarker, AstQueueEffect, AstSlot},
    preempt::{PreemptMarker, PreemptMarkers, PreemptionPoint},
};

#[test]
fn ast_slot_preserves_first_seen_order_and_coalesces_duplicates() {
    let mut slot = AstSlot::new();

    assert!(slot.is_empty());
    assert_eq!(
        slot.queue(AstMarker::InterruptCheck),
        AstQueueEffect::Queued
    );
    assert_eq!(
        slot.queue(AstMarker::FaultInjection),
        AstQueueEffect::Queued
    );
    assert_eq!(
        slot.queue(AstMarker::InterruptCheck),
        AstQueueEffect::Coalesced
    );
    assert_eq!(slot.queue(AstMarker::PreemptCheck), AstQueueEffect::Queued);

    assert_eq!(
        slot.pending(),
        &[
            AstMarker::InterruptCheck,
            AstMarker::FaultInjection,
            AstMarker::PreemptCheck,
        ]
    );
    assert!(slot.has_pending(AstMarker::FaultInjection));

    let batch = slot.consume();

    assert_eq!(batch.len(), 3);
    assert_eq!(
        batch.as_slice(),
        &[
            AstMarker::InterruptCheck,
            AstMarker::FaultInjection,
            AstMarker::PreemptCheck,
        ]
    );
    assert!(slot.is_empty());
    assert!(slot.consume().is_empty());
}

#[test]
fn ast_marker_can_be_requeued_after_consumption() {
    let mut slot = AstSlot::new();

    assert_eq!(slot.queue(AstMarker::Drain), AstQueueEffect::Queued);
    assert_eq!(slot.queue(AstMarker::Drain), AstQueueEffect::Coalesced);
    assert_eq!(slot.consume().as_slice(), &[AstMarker::Drain]);

    assert_eq!(slot.queue(AstMarker::Drain), AstQueueEffect::Queued);
    assert_eq!(slot.consume().into_vec(), vec![AstMarker::Drain]);
}

#[test]
fn ast_slots_are_task_local_and_independent() {
    let mut first = AstSlot::new();
    let mut second = AstSlot::new();

    assert_eq!(first.queue(AstMarker::Local(7)), AstQueueEffect::Queued);
    assert_eq!(second.queue(AstMarker::Local(7)), AstQueueEffect::Queued);
    assert_eq!(
        second.queue(AstMarker::InterruptCheck),
        AstQueueEffect::Queued
    );

    assert_eq!(first.consume().as_slice(), &[AstMarker::Local(7)]);
    assert!(first.is_empty());
    assert_eq!(
        second.pending(),
        &[AstMarker::Local(7), AstMarker::InterruptCheck]
    );

    second.clear();
    assert!(second.is_empty());
}

#[test]
fn preemption_point_coalesces_all_markers_until_consumed() {
    let point = PreemptionPoint::new();

    assert_eq!(point.snapshot(), PreemptMarkers::empty());
    point.mark_need_resched();
    point.mark(PreemptMarker::NeedResched);
    point.mark_slice_expired();
    point.mark(PreemptMarker::SliceExpired);
    point.mark_userspace_preempt();
    point.mark(PreemptMarker::UserspacePreempt);

    let snapshot = point.snapshot();
    assert!(snapshot.need_resched());
    assert!(snapshot.slice_expired());
    assert!(snapshot.userspace_preempt());
    assert!(snapshot.contains(PreemptMarker::NeedResched));
    assert!(snapshot.contains(PreemptMarker::SliceExpired));
    assert!(snapshot.contains(PreemptMarker::UserspacePreempt));
    assert_eq!(snapshot.bits(), 0b0000_0111);

    assert_eq!(point.consume(), snapshot);
    assert!(point.consume().is_empty());
}

#[test]
fn userspace_preempt_is_distinct_from_normal_reschedule() {
    let point = PreemptionPoint::new();

    point.mark_need_resched();
    let first = point.consume();
    assert!(first.need_resched());
    assert!(!first.slice_expired());
    assert!(!first.userspace_preempt());

    point.mark_userspace_preempt();
    let second = point.consume();
    assert!(!second.need_resched());
    assert!(!second.slice_expired());
    assert!(second.userspace_preempt());
}

#[test]
fn take_single_marker_preserves_other_pending_markers() {
    let point = PreemptionPoint::new();

    point.mark_need_resched();
    point.mark_userspace_preempt();

    assert!(point.take(PreemptMarker::UserspacePreempt));
    assert!(!point.take(PreemptMarker::UserspacePreempt));

    let remaining = point.consume();
    assert!(remaining.need_resched());
    assert!(!remaining.userspace_preempt());
}

#[test]
fn preemption_point_clear_drops_pending_markers_without_snapshot() {
    let point = PreemptionPoint::new();

    point.mark_need_resched();
    point.mark_slice_expired();
    assert!(point.is_marked(PreemptMarker::NeedResched));
    assert!(!point.is_marked(PreemptMarker::UserspacePreempt));
    point.mark_userspace_preempt();
    assert!(point.is_marked(PreemptMarker::UserspacePreempt));

    point.clear();

    assert!(point.snapshot().is_empty());
}
