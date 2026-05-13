//! OBS-3a drive smoke test.
//!
//! Verifies that calling `tx_scripts::drive::drive` against
//! a minimal `StepOp` produces the expected L2 + L4 observation records in the
//! synthetic SPSC ring.
//!
//! Record layout expected after one `drive` call on an op that completes in one
//! step (no yields):
//!
//!   slot 0: SpanBegin  level=Drive   (L2 drive begin)
//!   slot 1: SpanBegin  level=Step    (L4 step begin)
//!   slot 2: SpanEnd    level=Step    (L4 step end, PayloadStepOutcome)
//!   slot 3: SpanEnd    level=Drive   (L2 drive end, PayloadDriveEnd)
//!
//! Spec ref: `docs/Txv3/08_OBSERVATION_v1.md` §16 OBS-3a.

extern crate std;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, DmaIf, EntropyIf, InitIf, IrqIf, MemoryRegion, ObserverIf, PercpuIf, PhysRange,
    PlatformConfig, PlatformInfo, PlatformInfoIf, PmapIf, PowerIf, RingDescriptor, SignalFrameIf,
    SmpIf, TimeIf, TrapIf, VirtAddr,
};
use tx_observe_types::{TxTraceHartRing, TxTraceKind, TxTraceLevel, TxTraceRecord};
use tx_substrate::step_v3::{
    NoProgress, ProcessIdentity, ResumeKind, ScriptCtx, StepOp, StepOutcome, StepProgress,
    WireAbortReason, YieldOutcome, YieldResolved,
};

// ── Backing storage ──────────────────────────────────────────────────────────
//
// Same sizing as the OBS-2 smoke test: 2048 bytes gives 16 record slots.
const RING_BYTES: usize = 2048;
static mut RING_STORAGE: [u8; RING_BYTES] = [0u8; RING_BYTES];

// ── Mock platform ─────────────────────────────────────────────────────────────

static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static TS_COUNTER: AtomicUsize = AtomicUsize::new(0);

static BOOT_MEMORY: [MemoryRegion; 0] = [];
static BOOT_INFO: BootInfo = BootInfo {
    memory_regions: &BOOT_MEMORY,
    kernel_image: PhysRange {
        start: tx_hal::PhysAddr(0),
        size: 0,
    },
    initrd: None,
    cmdline: None,
};
static PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: "tx-scripts-drive-smoke",
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 1_000_000_000,
    possible_cpu_count: 1,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-scripts-drive-smoke";
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(0xffff_ffc0_0000_0000);
}

impl BootPlatformIf for TestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for TestPlatform {
    fn init_early(_handoff: tx_hal::BootHandoff) {}
    fn init_later(_handoff: tx_hal::BootHandoff) {}
}

impl BootInfoIf for TestPlatform {
    fn boot_info() -> &'static BootInfo {
        &BOOT_INFO
    }
}

impl PlatformInfoIf for TestPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &PLATFORM_INFO
    }
}

impl AuxvIf for TestPlatform {}
impl ConsoleIf for TestPlatform {
    fn write_bytes(_bytes: &[u8]) {}
}
impl PmapIf for TestPlatform {}
impl TrapIf for TestPlatform {}
impl SignalFrameIf for TestPlatform {}
impl IrqIf for TestPlatform {}
impl EntropyIf for TestPlatform {}

impl TimeIf for TestPlatform {
    fn read_ns() -> u64 {
        TS_COUNTER.fetch_add(1, Ordering::Relaxed) as u64 + 1
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        PLATFORM_INFO.timebase_frequency_hz
    }
}

impl PercpuIf for TestPlatform {
    fn current_cpu_id() -> CpuId {
        CpuId(CURRENT_CPU.load(Ordering::Acquire))
    }
}

impl CacheIf for TestPlatform {}
impl DmaIf for TestPlatform {}

impl SmpIf for TestPlatform {
    fn possible_cpus() -> CpuMask {
        CpuMask::first(PLATFORM_INFO.possible_cpu_count)
    }
    fn online_cpus() -> CpuMask {
        CpuMask::first(1)
    }
}

impl PowerIf for TestPlatform {
    fn system_off() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}

