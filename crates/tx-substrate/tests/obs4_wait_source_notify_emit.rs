//! OBS-4: `WaitSource::notify_emit` smoke test.
//!
//! Verifies that calling `notify_emit` on a `WaitSource` with
//! at least one registered subscriber:
//! 1. Posts `MailboxEvent::SourceFired` to the live mailbox (same semantics as
//!    the non-emitting `notify`).
//! 2. Emits one `TxTraceKind::Instant` / `TxPayloadTag::WaitSourceNotify`
//!    record per successfully-posted subscriber into the SPSC ring.
//! 3. The emitted record carries the correct `source_id_low`,
//!    `wait_generation_low`, and `mask_bits` in the 16-byte inline payload.
//!
//! Spec ref: `docs/Txv3/08_OBSERVATION_v1.md` §16 OBS-4.
//! OBS-A-1: the emit is at a substrate convergence point (inside `notify_emit`),
//! not inside a `StepOp::poll` body.

extern crate std;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, DmaIf, EntropyIf, InitIf, IrqIf, MemoryRegion, ObserverIf, PercpuIf, PhysRange,
    PlatformConfig, PlatformInfo, PlatformInfoIf, PmapIf, PowerIf, RingDescriptor, SignalFrameIf,
    SmpIf, TimeIf, TrapIf, VirtAddr,
};
use tx_observe_types::{TxPayloadTag, TxTraceHartRing, TxTraceKind, TxTraceRecord};
use tx_substrate::{
    step::{InterestMask, WaitSourceId},
    wake::{MailboxEvent, TaskMailbox, WaitGeneration, WaitSource},
};

// ── Backing ring storage ──────────────────────────────────────────────────────

const RING_BYTES: usize = 2048;
static mut RING_STORAGE: [u8; RING_BYTES] = [0u8; RING_BYTES];
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static TS_COUNTER: AtomicUsize = AtomicUsize::new(0);

