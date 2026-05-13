//! OBS-2 smoke test.
//!
//! Drives the producer side of the SPSC ring against a synthetic
//! `RingDescriptor` backed by a `static mut` byte buffer, then memcpy-decodes
//! the resulting `TxTraceRecord` slots and asserts:
//!
//! - per-record magic / version
//! - kind matches the emit method called
//! - span_id encoding (hart_id in bits 56..64, local counter in bits 0..56)
//! - producer cursor advances monotonically
//! - seq numbers are dense and monotone within a hart
//! - no torn records (full 80-byte memcpy decodes cleanly)
//!
//! Spec ref: `08_OBSERVATION_v1.md` §16 OBS-2.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, DmaIf, EntropyIf, InitIf, IrqIf, MemoryRegion, ObserverIf, PercpuIf, PhysRange,
    PlatformConfig, PlatformInfo, PlatformInfoIf, PmapIf, PowerIf, RingDescriptor, SignalFrameIf,
    SmpIf, TimeIf, TrapIf, VirtAddr,
};
use tx_observe_types::{TxPayloadTag, TxTraceHartRing, TxTraceKind, TxTraceLevel, TxTraceRecord};

// ── Backing storage ─────────────────────────────────────────────────────────
//
// Ring layout: 208 byte header + 16 slots * 80 bytes = 1488 bytes.  Round up
// to the next power of two (`init` requires `desc.size.is_power_of_two()`).
const RING_BYTES: usize = 2048;
static mut RING_STORAGE: [u8; RING_BYTES] = [0u8; RING_BYTES];

// ── Mock platform ───────────────────────────────────────────────────────────

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
    board: "tx-observe-smoke",
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 1_000_000_000,
    possible_cpu_count: 1,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-observe-smoke";
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
        // Monotone, incremented per call — guarantees timestamps differ.
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
        // SAFETY: this test holds `TEST_LOCK` for the duration of `init` and
        // the emit operations.  The buffer is `static mut`, accessed only
        // through the returned `RingDescriptor` and only by the single hart
        // that owns it.  `addr_of_mut!` avoids materialising an intermediate
        // `&mut [u8; N]` reference, which would trip the Rust 2024
        // static-mut-ref lint.
        let ptr = core::ptr::addr_of_mut!(RING_STORAGE) as *mut u8;
        let base = unsafe { NonNull::new_unchecked(ptr) };
        Some(RingDescriptor {
            base,
            size: RING_BYTES,
            doorbell: None,
        })
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn reset_ring() {
    let ptr = core::ptr::addr_of_mut!(RING_STORAGE) as *mut u8;
    unsafe {
        core::ptr::write_bytes(ptr, 0, RING_BYTES);
    }
    TS_COUNTER.store(0, Ordering::Relaxed);
    CURRENT_CPU.store(0, Ordering::Release);
}

/// Read back the ring header and slot array from the static buffer.
///
/// SAFETY: must be called with `TEST_LOCK` held; no concurrent producer.
unsafe fn read_ring() -> (TxTraceHartRing, Vec<TxTraceRecord>) {
    let base = core::ptr::addr_of!(RING_STORAGE) as *const u8;
    let hdr = core::ptr::read(base as *const TxTraceHartRing);
    let slots_ptr = base.add(core::mem::size_of::<TxTraceHartRing>()) as *const TxTraceRecord;
    // 1840 / 80 = 23 → prev_power_of_two(23) = 16.
    let mut slots = Vec::with_capacity(16);
    for i in 0..16 {
        slots.push(core::ptr::read(slots_ptr.add(i)));
    }
    (hdr, slots)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn smoke_span_begin_end_roundtrip() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("init");
    let emitter = tx_observe::current().expect("emitter");

    let span = emitter.span_begin(
        TxTraceLevel::Drive,
        tx_observe::EventNameId::from_raw(0xAAAA_AAAA),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );
    emitter.span_end(span, TxPayloadTag::None, &[]);

    // SAFETY: producer is done; we hold the lock; read is single-threaded.
    let (hdr, slots) = unsafe { read_ring() };

    let producer = hdr.producer.load(Ordering::Acquire);
    let consumer = hdr.consumer.load(Ordering::Acquire);
    let lost = hdr.lost.load(Ordering::Acquire);

    assert_eq!(producer, 2, "two records written");
    assert_eq!(consumer, 0, "no consumer in test");
    assert_eq!(lost, 0, "no overrun");
    assert_eq!(hdr.hart_id, 0);

    // Slot 0 — SpanBegin
    let r0 = &slots[0];
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
    assert!(span_raw & 0x00FF_FFFF_FFFF_FFFF > 0, "local counter nonzero");

    // Slot 1 — SpanEnd
    let r1 = &slots[1];
    assert_eq!(r1.magic, 0x5254, "slot 1 magic 'TR'");
    assert_eq!(r1.kind, TxTraceKind::SpanEnd as u8);
    assert_eq!(r1.seq, 1, "seq advances");
    assert_eq!(r1.span, span_raw, "SpanEnd references the same span_id");
    assert!(r1.ts >= r0.ts, "monotone timestamps");

    // Subsequent slots untouched — magic stays 0 (zero-fill from init).
    assert_eq!(slots[2].magic, 0);
    assert_eq!(slots[2].kind, TxTraceKind::Nop as u8);
}

#[test]
fn smoke_instant_and_counter() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("init");
    let emitter = tx_observe::current().expect("emitter");

    emitter.instant(
        TxTraceLevel::Boundary,
        tx_observe::EventNameId::from_raw(0xBBBB),
        tx_observe::SpanId::NONE,
        TxPayloadTag::None,
        &[],
    );
    emitter.counter(tx_observe::EventNameId::from_raw(0xCCCC), 42);

    let (hdr, slots) = unsafe { read_ring() };
    assert_eq!(hdr.producer.load(Ordering::Acquire), 2);

    assert_eq!(slots[0].kind, TxTraceKind::Instant as u8);
    assert_eq!(slots[0].level, TxTraceLevel::Boundary as u8);
    assert_eq!(slots[0].name, 0xBBBB);

    assert_eq!(slots[1].kind, TxTraceKind::Counter as u8);
    assert_eq!(slots[1].name, 0xCCCC);
}

#[test]
fn smoke_overrun_increments_lost_counter() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("init");
    let emitter = tx_observe::current().expect("emitter");

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

    let (hdr, _slots) = unsafe { read_ring() };
    let producer = hdr.producer.load(Ordering::Acquire);
    let lost = hdr.lost.load(Ordering::Acquire);

    assert_eq!(producer, 16, "producer caps at slot_count on overrun");
    assert_eq!(lost, 1, "one record dropped");
}

#[test]
fn smoke_span_ids_unique_per_emit() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    reset_ring();

    tx_observe::init::<TestPlatform>(CpuId(0)).expect("init");
    let emitter = tx_observe::current().expect("emitter");

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
