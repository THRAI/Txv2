//! Integration test: `drive` emits L2/L3/L4 observation records.
//!
//! Pins the OBS hook surface added per `docs/Txv3/08_OBSERVATION_v1.md` §6
//! HOOKS-1 (Drive / Step rows) and §8.4 (Yield/Resume payloads).
//!
//! Each scenario drives a scripted `MockStepOp` through `drive()` and asserts
//! the resulting `TxTraceRecord` shape in the per-hart SPSC ring:
//!
//! 1. `drive_emits_l2_drive_begin_and_end_around_done` —
//!    Done-in-first-step produces exactly four records: `SpanBegin(Drive)`,
//!    `SpanBegin(Step)`, `SpanEnd(Step, variant=Done)`, `SpanEnd(Drive, ok)`.
//!
//! 2. `drive_emits_l4_step_outcome_continue_then_done` —
//!    Two iterations carry the per-step outcome variant (`Continue` then
//!    `Done`) at the L4 level. Confirms `iteration` advances and that
//!    `PayloadStepOutcome.variant` rides on the L4 span-end.
//!
//! 3. `drive_emits_l2_drive_end_err_on_step_err` —
//!    `StepOutcome::Err` closes both step and drive spans with the
//!    `result_kind = 1` (Err) shape and the `errno` field populated by
//!    `Errno::linux_i32`.
//!
//! No reactor parking is exercised here — `OnWaitSource` etc. require the
//! L3 yield/resume path which depends on the mailbox plumbing covered in
//! separate `wake` integration tests. The yield-shape branch is tested via
//! `DriveMode::Nonblocking` translating to `EAGAIN` (L2 end with errno set,
//! no L3 yield span opened because `AcceptOutcome::Translate` precedes
//! resolution).

extern crate std;

use std::vec::Vec;
use tx_observe::testing::TestPlatform;
use tx_observe_types::{
    PayloadDriveBegin, PayloadDriveEnd, PayloadStepOutcome, TxPayloadTag, TxTraceKind,
    TxTraceLevel, TxTraceRecord,
};
use tx_scripts::adapter::step_engine::{
    DriveMode, Errno, InterestMask, NoProgress, ProcessIdentity, ScriptCtx, StepOp, StepOutcome,
    WaitSourceId, YieldShape,
};

// ---------------------------------------------------------------------------
// Mock op + block_on helper (mirror tests/drive.rs)
// ---------------------------------------------------------------------------

fn block_on<F: core::future::Future>(future: F) -> F::Output {
    use core::task::{Context, Poll, Waker};
    let waker = Waker::noop().clone();
    let mut cx = Context::from_waker(&waker);
    let mut pinned = Box::pin(future);
    for _ in 0..64 {
        match pinned.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => continue,
        }
    }
    panic!("drive_observe test block_on: future did not resolve");
}

struct MockStepOp {
    queue: std::collections::VecDeque<StepOutcome<u32, NoProgress>>,
}

impl MockStepOp {
    fn new(outcomes: impl IntoIterator<Item = StepOutcome<u32, NoProgress>>) -> Self {
        Self {
            queue: outcomes.into_iter().collect(),
        }
    }
}

impl StepOp<ProcessIdentity> for MockStepOp {
    type Output = u32;
    type Progress = NoProgress;
    fn step(&mut self, _ctx: &mut ScriptCtx<ProcessIdentity>) -> StepOutcome<u32, NoProgress> {
        self.queue
            .pop_front()
            .expect("MockStepOp: queue exhausted unexpectedly")
    }
}

// ---------------------------------------------------------------------------
// Payload decoders — read inline 16-byte buffer back into the typed struct
// ---------------------------------------------------------------------------

fn read_drive_begin(rec: &TxTraceRecord) -> PayloadDriveBegin {
    assert_eq!(rec.payload_tag, TxPayloadTag::DriveBegin as u16);
    unsafe { core::ptr::read_unaligned(rec.payload.as_ptr() as *const PayloadDriveBegin) }
}

fn read_drive_end(rec: &TxTraceRecord) -> PayloadDriveEnd {
    assert_eq!(rec.payload_tag, TxPayloadTag::DriveEnd as u16);
    unsafe { core::ptr::read_unaligned(rec.payload.as_ptr() as *const PayloadDriveEnd) }
}

