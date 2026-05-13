//! OBS-4 production-path integration test (D16 resolved).
//!
//! Exercises `WaitSource::notify_emit` at the semantic pattern of a
//! **close-side convergence point** — the same shape as `PipePayload::decr_reader`
//! and `PipePayload::decr_writer` now use after D16 migrated all
//! TODO(OBS-4-followup) sites.
//!
//! Specifically tests:
//! 1. A `WaitSource` is notified via `notify_emit` (not `notify`) at a
//!    synthetic "last writer closed, wake readers" convergence point.
//! 2. The mailbox receives the expected `MailboxEvent::SourceFired` event.
//! 3. Exactly one `TxTraceKind::Instant` / `TxPayloadTag::WaitSourceNotify`
//!    record lands in the SPSC ring with correct `source_id_low`,
//!    `wait_generation_low`, and `mask_bits`.
//!
//! The platform context is implicit: `tx_observe::init` installs the cpu-id
//! function pointer at boot; `notify_emit` calls `tx_observe::current()`
//! without a `P` type parameter (D16 Option C).
//!
//! Spec ref: `docs/Txv3/08_OBSERVATION_v1.md` §16 OBS-4.
//! OBS-A-1 compliance: the emit is at a substrate convergence point, not
//! inside a `StepOp::step()` body.

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
static mut RING_STORAGE2: [u8; RING_BYTES] = [0u8; RING_BYTES];
static TEST_LOCK2: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CURRENT_CPU2: AtomicUsize = AtomicUsize::new(0);
static TS_COUNTER2: AtomicUsize = AtomicUsize::new(0);

// ── Minimal mock platform ─────────────────────────────────────────────────────

static BOOT_MEMORY2: [MemoryRegion; 0] = [];
static BOOT_INFO2: BootInfo = BootInfo {
    memory_regions: &BOOT_MEMORY2,
    kernel_image: PhysRange {
        start: tx_hal::PhysAddr(0),
        size: 0,
    },
    initrd: None,
    cmdline: None,
};
static PLATFORM_INFO2: PlatformInfo = PlatformInfo {
    board: "tx-substrate-obs4-conv-test",
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 1_000_000_000,
    possible_cpu_count: 1,
};

struct TestPlatform2;

impl PlatformConfig for TestPlatform2 {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-substrate-obs4-conv-test";
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(0xffff_ffc0_0000_0000);
}
impl BootPlatformIf for TestPlatform2 {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}
impl InitIf for TestPlatform2 {
    fn init_early(_: tx_hal::BootHandoff) {}
    fn init_later(_: tx_hal::BootHandoff) {}
}
impl BootInfoIf for TestPlatform2 {
    fn boot_info() -> &'static BootInfo {
        &BOOT_INFO2
    }
}
impl PlatformInfoIf for TestPlatform2 {
    fn platform_info() -> &'static PlatformInfo {
        &PLATFORM_INFO2
    }
}
impl AuxvIf for TestPlatform2 {}
impl ConsoleIf for TestPlatform2 {
    fn write_bytes(_: &[u8]) {}
}
impl PmapIf for TestPlatform2 {}
impl TrapIf for TestPlatform2 {}
impl SignalFrameIf for TestPlatform2 {}
impl IrqIf for TestPlatform2 {}
impl EntropyIf for TestPlatform2 {}
impl CacheIf for TestPlatform2 {}
impl DmaIf for TestPlatform2 {}
impl SmpIf for TestPlatform2 {
    fn possible_cpus() -> CpuMask {
        CpuMask::first(1)
    }
    fn online_cpus() -> CpuMask {
        CpuMask::first(1)
    }
}
impl PowerIf for TestPlatform2 {
    fn system_off() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}
impl TimeIf for TestPlatform2 {
    fn read_ns() -> u64 {
        TS_COUNTER2.fetch_add(1, Ordering::Relaxed) as u64 + 1
    }
    fn set_deadline_ns(_: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}
impl PercpuIf for TestPlatform2 {
    fn current_cpu_id() -> CpuId {
        CpuId(CURRENT_CPU2.load(Ordering::Acquire))
    }
}
impl ObserverIf for TestPlatform2 {
    fn observation_ring(hart: CpuId) -> Option<RingDescriptor> {
        if hart.0 != 0 {
            return None;
        }
        let ptr = core::ptr::addr_of_mut!(RING_STORAGE2) as *mut u8;
        Some(RingDescriptor {
            base: unsafe { NonNull::new_unchecked(ptr) },
            size: RING_BYTES,
            doorbell: None,
        })
    }
}

fn reset_ring() {
    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!(RING_STORAGE2) as *mut u8,
            0,
            RING_BYTES,
        );
    }
    TS_COUNTER2.store(0, Ordering::Relaxed);
    CURRENT_CPU2.store(0, Ordering::Release);
}