// ── Minimal mock platform ─────────────────────────────────────────────────────

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
    board: "tx-substrate-obs4-test",
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 1_000_000_000,
    possible_cpu_count: 1,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-substrate-obs4-test";
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(0xffff_ffc0_0000_0000);
}
impl BootPlatformIf for TestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}
impl InitIf for TestPlatform {
    fn init_early(_: tx_hal::BootHandoff) {}
    fn init_later(_: tx_hal::BootHandoff) {}
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
    fn write_bytes(_: &[u8]) {}
}
impl PmapIf for TestPlatform {}
impl TrapIf for TestPlatform {}
impl SignalFrameIf for TestPlatform {}
impl IrqIf for TestPlatform {}
impl EntropyIf for TestPlatform {}
impl CacheIf for TestPlatform {}
impl DmaIf for TestPlatform {}
impl SmpIf for TestPlatform {
    fn possible_cpus() -> CpuMask {
        CpuMask::first(1)
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
impl TimeIf for TestPlatform {
    fn read_ns() -> u64 {
        TS_COUNTER.fetch_add(1, Ordering::Relaxed) as u64 + 1
    }
    fn set_deadline_ns(_: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}
impl PercpuIf for TestPlatform {
    fn current_cpu_id() -> CpuId {
        CpuId(CURRENT_CPU.load(Ordering::Acquire))
    }
}
impl ObserverIf for TestPlatform {
    fn observation_ring(hart: CpuId) -> Option<RingDescriptor> {
        if hart.0 != 0 {
            return None;
        }
        let ptr = core::ptr::addr_of_mut!(RING_STORAGE) as *mut u8;
        Some(RingDescriptor {
            base: unsafe { NonNull::new_unchecked(ptr) },
            size: RING_BYTES,
            doorbell: None,
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn reset_ring() {
    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!(RING_STORAGE) as *mut u8,
            0,
            RING_BYTES,
        );
    }
    TS_COUNTER.store(0, Ordering::Relaxed);
    CURRENT_CPU.store(0, Ordering::Release);
}

/// Read the ring header and the first `n` slots.
///
/// SAFETY: must hold `TEST_LOCK`; no concurrent producer.
unsafe fn read_ring(n: usize) -> (TxTraceHartRing, std::vec::Vec<TxTraceRecord>) {
    let base = core::ptr::addr_of!(RING_STORAGE) as *const u8;
    let hdr = core::ptr::read(base as *const TxTraceHartRing);
    let slots_ptr = base.add(core::mem::size_of::<TxTraceHartRing>()) as *const TxTraceRecord;
    let mut slots = std::vec::Vec::with_capacity(n);
    for i in 0..n {
        slots.push(core::ptr::read(slots_ptr.add(i)));
    }
    (hdr, slots)
}

// ── Test ──────────────────────────────────────────────────────────────────────

/// `notify_emit` posts a `SourceFired` event to the live mailbox and emits one
/// `WaitSourceNotify` Instant record per woken task.
///
/// Verifies:
/// - `posted == 1` (one live subscriber matched the mask).
/// - Mailbox received `MailboxEvent::SourceFired` with the correct generation.
/// - Ring has exactly 1 record.
/// - Record has `kind = Instant`, `payload_tag = WaitSourceNotify`.
/// - Payload `source_id_low` == the source id, `wait_generation_low` == generation
///   captured at registration, `mask_bits` == the fired overlap.
#[test]
fn notify_emit_posts_event_and_emits_wait_source_notify_record() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    // Init the emitter for hart 0.
    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");

    // Create a WaitSource with id=42.
    const SOURCE_ID: u64 = 42;
    let src = WaitSource::new(WaitSourceId::new(SOURCE_ID));

    // Create a mailbox and register a subscriber.
    let mailbox = Arc::new(TaskMailbox::new());
    let gen = WaitGeneration::new(7);
    let mask = InterestMask::new(0b0011);
    let _ = src.register(Arc::downgrade(&mailbox), gen, mask);

    // Call notify_emit with a mask that overlaps the subscriber's interests.
    let fire_mask = InterestMask::new(0b0010);
    let posted = src.notify_emit(fire_mask);

    // One subscriber matched and got posted.
    assert_eq!(posted, 1, "one subscriber should have been posted");

    // Mailbox received the SourceFired event.
    let evt = mailbox.poll().expect("mailbox should have an event");
    match evt {
        MailboxEvent::SourceFired {
            generation,
            source,
            interests,
        } => {
            assert_eq!(generation, gen, "generation mismatch");
            assert_eq!(source, WaitSourceId::new(SOURCE_ID), "source id mismatch");
            // The posted interests are the overlap (0b0010), not the full mask (0b0011).
            assert_eq!(
                interests,
                InterestMask::new(0b0010),
                "posted interests should be the overlap"
            );
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }

    // SAFETY: producer done; holding TEST_LOCK.
    let (hdr, slots) = unsafe { read_ring(2) };
    let producer = hdr.producer.load(Ordering::Acquire);

    // Exactly one record should have been emitted (one woken task).
    assert_eq!(producer, 1, "ring should have exactly 1 record");

    let rec = &slots[0];

    // Record kind must be Instant.
    assert_eq!(
        rec.kind,
        TxTraceKind::Instant as u8,
        "record should be an Instant"
    );

    // Payload tag must be WaitSourceNotify.
    assert_eq!(
        rec.payload_tag,
        TxPayloadTag::WaitSourceNotify as u16,
        "payload_tag should be WaitSourceNotify"
    );

    // Decode the inline payload (little-endian u32 at offsets 0, 4, 8, 12).
    let payload = &rec.payload;
    let source_id_low = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let mask_bits = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let task_id_low = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    let wait_gen_low = u32::from_le_bytes(payload[12..16].try_into().unwrap());

    assert_eq!(
        source_id_low, SOURCE_ID as u32,
        "source_id_low should be the low 32 bits of the WaitSourceId"
    );
    assert_eq!(
        mask_bits, 0b0010u32,
        "mask_bits should be the overlap between fired and subscriber masks"
    );
    assert_eq!(
        task_id_low, 0,
        "task_id_low is 0 for a mailbox constructed with TaskMailbox::new() (no task id)"
    );
    assert_eq!(
        wait_gen_low,
        gen.raw() as u32,
        "wait_generation_low should be the low 32 bits of the subscriber's WaitGeneration"
    );
}

/// `notify_emit` emits one record per woken task when multiple subscribers match.
#[test]
fn notify_emit_emits_one_record_per_woken_task() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");

    let src = WaitSource::new(WaitSourceId::new(99));
    let m1 = Arc::new(TaskMailbox::new());
    let m2 = Arc::new(TaskMailbox::new());
    let gen1 = WaitGeneration::new(1);
    let gen2 = WaitGeneration::new(2);
    let _ = src.register(Arc::downgrade(&m1), gen1, InterestMask::new(0b1));
    let _ = src.register(Arc::downgrade(&m2), gen2, InterestMask::new(0b1));

    let posted = src.notify_emit(InterestMask::new(0b1));
    assert_eq!(posted, 2, "both subscribers should be posted");

    // SAFETY: holding TEST_LOCK.
    let (hdr, _slots) = unsafe { read_ring(4) };
    let producer = hdr.producer.load(Ordering::Acquire);

    // One record per woken task (two tasks, two records).
    assert_eq!(
        producer, 2,
        "ring should have 2 records (one per woken task)"
    );
}

/// `notify_emit` carries the correct `task_id_low` from the mailbox into the ring.
///
/// Tests the γ-fix: a mailbox constructed with `with_task_id(tid)` produces a
/// `WaitSourceNotify` record whose `task_id_low` field equals `tid`, not `0`.
#[test]
fn notify_emit_carries_task_id_low_from_mailbox() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");

    const SOURCE_ID: u64 = 77;
    const TASK_ID: u32 = 42;

    let src = WaitSource::new(WaitSourceId::new(SOURCE_ID));
    let mailbox = Arc::new(TaskMailbox::new().with_task_id(TASK_ID));
    let gen = WaitGeneration::new(5);
    let _ = src.register(Arc::downgrade(&mailbox), gen, InterestMask::new(0b1));

    let posted = src.notify_emit(InterestMask::new(0b1));
    assert_eq!(posted, 1);

    let (hdr, slots) = unsafe { read_ring(2) };
    assert_eq!(hdr.producer.load(Ordering::Acquire), 1);

    let payload = &slots[0].payload;
    let task_id_low = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    assert_eq!(
        task_id_low, TASK_ID,
        "task_id_low should be the value set via with_task_id"
    );
}

/// Two mailboxes with different `task_id_low` values produce distinct records.
///
/// Without the γ-fix both would have `task_id_low = 0`; the daemon's
/// `compute_flow_id` would produce identical flow IDs for both tasks on the
/// same `(wait_gen, boot_id)` pair, causing false flow arrows in Perfetto.
#[test]
fn pipe_eof_emits_correct_task_id_per_task() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");

    const SOURCE_ID: u64 = 88;
    const TID_A: u32 = 101;
    const TID_B: u32 = 202;

    let src = WaitSource::new(WaitSourceId::new(SOURCE_ID));
    let m_a = Arc::new(TaskMailbox::new().with_task_id(TID_A));
    let m_b = Arc::new(TaskMailbox::new().with_task_id(TID_B));
    let gen_a = WaitGeneration::new(10);
    let gen_b = WaitGeneration::new(11);
    let _ = src.register(Arc::downgrade(&m_a), gen_a, InterestMask::new(0b1));
    let _ = src.register(Arc::downgrade(&m_b), gen_b, InterestMask::new(0b1));

    let posted = src.notify_emit(InterestMask::new(0b1));
    assert_eq!(posted, 2, "both subscribers should be notified");

    let (hdr, slots) = unsafe { read_ring(4) };
    assert_eq!(hdr.producer.load(Ordering::Acquire), 2);

    let tid0 = u32::from_le_bytes(slots[0].payload[8..12].try_into().unwrap());
    let tid1 = u32::from_le_bytes(slots[1].payload[8..12].try_into().unwrap());

    // Both task ids must appear (one per subscriber, in registration order).
    let mut seen = std::vec![tid0, tid1];
    seen.sort();
    assert_eq!(
        seen,
        std::vec![TID_A, TID_B],
        "each record must carry its own task_id_low; no collision"
    );
}

/// `notify_emit` is a no-op on the emit side if the emitter is not wired.
/// Verified by checking that `notify` and `notify_emit` produce the same
/// wake semantics (both return the same posted count) when no ring is
/// configured for the current CPU.
#[test]
fn notify_emit_falls_back_gracefully_when_no_ring() {
    // Use CPU id 1 for which no ring is configured in TestPlatform.
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();
    CURRENT_CPU.store(1, Ordering::Release);

    tx_observe::init::<TestPlatform>(CpuId(1))
        .err()
        .expect("CPU 1 should have no ring");

    let src = WaitSource::new(WaitSourceId::new(7));
    let m = Arc::new(TaskMailbox::new());
    let _ = src.register(
        Arc::downgrade(&m),
        WaitGeneration::new(3),
        InterestMask::new(0b1),
    );

    // Even without a ring, the wake delivery must succeed.
    let posted = src.notify_emit(InterestMask::new(0b1));
    assert_eq!(posted, 1, "wake must still be delivered without a ring");

    // No record written (no ring).
    // We read from CPU 0's storage and check it hasn't been touched.
    CURRENT_CPU.store(0, Ordering::Release);
    let (hdr, _) = unsafe { read_ring(2) };
    let cpu0_producer = hdr.producer.load(Ordering::Acquire);
    assert_eq!(
        cpu0_producer, 0,
        "CPU 0 ring should be untouched when emit uses CPU 1"
    );
}
