//! OBS-2 smoke test — migrated to `tx_observe::testing` helpers.
//!
//! Drives the producer side of the SPSC ring against a synthetic platform
//! provided by [`tx_observe::testing::TestPlatform`], then asserts:
//!
//! - per-record magic / version
//! - kind matches the emit method called
//! - span_id encoding (hart_id in bits 56..64, local counter in bits 0..56)
//! - producer cursor advances monotonically
//! - seq numbers are dense and monotone within a hart
//! - no torn records (full 80-byte memcpy decodes cleanly)
//!
//! Spec ref: `08_OBSERVATION_v1.md` §16 OBS-2.
//!
//! **Before migration:** 337 lines of boilerplate + tests.
//! **After migration:**  ~100 lines.

use core::sync::atomic::Ordering;

use tx_observe::testing::TestPlatform;
use tx_observe_types::payload::ALLOC_TRACK_PAGE_FRAME;
use tx_observe_types::{TxPayloadTag, TxTraceKind, TxTraceLevel};

#[test]
fn smoke_span_begin_end_roundtrip() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    let span = emitter.span_begin(
        TxTraceLevel::Drive,
        tx_observe::EventNameId::from_raw(0xAAAA_AAAA),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );
    emitter.span_end(span, TxPayloadTag::None, &[]);

    let hdr = obs.header();
    let records = obs.records();

    let producer = hdr.producer.load(Ordering::Acquire);
    let consumer = hdr.consumer.load(Ordering::Acquire);
    let lost = hdr.lost.load(Ordering::Acquire);

    assert_eq!(producer, 2, "two records written");
    assert_eq!(consumer, 0, "no consumer in test");
    assert_eq!(lost, 0, "no overrun");
    assert_eq!(hdr.hart_id, 0);

    // Slot 0 — SpanBegin
    let r0 = &records[0];
    assert_eq!(r0.magic, 0x5254, "slot 0 magic 'TR'");
    assert_eq!(r0.version, 0);
    assert_eq!(r0.kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(r0.level, TxTraceLevel::Drive as u8);
    assert_eq!(r0.seq, 0);
    assert_eq!(r0.hart, 0);
    assert_eq!(r0.name, 0xAAAA_AAAA);
    assert_ne!(r0.ts, 0, "timestamp populated");

    // span_id: hart_id (0) in bits 56..64, local counter in bits 0..56.
    let span_raw = span.raw();
    assert_eq!(r0.span, span_raw);
    assert_eq!((span_raw >> 56) & 0xFF, 0, "hart_id in span high byte");
    assert!(
        span_raw & 0x00FF_FFFF_FFFF_FFFF > 0,
        "local counter nonzero"
    );

    // Slot 1 — SpanEnd
    let r1 = &records[1];
    assert_eq!(r1.magic, 0x5254, "slot 1 magic 'TR'");
    assert_eq!(r1.kind, TxTraceKind::SpanEnd as u8);
    assert_eq!(r1.seq, 1, "seq advances");
    assert_eq!(r1.span, span_raw, "SpanEnd references the same span_id");
    assert!(r1.ts >= r0.ts, "monotone timestamps");
}

#[test]
fn smoke_instant_and_counter() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    emitter.instant(
        TxTraceLevel::Boundary,
        tx_observe::EventNameId::from_raw(0xBBBB),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );
    emitter.counter(tx_observe::EventNameId::from_raw(0xCCCC), 42);

    let hdr = obs.header();
    let records = obs.records();

    assert_eq!(hdr.producer.load(Ordering::Acquire), 2);

    assert_eq!(records[0].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[0].level, TxTraceLevel::Boundary as u8);
    assert_eq!(records[0].name, 0xBBBB);

    assert_eq!(records[1].kind, TxTraceKind::Counter as u8);
    assert_eq!(records[1].name, 0xCCCC);
}

#[test]
fn allocation_marker_routes_via_explicit_track_parent() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    emitter.allocation(
        tx_observe::AllocationTrack::PageFrame,
        tx_observe::EventNameId::from_raw(0xA110_C001),
        0x1234,
    );

    let records = obs.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[0].level, TxTraceLevel::Mutation as u8);
    assert_eq!(records[0].name, 0xA110_C001);
    assert_eq!(records[0].parent, ALLOC_TRACK_PAGE_FRAME);
    assert_eq!(records[0].payload_tag, TxPayloadTag::ArgValue as u16);
}

#[test]
fn disabled_observation_hides_current_emitter_without_clearing_ring() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    emitter.counter(tx_observe::EventNameId::from_raw(0xCAFE), 1);
    tx_observe::set_enabled(false);
    assert!(tx_observe::current().is_none());

    tx_observe::set_enabled(true);
    let emitter = tx_observe::current().expect("emitter restored after re-enable");
    emitter.counter(tx_observe::EventNameId::from_raw(0xCAFE), 2);

    let hdr = obs.header();
    assert_eq!(hdr.producer.load(Ordering::Acquire), 2);
}

#[test]
fn compact_dump_ring_order_rounds_up_to_cover_visible_records() {
    assert_eq!(tx_observe::testing_compact_ring_order(0), 2);
    assert_eq!(tx_observe::testing_compact_ring_order(1), 2);
    assert_eq!(tx_observe::testing_compact_ring_order(4), 2);
    assert_eq!(tx_observe::testing_compact_ring_order(5), 3);
    assert_eq!(tx_observe::testing_compact_ring_order(100), 7);
}

#[test]
fn smoke_overrun_increments_lost_counter() {
    let obs = TestPlatform::new().with_slot_count(16).init();
    let emitter = obs.emitter();

    // slot_count = 16.  Emit 17 records without consumer advancing: 16 land,
    // 17th must drop and bump `lost`.
    for _ in 0..17 {
        emitter.instant(
            TxTraceLevel::Drive,
            tx_observe::EventNameId::from_raw(0xDEAD),
            tx_observe::SpanId::NONE,
            TxPayloadTag::None,
            &[],
        );
    }

    let hdr = obs.header();
    let producer = hdr.producer.load(Ordering::Acquire);
    let lost = hdr.lost.load(Ordering::Acquire);

    assert_eq!(producer, 16, "producer caps at slot_count on overrun");
    assert_eq!(lost, 1, "one record dropped");
}

#[test]
fn smoke_span_ids_unique_per_emit() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    let s1 = emitter.span_begin(
        TxTraceLevel::Drive,
        tx_observe::EventNameId::from_raw(1),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );
    let s2 = emitter.span_begin(
        TxTraceLevel::Drive,
        tx_observe::EventNameId::from_raw(2),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );
    let s3 = emitter.span_begin(
        TxTraceLevel::Drive,
        tx_observe::EventNameId::from_raw(3),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );

    assert_ne!(s1.raw(), s2.raw());
    assert_ne!(s2.raw(), s3.raw());
    // Local counter is monotone within a hart.
    assert!(s2.raw() > s1.raw());
    assert!(s3.raw() > s2.raw());
}