impl ObserverIf for TestPlatform {
    fn observation_ring(hart: CpuId) -> Option<RingDescriptor> {
        if hart.0 != 0 {
            return None;
        }
        let ptr = core::ptr::addr_of_mut!(RING_STORAGE) as *mut u8;
        let base = unsafe { NonNull::new_unchecked(ptr) };
        Some(RingDescriptor {
            base,
            size: RING_BYTES,
            doorbell: None,
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn reset_ring() {
    let ptr = core::ptr::addr_of_mut!(RING_STORAGE) as *mut u8;
    unsafe {
        core::ptr::write_bytes(ptr, 0, RING_BYTES);
    }
    TS_COUNTER.store(0, Ordering::Relaxed);
    CURRENT_CPU.store(0, Ordering::Release);
}

/// Read back the ring header + first N slots.
///
/// SAFETY: must hold `TEST_LOCK`; no concurrent producer.
unsafe fn read_ring() -> (TxTraceHartRing, std::vec::Vec<TxTraceRecord>) {
    read_ring_n(8)
}

/// Read back the ring header + first `n` slots.
///
/// SAFETY: must hold `TEST_LOCK`; no concurrent producer.
unsafe fn read_ring_n(n: usize) -> (TxTraceHartRing, std::vec::Vec<TxTraceRecord>) {
    let base = core::ptr::addr_of!(RING_STORAGE) as *const u8;
    let hdr = core::ptr::read(base as *const TxTraceHartRing);
    let slots_ptr = base.add(core::mem::size_of::<TxTraceHartRing>()) as *const TxTraceRecord;
    let mut slots = std::vec::Vec::with_capacity(n);
    for i in 0..n {
        slots.push(core::ptr::read(slots_ptr.add(i)));
    }
    (hdr, slots)
}

// ── Minimal test StepOps ─────────────────────────────────────────────────────

/// An op that completes in a single step with output `42u32`.
struct OneShotOp;

/// An op that yields once on `OnWaitSource` then completes with `77u32`.
///
/// Used by the OBS-3b yield/resume ring test below.
struct YieldOnceThenDoneOp {
    yielded: bool,
}

impl YieldOnceThenDoneOp {
    fn new() -> Self {
        Self { yielded: false }
    }
}

impl StepOp<ProcessIdentity> for OneShotOp {
    type Output = u32;
    type Progress = NoProgress;

    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<ProcessIdentity>,
    ) -> StepOutcome<Self::Output, Self::Progress> {
        StepOutcome::Done(42u32)
    }
}

impl StepOp<ProcessIdentity> for YieldOnceThenDoneOp {
    type Output = u32;
    type Progress = NoProgress;

    fn step(
        &mut self,
        _ctx: &mut ScriptCtx<ProcessIdentity>,
    ) -> StepOutcome<Self::Output, Self::Progress> {
        if self.yielded {
            StepOutcome::Done(77u32)
        } else {
            self.yielded = true;
            // Yield on a synthetic WaitSource (source_id=0xAB, interests=1).
            StepOutcome::yield_on_wait_source(NoProgress::EMPTY, 0xAB, 1)
        }
    }
}

// ── Test ──────────────────────────────────────────────────────────────────────

/// Calling `drive` on a single-step op emits L2 (Drive) and L4 (Step) records
/// in the expected order.
#[test]
fn drive_emits_l2_and_l4_records() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");

    let mut op = OneShotOp;
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();

    let outcome = tx_scripts::drive::drive(
        &mut op,
        &mut ctx,
        // TODO(α-followup): real metadata once wait substrate is in scope
        |_shape, _ctx| YieldOutcome::Resolved(YieldResolved::PLACEHOLDER),
    );

    // Op completed successfully — DriveOutcome::Done(42).
    assert!(
        matches!(outcome, tx_scripts::drive::DriveOutcome::Done(42u32)),
        "expected Done(42), got {:?}",
        outcome,
    );

    // SAFETY: producer done, holding TEST_LOCK, single-threaded.
    let (hdr, slots) = unsafe { read_ring() };

    let producer = hdr.producer.load(Ordering::Acquire);
    let lost = hdr.lost.load(Ordering::Acquire);

    // 4 records expected: L2-begin, L4-begin, L4-end, L2-end.
    assert_eq!(
        producer, 4,
        "expected 4 records (L2-begin, L4-begin, L4-end, L2-end)"
    );
    assert_eq!(lost, 0, "no ring overrun");

    // Slot 0: SpanBegin at Drive level (L2 begin).
    assert_eq!(
        slots[0].kind,
        TxTraceKind::SpanBegin as u8,
        "slot 0 should be SpanBegin"
    );
    assert_eq!(
        slots[0].level,
        TxTraceLevel::Drive as u8,
        "slot 0 should be at Drive level"
    );

    // Slot 1: SpanBegin at Step level (L4 begin).
    assert_eq!(
        slots[1].kind,
        TxTraceKind::SpanBegin as u8,
        "slot 1 should be SpanBegin"
    );
    assert_eq!(
        slots[1].level,
        TxTraceLevel::Step as u8,
        "slot 1 should be at Step level"
    );

    // Slot 2: SpanEnd at Boundary level (L4 end — span_end uses Boundary per HartEmitter::span_end).
    assert_eq!(
        slots[2].kind,
        TxTraceKind::SpanEnd as u8,
        "slot 2 should be SpanEnd"
    );
    // The L4 end span_id matches the L4 begin span_id.
    assert_eq!(
        slots[2].span, slots[1].span,
        "L4 SpanEnd references L4 SpanBegin span_id"
    );

    // Slot 3: SpanEnd at Boundary level (L2 end).
    assert_eq!(
        slots[3].kind,
        TxTraceKind::SpanEnd as u8,
        "slot 3 should be SpanEnd"
    );
    // The L2 end span_id matches the L2 begin span_id.
    assert_eq!(
        slots[3].span, slots[0].span,
        "L2 SpanEnd references L2 SpanBegin span_id"
    );

    // Span IDs for L2 and L4 are distinct.
    assert_ne!(
        slots[0].span, slots[1].span,
        "L2 and L4 spans must be distinct"
    );

    // Seq numbers are dense and increasing.
    for i in 0..4 {
        assert_eq!(slots[i].seq, i as u64, "seq slot {i} should be {i}");
        assert_eq!(slots[i].magic, 0x5254, "magic must be 'TR' in slot {i}");
    }
}

/// OBS-3b: calling `drive` on an op that yields once produces YieldBegin +
/// Resume records (L3) bracketing the wait, with Resume carrying the
/// `wait_generation` and `source_id` material from the `YieldResolved` returned
/// by the `yield_resolve` closure.
///
/// Expected ring layout (9 records):
///   slot 0: SpanBegin  level=Drive   (L2 drive begin)
///   slot 1: SpanBegin  level=Step    (L4 step-1 begin)
///   slot 2: SpanEnd    level=*       (L4 step-1 end)
///   slot 3: SpanBegin  level=Yield   (L3 yield begin)
///   slot 4: Instant    level=Yield   (L3 resume)
///   slot 5: SpanEnd    level=*       (L3 yield end)
///   slot 6: SpanBegin  level=Step    (L4 step-2 begin)
///   slot 7: SpanEnd    level=*       (L4 step-2 end)
///   slot 8: SpanEnd    level=Drive   (L2 drive end)
#[test]
fn drive_emits_l3_yield_begin_and_resume_records() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");

    let mut op = YieldOnceThenDoneOp::new();
    let mut ctx = ScriptCtx::<ProcessIdentity>::new();

    // Synthetic wake context: generation=5, source_id=0xAB.
    let test_gen = tx_substrate::wake::WaitGeneration::new(5);
    let test_source = tx_substrate::step_v3::WaitSourceId::new(0xAB);

    let outcome = tx_scripts::drive::drive(&mut op, &mut ctx, move |_shape, _ctx| {
        // Return real wake-context metadata so the Resume record is populated.
        YieldOutcome::Resolved(YieldResolved {
            wait_generation: test_gen,
            source_id: test_source,
            resume_kind: ResumeKind::Retry,
            abort_reason: WireAbortReason::None,
        })
    });

    // Op should have completed with 77.
    assert!(
        matches!(outcome, tx_scripts::drive::DriveOutcome::Done(77u32)),
        "expected Done(77), got {:?}",
        outcome,
    );

    // SAFETY: producer done, holding TEST_LOCK, single-threaded.
    let (hdr, slots) = unsafe { read_ring_n(12) };

    let producer = hdr.producer.load(Ordering::Acquire);
    let lost = hdr.lost.load(Ordering::Acquire);

    // 9 records expected (see layout in doc comment above).
    assert_eq!(
        producer, 9,
        "expected 9 records for a single-yield op; got {producer}"
    );
    assert_eq!(lost, 0, "no ring overrun");

    // All records must have valid magic.
    for i in 0..9 {
        assert_eq!(slots[i].magic, 0x5254, "magic must be 'TR' in slot {i}");
        assert_eq!(slots[i].seq, i as u64, "seq must be {i} in slot {i}");
    }

    // Slot 0: L2 SpanBegin (Drive).
    assert_eq!(
        slots[0].kind,
        TxTraceKind::SpanBegin as u8,
        "slot 0: SpanBegin"
    );
    assert_eq!(
        slots[0].level,
        TxTraceLevel::Drive as u8,
        "slot 0: Drive level"
    );

    // Slot 1: L4 SpanBegin (Step iteration 1).
    assert_eq!(
        slots[1].kind,
        TxTraceKind::SpanBegin as u8,
        "slot 1: SpanBegin (step-1)"
    );
    assert_eq!(
        slots[1].level,
        TxTraceLevel::Step as u8,
        "slot 1: Step level"
    );

    // Slot 2: L4 SpanEnd (step-1 end).
    assert_eq!(
        slots[2].kind,
        TxTraceKind::SpanEnd as u8,
        "slot 2: SpanEnd (step-1)"
    );
    assert_eq!(
        slots[2].span, slots[1].span,
        "slot 2: span_id matches step-1 begin"
    );

    // Slot 3: L3 SpanBegin (YieldBegin).
    assert_eq!(
        slots[3].kind,
        TxTraceKind::SpanBegin as u8,
        "slot 3: SpanBegin (YieldBegin)"
    );
    assert_eq!(
        slots[3].level,
        TxTraceLevel::Yield as u8,
        "slot 3: Yield level"
    );
    let yield_begin_span = slots[3].span;

    // Slot 4: L3 Instant (Resume).
    assert_eq!(
        slots[4].kind,
        TxTraceKind::Instant as u8,
        "slot 4: Instant (Resume)"
    );
    assert_eq!(
        slots[4].level,
        TxTraceLevel::Yield as u8,
        "slot 4: Yield level"
    );
    // Resume carries `parent = yield_span`.
    assert_eq!(
        slots[4].parent, yield_begin_span,
        "slot 4: parent references yield span"
    );

    // Slot 5: L3 SpanEnd (yield end).
    assert_eq!(
        slots[5].kind,
        TxTraceKind::SpanEnd as u8,
        "slot 5: SpanEnd (yield)"
    );
    assert_eq!(
        slots[5].span, yield_begin_span,
        "slot 5: span_id matches YieldBegin span"
    );

    // Slot 6: L4 SpanBegin (Step iteration 2).
    assert_eq!(
        slots[6].kind,
        TxTraceKind::SpanBegin as u8,
        "slot 6: SpanBegin (step-2)"
    );
    assert_eq!(
        slots[6].level,
        TxTraceLevel::Step as u8,
        "slot 6: Step level"
    );

    // Slot 7: L4 SpanEnd (step-2 end).
    assert_eq!(
        slots[7].kind,
        TxTraceKind::SpanEnd as u8,
        "slot 7: SpanEnd (step-2)"
    );
    assert_eq!(
        slots[7].span, slots[6].span,
        "slot 7: span_id matches step-2 begin"
    );

    // Slot 8: L2 SpanEnd (Drive end).
    assert_eq!(
        slots[8].kind,
        TxTraceKind::SpanEnd as u8,
        "slot 8: SpanEnd (Drive)"
    );
    assert_eq!(
        slots[8].span, slots[0].span,
        "slot 8: span_id matches Drive begin"
    );

    // YieldBegin span is distinct from Drive and Step spans.
    assert_ne!(yield_begin_span, slots[0].span, "yield span != drive span");
    assert_ne!(yield_begin_span, slots[1].span, "yield span != step-1 span");
    assert_ne!(yield_begin_span, slots[6].span, "yield span != step-2 span");
}