/// Read the ring header and the first `n` slots.
/// SAFETY: must hold `TEST_LOCK2`; no concurrent producer.
unsafe fn read_ring(n: usize) -> (TxTraceHartRing, std::vec::Vec<TxTraceRecord>) {
    let base = core::ptr::addr_of!(RING_STORAGE2) as *const u8;
    let hdr = core::ptr::read(base as *const TxTraceHartRing);
    let slots_ptr = base.add(core::mem::size_of::<TxTraceHartRing>()) as *const TxTraceRecord;
    let mut slots = std::vec::Vec::with_capacity(n);
    for i in 0..n {
        slots.push(core::ptr::read(slots_ptr.add(i)));
    }
    (hdr, slots)
}

/// Helper: simulate the "last writer closed → wake readers" convergence point.
///
/// This mirrors the post-migration shape of `PipePayload::decr_writer`
/// (D16 resolved): calls `reader_wait_source.notify_emit(PIPE_READABLE)`
/// instead of `notify`. The platform context is implicit via the cpu-id
/// function pointer installed by `tx_observe::init` at boot.
///
/// OBS-A-1 compliance: this is a substrate convergence point function (the
/// structural equivalent of the close path), NOT inside a `StepOp::step()` body.
fn simulate_last_writer_close(
    reader_wait_source: &WaitSource,
    readable_mask: InterestMask,
) -> usize {
    // In production: reader_count.fetch_sub(1) then fire if prev==1.
    // Here we directly invoke the convergence-point notify_emit.
    reader_wait_source.notify_emit(readable_mask)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Simulated close-path convergence point: `notify_emit` at a "last writer
/// closed" site produces a `WaitSourceNotify` record and delivers
/// `MailboxEvent::SourceFired` to the waiting reader.
///
/// This is the production observable that OBS-4 targets for the pipe close
/// path once the TODO(OBS-4-followup) migrations in `pipe.rs` are completed.
#[test]
fn convergence_close_path_emits_wait_source_notify_record() {
    let _guard = TEST_LOCK2.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform2>(CpuId(0)).expect("observe init");

    // Source id chosen to avoid collision with obs4_wait_source_notify_emit tests.
    const SOURCE_ID: u64 = 200;
    const READABLE_MASK: u64 = 0b0001; // PIPE_READABLE equivalent

    let src = WaitSource::new(WaitSourceId::new(SOURCE_ID));
    let reader_mailbox = Arc::new(TaskMailbox::new());
    let gen = WaitGeneration::new(3);
    let _ = src.register(
        Arc::downgrade(&reader_mailbox),
        gen,
        InterestMask::new(READABLE_MASK),
    );

    // Invoke the convergence-point helper (simulates the post-migration
    // decr_writer body).
    let posted = simulate_last_writer_close(&src, InterestMask::new(READABLE_MASK));
    assert_eq!(posted, 1, "one reader should be woken");

    // Reader mailbox received SourceFired.
    let evt = reader_mailbox.poll().expect("mailbox should have an event");
    match evt {
        MailboxEvent::SourceFired {
            generation,
            source,
            interests,
        } => {
            assert_eq!(generation, gen, "generation mismatch");
            assert_eq!(source, WaitSourceId::new(SOURCE_ID), "source id mismatch");
            assert_eq!(
                interests,
                InterestMask::new(READABLE_MASK),
                "interest mask mismatch"
            );
        }
        other => panic!("expected SourceFired, got {other:?}"),
    }

    // Ring has exactly one WaitSourceNotify record.
    let (hdr, slots) = unsafe { read_ring(2) };
    let producer = hdr.producer.load(Ordering::Acquire);
    assert_eq!(producer, 1, "ring should have exactly 1 record");

    let rec = &slots[0];
    assert_eq!(
        rec.kind,
        TxTraceKind::Instant as u8,
        "record should be an Instant"
    );
    assert_eq!(
        rec.payload_tag,
        TxPayloadTag::WaitSourceNotify as u16,
        "payload_tag should be WaitSourceNotify"
    );

    let payload = &rec.payload;
    let source_id_low = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let mask_bits = u32::from_le_bytes(payload[4..8].try_into().unwrap());
    let task_id_low = u32::from_le_bytes(payload[8..12].try_into().unwrap());
    let wait_gen_low = u32::from_le_bytes(payload[12..16].try_into().unwrap());

    assert_eq!(source_id_low, SOURCE_ID as u32, "source_id_low mismatch");
    assert_eq!(
        mask_bits, READABLE_MASK as u32,
        "mask_bits should be the readable interest"
    );
    assert_eq!(
        task_id_low, 0,
        "task_id_low is 0 for a mailbox constructed with TaskMailbox::new() (no task id)"
    );
    assert_eq!(
        wait_gen_low,
        gen.raw() as u32,
        "wait_generation_low should match subscriber's generation"
    );
}
