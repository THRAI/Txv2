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
use tx_observe_types::payload::{ALLOC_TRACK_DS_METHOD, ALLOC_TRACK_PAGE_FRAME};
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
fn debug_counter_hashes_name_inside_emitter() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    emitter.debug_counter(b"debug.test.counter", 7);

    let records = obs.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, TxTraceKind::Counter as u8);
    assert_eq!(records[0].level, TxTraceLevel::Boundary as u8);
    assert_eq!(records[0].name, tx_observe::fnv1a32(b"debug.test.counter"));
    assert_eq!(records[0].payload_tag, TxPayloadTag::CounterValue as u16);
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
fn ds_method_metric_routes_via_explicit_track_parent() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    emitter.ds_method_metric(
        tx_observe::EventNameId::from_raw(0xD500_C001),
        tx_observe::EventNameId::from_raw(0xD500_D042),
        42,
    );

    let records = obs.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[0].level, TxTraceLevel::Mutation as u8);
    assert_eq!(records[0].name, 0xD500_C001);
    assert_eq!(records[0].parent, ALLOC_TRACK_DS_METHOD);
    assert_eq!(records[0].payload_tag, TxPayloadTag::ArgValue as u16);
}

#[test]
fn wait_source_notify_helper_emits_typed_yield_instant() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    emitter.wait_source_notify(0x42, 0b1010, 0x1234, 0x5678);

    let records = obs.records();
    assert_eq!(records.len(), 1);

    let rec = &records[0];
    assert_eq!(rec.kind, TxTraceKind::Instant as u8);
    assert_eq!(rec.level, TxTraceLevel::Yield as u8);
    assert_eq!(rec.name, tx_observe::fnv1a32(b"wake.notify"));
    assert_eq!(rec.span, tx_observe::SpanId::NONE.raw());
    assert_eq!(rec.parent, tx_observe::SpanId::NONE.raw());
    assert_eq!(rec.payload_tag, TxPayloadTag::WaitSourceNotify as u16);
    assert_eq!(rec.payload_len, 16);

    assert_eq!(
        u32::from_le_bytes(rec.payload[0..4].try_into().unwrap()),
        0x42
    );
    assert_eq!(
        u32::from_le_bytes(rec.payload[4..8].try_into().unwrap()),
        0b1010
    );
    assert_eq!(
        u32::from_le_bytes(rec.payload[8..12].try_into().unwrap()),
        0x1234
    );
    assert_eq!(
        u32::from_le_bytes(rec.payload[12..16].try_into().unwrap()),
        0x5678
    );
}