fn read_step_outcome(rec: &TxTraceRecord) -> PayloadStepOutcome {
    assert_eq!(rec.payload_tag, TxPayloadTag::StepOutcome as u16);
    unsafe { core::ptr::read_unaligned(rec.payload.as_ptr() as *const PayloadStepOutcome) }
}

// ---------------------------------------------------------------------------
// Test 1: Done in first step → 4 records
// ---------------------------------------------------------------------------

#[test]
fn drive_emits_l2_drive_begin_and_end_around_done() {
    let obs = TestPlatform::new().init();

    let op = MockStepOp::new([StepOutcome::Done(42)]);
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));
    assert_eq!(result, Ok(42));

    let records = obs.records();
    assert_eq!(
        records.len(),
        4,
        "expected exactly 4 records (L2 begin, L4 begin, L4 end, L2 end), got {}: {:?}",
        records.len(),
        records.iter().map(|r| r.kind).collect::<Vec<_>>(),
    );

    // 1. L2 SpanBegin (DriveBegin)
    assert_eq!(records[0].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[0].level, TxTraceLevel::Drive as u8);
    let db = read_drive_begin(&records[0]);
    assert_eq!(db.mode, 1, "Waiting=1");
    assert_eq!(db.has_deadline, 0);
    assert_eq!(db.task_id_low, 0, "no subject in test ctx");
    assert_ne!(db.op_type, 0, "op_type id derived from type_name");

    // 2. L4 SpanBegin (Step iteration 0)
    assert_eq!(records[1].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[1].level, TxTraceLevel::Step as u8);

    // 3. L4 SpanEnd with StepOutcome(variant=Done)
    assert_eq!(records[2].kind, TxTraceKind::SpanEnd as u8);
    let so = read_step_outcome(&records[2]);
    assert_eq!(so.variant, 2, "Done=2");
    assert_eq!(so.errno, 0);

    // 4. L2 SpanEnd (DriveEnd, result_kind=Ok)
    assert_eq!(records[3].kind, TxTraceKind::SpanEnd as u8);
    let de = read_drive_end(&records[3]);
    assert_eq!(de.result_kind, 0, "Ok=0");
    assert_eq!(de.errno, 0);
}

// ---------------------------------------------------------------------------
// Test 2: Continue → Done → 6 records (two step iterations)
// ---------------------------------------------------------------------------

#[test]
fn drive_emits_l4_step_outcome_continue_then_done() {
    let obs = TestPlatform::new().init();

    let op = MockStepOp::new([
        StepOutcome::Continue {
            progress: NoProgress,
        },
        StepOutcome::Done(7),
    ]);
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));
    assert_eq!(result, Ok(7));

    let records = obs.records();
    assert_eq!(
        records.len(),
        6,
        "expected 6 records (drive begin, step0 begin/end, step1 begin/end, drive end)"
    );

    // Step 0 end: variant=Continue (0)
    let so0 = read_step_outcome(&records[2]);
    assert_eq!(so0.variant, 0, "Continue=0");
    assert_eq!(so0.progress_kind, 0, "NoProgress=0");

    // Step 1 end: variant=Done (2)
    let so1 = read_step_outcome(&records[4]);
    assert_eq!(so1.variant, 2, "Done=2");
}

// ---------------------------------------------------------------------------
// Test 3: Err exits with linux_i32 errno on PayloadStepOutcome and DriveEnd
// ---------------------------------------------------------------------------

#[test]
fn drive_emits_l2_drive_end_err_on_step_err() {
    let obs = TestPlatform::new().init();

    let op = MockStepOp::new([StepOutcome::Err(Errno::ENOSYS)]);
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::ENOSYS));

    let records = obs.records();
    assert_eq!(records.len(), 4);

    // L4 step end: variant=Err (3), errno=ENOSYS linux value (38)
    let so = read_step_outcome(&records[2]);
    assert_eq!(so.variant, 3, "Err=3");
    assert_eq!(so.errno, 38, "ENOSYS linux value");

    // L2 drive end: result_kind=Err (1), errno=ENOSYS
    let de = read_drive_end(&records[3]);
    assert_eq!(de.result_kind, 1);
    assert_eq!(de.errno, 38);
}

