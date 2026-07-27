//! Tests for the canonical `tx_substrate::wake` free-function verbs.
//!
//! `new_source` and `notify` are the promoted canonical shims that
//! replace the per-subsystem `wait_routing::new_wait_source` and
//! `wait_routing::*_with_post` wrappers.

use alloc::sync::Arc;
use tx_substrate::step::InterestMask;
use tx_substrate::wake::{self, MailboxEvent, TaskMailbox, WaitGeneration};

extern crate alloc;

/// `new_source` must produce distinct `Arc` instances (pointer inequality)
/// for distinct ids and even for the same id.
#[test]
fn new_source_returns_unique_arcs() {
    let a = wake::new_source(1);
    let b = wake::new_source(1);
    let c = wake::new_source(2);

    // Each call produces a fresh allocation.
    assert!(
        !Arc::ptr_eq(&a, &b),
        "same id must still produce distinct Arcs"
    );
    assert!(
        !Arc::ptr_eq(&a, &c),
        "different id must produce distinct Arcs"
    );
}

/// `new_source` records the id correctly.
#[test]
fn new_source_records_id() {
    use tx_substrate::step::WaitSourceId;

    let src = wake::new_source(42);
    assert_eq!(src.id(), WaitSourceId::new(42));
}

/// `notify` correctly delivers a `SourceFired` event to a registered mailbox.
#[test]
fn notify_passes_mask_bits_through() {
    let src = wake::new_source(7);
    let mb = Arc::new(TaskMailbox::new());
    let gen = WaitGeneration::new(3);

    let _guard = src
        .prepare(Arc::downgrade(&mb), gen, InterestMask::new(0b1111))
        .install();

    wake::notify(&src, 0b0101);

    let evt = mb.poll().expect("mailbox should have an event");
    match evt {
        MailboxEvent::SourceFired { interests, .. } => {
            // Only the overlapping bits should arrive.
            assert_eq!(interests, InterestMask::new(0b0101));
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }
}

/// `notify` with an empty mask posts nothing (WaitSource::notify
/// iterates interests; zero overlap means no post).
#[test]
fn notify_zero_mask_posts_nothing() {
    let src = wake::new_source(99);
    let mb = Arc::new(TaskMailbox::new());
    let gen = WaitGeneration::new(1);

    let _guard = src
        .prepare(Arc::downgrade(&mb), gen, InterestMask::new(0b1111))
        .install();

    wake::notify(&src, 0);
    assert!(mb.is_empty(), "zero-mask notify should post nothing");
}

/// A notify that races just before registration must not be lost.
/// The next waiter consumes the pending mask and retries its step.
#[test]
fn notify_before_register_is_delivered_as_pending_source_fire() {
    let src = wake::new_source(101);
    let mb = Arc::new(TaskMailbox::new());
    let gen = WaitGeneration::new(9);

    wake::notify(&src, 0b0100);
    assert!(mb.is_empty(), "no subscriber existed at notify time");

    let _guard = src
        .prepare(Arc::downgrade(&mb), gen, InterestMask::new(0b1100))
        .install();

    let evt = mb.poll().expect("pending source fire should be delivered");
    match evt {
        MailboxEvent::SourceFired {
            generation,
            interests,
            ..
        } => {
            assert_eq!(generation, gen);
            assert_eq!(interests, InterestMask::new(0b0100));
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }

    let mb2 = Arc::new(TaskMailbox::new());
    let _guard2 = src
        .prepare(
            Arc::downgrade(&mb2),
            WaitGeneration::new(10),
            InterestMask::new(0b1100),
        )
        .install();
    assert!(mb2.is_empty(), "pending mask is consumed once");
}