#[test]
fn drive_step_yield_helpers_emit_typed_records() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();
    let op_name = tx_observe::EventNameId::from_raw(0xD21E_0001);

    let drive = emitter.drive_begin(op_name, 1, 1, true, 0x1234, tx_observe::SpanId::NONE);
    let step = emitter.step_begin(drive);
    emitter.step_end(step, 1, false, 1, 1, 0, 4096);
    let yield_span = emitter.yield_begin(drive, 1, 0x1234, 0);
    emitter.resume(yield_span, 0, 0, 0, 0xAABB_CCDD_EEFF_0011);
    emitter.span_end_empty(yield_span);
    emitter.drive_end(drive, 0, 0, 0);

    let records = obs.records();
    assert_eq!(records.len(), 7);

    assert_eq!(records[0].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[0].level, TxTraceLevel::Drive as u8);
    assert_eq!(records[0].name, op_name.raw());
    assert_eq!(records[0].parent, tx_observe::SpanId::NONE.raw());
    assert_eq!(records[0].payload_tag, TxPayloadTag::DriveBegin as u16);
    assert_eq!(
        u32::from_le_bytes(records[0].payload[0..4].try_into().unwrap()),
        op_name.raw()
    );
    assert_eq!(records[0].payload[4], 1);
    assert_eq!(records[0].payload[5], 1);
    assert_eq!(records[0].payload[6], 1);
    assert_eq!(
        u32::from_le_bytes(records[0].payload[8..12].try_into().unwrap()),
        0x1234
    );

    assert_eq!(records[1].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[1].level, TxTraceLevel::Step as u8);
    assert_eq!(records[1].name, tx_observe::fnv1a32(b"step"));
    assert_eq!(records[1].parent, drive.raw());
    assert_eq!(records[1].payload_tag, TxPayloadTag::None as u16);

    assert_eq!(records[2].kind, TxTraceKind::SpanEnd as u8);
    assert_eq!(records[2].span, step.raw());
    assert_eq!(records[2].payload_tag, TxPayloadTag::StepOutcome as u16);
    assert_eq!(records[2].payload[0], 1);
    assert_eq!(records[2].payload[1], 0);
    assert_eq!(records[2].payload[2], 1);
    assert_eq!(records[2].payload[3], 1);
    assert_eq!(
        u32::from_le_bytes(records[2].payload[8..12].try_into().unwrap()),
        4096
    );

    assert_eq!(records[3].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[3].level, TxTraceLevel::Yield as u8);
    assert_eq!(records[3].name, tx_observe::fnv1a32(b"yield.OnWaitSource"));
    assert_eq!(records[3].parent, drive.raw());
    assert_eq!(records[3].payload_tag, TxPayloadTag::YieldBegin as u16);

    assert_eq!(records[4].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[4].level, TxTraceLevel::Yield as u8);
    assert_eq!(records[4].name, tx_observe::fnv1a32(b"resume"));
    assert_eq!(records[4].parent, yield_span.raw());
    assert_eq!(records[4].payload_tag, TxPayloadTag::Resume as u16);
    assert_eq!(
        u64::from_le_bytes(records[4].payload[8..16].try_into().unwrap()),
        0xAABB_CCDD_EEFF_0011
    );

    assert_eq!(records[5].kind, TxTraceKind::SpanEnd as u8);
    assert_eq!(records[5].span, yield_span.raw());
    assert_eq!(records[5].payload_tag, TxPayloadTag::None as u16);

    assert_eq!(records[6].kind, TxTraceKind::SpanEnd as u8);
    assert_eq!(records[6].span, drive.raw());
    assert_eq!(records[6].payload_tag, TxPayloadTag::DriveEnd as u16);
    assert_eq!(records[6].payload[12], 0);
}

#[test]
fn syscall_helpers_emit_boundary_span_and_arg_continuations() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    let span = emitter.syscall_enter(64, 0, &[10, 20, 30, 40, 50, 60]);
    emitter.syscall_exit(span, -1, 22, 1);

    let records = obs.records();
    assert_eq!(records.len(), 8);

    assert_eq!(records[0].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[0].level, TxTraceLevel::Boundary as u8);
    assert_eq!(records[0].name, 64);
    assert_eq!(records[0].payload_tag, TxPayloadTag::SyscallEnter as u16);
    assert_eq!(
        u32::from_le_bytes(records[0].payload[0..4].try_into().unwrap()),
        64
    );
    assert_eq!(
        u16::from_le_bytes(records[0].payload[4..6].try_into().unwrap()),
        0
    );
    assert_eq!(
        u16::from_le_bytes(records[0].payload[6..8].try_into().unwrap()),
        6
    );

    for (idx, value) in [10u64, 20, 30, 40, 50, 60].into_iter().enumerate() {
        let rec = &records[idx + 1];
        assert_eq!(rec.kind, TxTraceKind::Instant as u8);
        assert_eq!(rec.level, TxTraceLevel::Boundary as u8);
        assert_eq!(rec.parent, span.raw());
        assert_eq!(rec.payload_tag, TxPayloadTag::ArgValue as u16);
        assert_eq!(
            u64::from_le_bytes(rec.payload[8..16].try_into().unwrap()),
            value
        );
    }

    assert_eq!(records[7].kind, TxTraceKind::SpanEnd as u8);
    assert_eq!(records[7].span, span.raw());
    assert_eq!(records[7].payload_tag, TxPayloadTag::SyscallExit as u16);
    assert_eq!(
        i64::from_le_bytes(records[7].payload[0..8].try_into().unwrap()),
        -1
    );
    assert_eq!(
        i32::from_le_bytes(records[7].payload[8..12].try_into().unwrap()),
        22
    );
    assert_eq!(records[7].payload[12], 1);
}