// ---------------------------------------------------------------------------
// Test: TID flows from TaskMailbox into PayloadDriveBegin.task_id_low
// ---------------------------------------------------------------------------

#[test]
fn drive_picks_up_tid_from_mailbox_when_subject_missing() {
    use std::sync::Arc;
    use tx_substrate::wake::mailbox::TaskMailbox;

    let obs = TestPlatform::new().init();

    // Simulate tx-kernel's thread-future submit path: build a mailbox
    // with a non-zero TID and attach it to the ScriptCtx.
    let mailbox = Arc::new(TaskMailbox::new().with_task_id(0x1234));
    let mut ctx = ScriptCtx::<ProcessIdentity>::new().with_mailbox(mailbox);

    let op = MockStepOp::new([StepOutcome::Done(0)]);
    let _ = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));

    let records = obs.records();
    let db = read_drive_begin(&records[0]);
    assert_eq!(
        db.task_id_low, 0x1234,
        "PayloadDriveBegin.task_id_low must come from the mailbox TID, not the (empty) subject"
    );
}

// ---------------------------------------------------------------------------
// Test 4: L0 → L2 parent span linkage via per-hart slot
// ---------------------------------------------------------------------------

#[test]
fn drive_l2_links_to_l0_parent_via_per_hart_slot() {
    let obs = TestPlatform::new().init();

    // Simulate the syscall dispatcher: open a synthetic L0 span and
    // install it as the current parent before invoking drive.
    let l0_span = obs.emitter().span_begin(
        TxTraceLevel::Boundary,
        tx_observe::EventNameId::from_raw(0xdead_beef),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );
    let prev = tx_observe::set_current_parent_span(l0_span);

    let op = MockStepOp::new([StepOutcome::Done(1)]);
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let _ = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Waiting,
        None,
        None,
        None,
    ));

    // Close the L0 span and restore the prior parent slot.
    obs.emitter().span_end(l0_span, TxPayloadTag::None, &[]);
    tx_observe::set_current_parent_span(prev);

    let records = obs.records();
    // Records: L0 begin, L2 begin, L4 begin, L4 end, L2 end, L0 end.
    assert_eq!(records.len(), 6, "got {:?}", records.len());

    // L0 SpanBegin at [0]: parent = NONE
    assert_eq!(records[0].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[0].level, TxTraceLevel::Boundary as u8);
    assert_eq!(records[0].parent, tx_observe::SpanId::NONE.raw());
    let l0_span_id = records[0].span;

    // L2 SpanBegin at [1]: parent = L0's span id
    assert_eq!(records[1].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[1].level, TxTraceLevel::Drive as u8);
    assert_eq!(
        records[1].parent, l0_span_id,
        "L2 drive_begin must attach to the L0 syscall span via per-hart parent slot"
    );

    // L4 SpanBegin at [2]: parent = L2's span id
    assert_eq!(records[2].kind, TxTraceKind::SpanBegin as u8);
    assert_eq!(records[2].level, TxTraceLevel::Step as u8);
    assert_eq!(
        records[2].parent, records[1].span,
        "L4 step_begin must attach to the L2 drive span"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Nonblocking translation to EAGAIN emits drive end with errno=EAGAIN
// ---------------------------------------------------------------------------

#[test]
fn drive_emits_drive_end_eagain_on_nonblocking_yield() {
    let obs = TestPlatform::new().init();

    let op = MockStepOp::new([StepOutcome::Yield {
        progress: NoProgress,
        shape: YieldShape::OnWaitSource {
            source: WaitSourceId::new(7),
            interests: InterestMask::new(1),
        },
    }]);
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();
    let result = block_on(tx_scripts::drive(
        op,
        &mut ctx,
        DriveMode::Nonblocking,
        None,
        None,
        None,
    ));
    assert_eq!(result, Err(Errno::EAGAIN));

    let records = obs.records();
    // L2 begin, L4 begin, L4 end (Yield outcome), L2 end (Err EAGAIN)
    assert_eq!(records.len(), 4);

    let so = read_step_outcome(&records[2]);
    assert_eq!(so.variant, 1, "Yield=1");
    assert_eq!(so.shape_kind, 1, "OnWaitSource=1");

    let de = read_drive_end(&records[3]);
    assert_eq!(de.result_kind, 1, "Err");
    assert_eq!(de.errno, 11, "EAGAIN linux value");
}
