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
fn preemption_point_coalesces_need_resched_and_slice_expired_until_consumed() {
    let point = PreemptionPoint::new();

    assert_eq!(point.snapshot(), PreemptMarkers::empty());
    point.mark_need_resched();
    point.mark(PreemptMarker::NeedResched);
    point.mark_slice_expired();
    point.mark(PreemptMarker::SliceExpired);

    let snapshot = point.snapshot();
    assert!(snapshot.need_resched());
    assert!(snapshot.slice_expired());
    assert!(snapshot.contains(PreemptMarker::NeedResched));
    assert!(snapshot.contains(PreemptMarker::SliceExpired));
    assert_eq!(snapshot.bits(), 0b0000_0011);

    assert_eq!(point.consume(), snapshot);
    assert!(point.consume().is_empty());
}

#[test]
fn preemption_markers_are_consumed_independently_across_boundaries() {
    let point = PreemptionPoint::new();

    point.mark_need_resched();
    let first = point.consume();
    assert!(first.need_resched());
    assert!(!first.slice_expired());

    point.mark_slice_expired();
    let second = point.consume();
    assert!(!second.need_resched());
    assert!(second.slice_expired());
}

#[test]
fn preemption_point_clear_drops_pending_markers_without_snapshot() {
    let point = PreemptionPoint::new();

    point.mark_need_resched();
    point.mark_slice_expired();
    assert!(point.is_marked(PreemptMarker::NeedResched));

    point.clear();

    assert!(point.snapshot().is_empty());
}