#[test]
fn phase_and_mutation_helpers_emit_typed_records() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();

    let phase = emitter.phase_begin(1, 3);
    emitter.phase_end(phase);
    emitter.mutation_zone_sign(0xAB00_0000_0000_0011, 0xAB);
    emitter.mutation_index_commit(0x1000, 0x20, 0xAB00_0000_0000_0033);

    let records = obs.records();
    assert_eq!(records.len(), 4);

    assert_eq!(records[0].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[0].level, TxTraceLevel::Phase as u8);
    assert_eq!(records[0].name, 1);
    assert_eq!(records[0].payload_tag, TxPayloadTag::PhaseTransition as u16);
    assert_eq!(records[0].payload[0], 1);
    assert_eq!(records[0].payload[1], 3);

    assert_eq!(records[1].kind, TxTraceKind::SpanEnd as u8);
    assert_eq!(records[1].span, phase.raw());
    assert_eq!(records[1].payload_tag, TxPayloadTag::None as u16);

    assert_eq!(records[2].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[2].level, TxTraceLevel::Mutation as u8);
    assert_eq!(records[2].name, 0x4d5a5347);
    assert_eq!(
        records[2].payload_tag,
        TxPayloadTag::MutationZoneSign as u16
    );
    assert_eq!(
        u64::from_le_bytes(records[2].payload[0..8].try_into().unwrap()),
        0xAB00_0000_0000_0011
    );
    assert_eq!(records[2].payload[8], 0xAB);

    assert_eq!(records[3].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[3].level, TxTraceLevel::Mutation as u8);
    assert_eq!(records[3].name, 0x4d494358);
    assert_eq!(
        records[3].payload_tag,
        TxPayloadTag::MutationIndexCommit as u16
    );
    assert_eq!(
        u32::from_le_bytes(records[3].payload[0..4].try_into().unwrap()),
        0x1000
    );
    assert_eq!(
        u32::from_le_bytes(records[3].payload[4..8].try_into().unwrap()),
        0x20
    );
    assert_eq!(
        u64::from_le_bytes(records[3].payload[8..16].try_into().unwrap()),
        0xAB00_0000_0000_0033
    );
}

#[test]
fn process_label_and_group_helpers_emit_synthetic_sched_records() {
    let obs = TestPlatform::new().init();
    let emitter = obs.emitter();
    let parent = emitter.step_begin(tx_observe::SpanId::NONE);
    let comm = *b"long-process-ab!";

    emitter.process_label(parent, 0x42, &comm);
    emitter.process_group(parent, 0x42, 0x21, 0x11);

    let records = obs.records();
    assert_eq!(records.len(), 3);

    assert_eq!(records[1].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[1].level, TxTraceLevel::Sched as u8);
    assert_eq!(records[1].name, 0x9000_0042);
    assert_eq!(records[1].parent, parent.raw());
    assert_eq!(records[1].payload_tag, TxPayloadTag::ProcessLabel as u16);
    assert_eq!(
        u32::from_le_bytes(records[1].payload[0..4].try_into().unwrap()),
        0x42
    );
    assert_eq!(&records[1].payload[4..15], b"long-proces");
    assert_eq!(records[1].payload[15], 0);

    assert_eq!(records[2].kind, TxTraceKind::Instant as u8);
    assert_eq!(records[2].level, TxTraceLevel::Sched as u8);
    assert_eq!(records[2].name, 0xA000_0042);
    assert_eq!(records[2].parent, parent.raw());
    assert_eq!(records[2].payload_tag, TxPayloadTag::ProcessGroup as u16);
    assert_eq!(
        u32::from_le_bytes(records[2].payload[0..4].try_into().unwrap()),
        0x42
    );
    assert_eq!(
        u32::from_le_bytes(records[2].payload[4..8].try_into().unwrap()),
        0x21
    );
    assert_eq!(
        u32::from_le_bytes(records[2].payload[8..12].try_into().unwrap()),
        0x11
    );
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
