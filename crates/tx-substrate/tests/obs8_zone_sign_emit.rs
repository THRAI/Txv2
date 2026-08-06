//! OBS-8: `zone::sign` L6 `MutationZoneSign` emit test.
//!
//! Verifies that calling `zone::sign` (via `sign_for`) with
//! `MUTATION_EMIT_ENABLED = true` emits exactly one
//! `TxTraceKind::Instant` / `TxPayloadTag::MutationZoneSign` record
//! into the per-hart ring.
//!
//! The record must carry:
//! - `kind = Instant`
//! - `level = Mutation (6)`
//! - `payload_tag = MutationZoneSign (50)`
//! - `payload.object_id` matching the cap's `trace_id()`
//!
//! Spec ref: `docs/Txv3/08_OBSERVATION_v1.md` §16 OBS-8, §6 L6.
//! OBS-A-1: the emit is at a substrate convergence point (`zone::sign`),
//! not inside a `StepOp::step` body.

extern crate std;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, DeadlineTimerIf, DmaIf, EntropyIf, InitIf, IrqIf, MemoryRegion, MonotonicCounterIf,
    ObserverIf, PercpuIf, PersistentClockIf, PhysRange, PlatformConfig, PlatformInfo,
    PlatformInfoIf, PmapIf, PowerIf, RingDescriptor, SignalFrameIf, SmpIf, TrapIf, VirtAddr,
};
use tx_observe_types::{TxPayloadTag, TxTraceHartRing, TxTraceKind, TxTraceLevel, TxTraceRecord};
use tx_substrate::{
    epoch,
    zone::{self, Zone, ZoneAllocated, MUTATION_EMIT_ENABLED},
};

// ── Backing ring storage ──────────────────────────────────────────────────────

const RING_BYTES: usize = 4096;
#[repr(align(8))]
struct AlignedRingStorage([u8; RING_BYTES]);

static mut RING_STORAGE: AlignedRingStorage = AlignedRingStorage([0u8; RING_BYTES]);

/// Serialise all tests in this file: they share `RING_STORAGE`, the
/// observation emitter state, and the zone registry.
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
    board: "tx-substrate-obs8-zone-test",
    spi_sd: None,
    mmio_regions: &[],
    device_resources: &tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH,
    timebase_frequency_hz: 1_000_000_000,
    possible_cpu_count: 1,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-substrate-obs8-zone-test";
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
unsafe fn restore_test_local_execution(_: usize) {}
impl IrqIf for TestPlatform {
    fn exclude_local_execution() -> tx_hal::LocalExecutionGuard {
        unsafe { tx_hal::LocalExecutionGuard::new(0, restore_test_local_execution) }
    }
}
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
impl MonotonicCounterIf for TestPlatform {
    fn read_ns() -> u64 {
        TS_COUNTER.fetch_add(1, Ordering::Relaxed) as u64 + 1
    }

    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl DeadlineTimerIf for TestPlatform {
    fn set_deadline_ns(_: u64) {}

    fn cancel_deadline() {}
}
impl PersistentClockIf for TestPlatform {}
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
        let ptr = unsafe { core::ptr::addr_of_mut!(RING_STORAGE.0) as *mut u8 };
        Some(RingDescriptor {
            base: unsafe { NonNull::new_unchecked(ptr) },
            size: RING_BYTES,
            doorbell: None,
        })
    }
}

// ── Test object type ──────────────────────────────────────────────────────────

struct SignTestObj {
    #[allow(dead_code)]
    val: u64,
}

static SIGN_TEST_ZONE: Zone<SignTestObj> = Zone::const_new();

unsafe impl ZoneAllocated for SignTestObj {
    fn zone() -> &'static Zone<Self> {
        &SIGN_TEST_ZONE
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn reset_ring() {
    unsafe {
        core::ptr::write_bytes(
            core::ptr::addr_of_mut!(RING_STORAGE.0) as *mut u8,
            0,
            RING_BYTES,
        );
    }
    TS_COUNTER.store(0, Ordering::Relaxed);
    CURRENT_CPU.store(0, Ordering::Release);
}

/// Read the ring header and the first `n` record slots.
///
/// SAFETY: must hold `TEST_LOCK`; no concurrent producer.
unsafe fn read_ring(n: usize) -> (TxTraceHartRing, std::vec::Vec<TxTraceRecord>) {
    let base = core::ptr::addr_of!(RING_STORAGE.0) as *const u8;
    let hdr = core::ptr::read(base as *const TxTraceHartRing);
    let slots_ptr = base.add(core::mem::size_of::<TxTraceHartRing>()) as *const TxTraceRecord;
    let mut slots = std::vec::Vec::with_capacity(n);
    for i in 0..n {
        slots.push(core::ptr::read(slots_ptr.add(i)));
    }
    (hdr, slots)
}

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset_ring();
    CURRENT_CPU.store(0, Ordering::Release);

    // Init observation for hart 0.
    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");

    // Init zone substrate.
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
    epoch::testing::init_for_test();
    zone::testing::init_for_test(
        4096,
        tx_substrate::page_allocator::testing::direct_map_base_for_test(),
    )
    .expect("zone init");
    zone::register_zone_for::<SignTestObj>().expect("register zone");

    guard
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// When `MUTATION_EMIT_ENABLED` is set, `zone::sign` emits one
/// `Instant / MutationZoneSign` record whose payload encodes the cap's
/// `trace_id()`.
#[test]
fn zone_sign_emits_mutation_zone_sign_record_when_gate_enabled() {
    let _guard = setup();
    MUTATION_EMIT_ENABLED.store(true, Ordering::Release);

    let reservation = zone::reserve_for::<SignTestObj>().expect("reserve");
    let cap = zone::sign_for(reservation, SignTestObj { val: 0xDEAD_BEEF });

    // Read back the expected trace_id before the cap is dropped.
    let expected_trace_id = cap.trace_id();

    // SAFETY: producer done; holding TEST_LOCK.
    let (hdr, slots) = unsafe { read_ring(4) };
    let producer = hdr.producer.load(Ordering::Acquire);

    // Exactly one record from zone::sign.
    assert_eq!(
        producer, 1,
        "ring should have exactly 1 record from zone::sign"
    );

    let rec = &slots[0];

    // Kind must be Instant.
    assert_eq!(
        rec.kind,
        TxTraceKind::Instant as u8,
        "record kind should be Instant"
    );

    // Level must be Mutation (6).
    assert_eq!(
        rec.level,
        TxTraceLevel::Mutation as u8,
        "record level should be Mutation"
    );

    // Payload tag must be MutationZoneSign (50).
    assert_eq!(
        rec.payload_tag,
        TxPayloadTag::MutationZoneSign as u16,
        "payload_tag should be MutationZoneSign"
    );

    // Decode object_id from the payload (little-endian u64 at offset 0).
    let object_id = u64::from_le_bytes(rec.payload[0..8].try_into().unwrap());
    assert_eq!(
        object_id, expected_trace_id,
        "payload object_id should equal cap.trace_id()"
    );

    // Decode kind byte from offset 8.
    let kind_byte = rec.payload[8];
    let expected_kind = (expected_trace_id >> 56) as u8;
    assert_eq!(
        kind_byte, expected_kind,
        "payload kind byte should match the high-byte of trace_id"
    );

    // Reset gate.
    MUTATION_EMIT_ENABLED.store(false, Ordering::Release);
}

/// When `MUTATION_EMIT_ENABLED` is `false` (default), `zone::sign` does
/// not emit any record.
#[test]
fn zone_sign_does_not_emit_when_gate_disabled() {
    let _guard = setup();
    MUTATION_EMIT_ENABLED.store(false, Ordering::Release);

    let reservation = zone::reserve_for::<SignTestObj>().expect("reserve");
    let _cap = zone::sign_for(reservation, SignTestObj { val: 42 });

    // SAFETY: producer done; holding TEST_LOCK.
    let (hdr, _) = unsafe { read_ring(2) };
    let producer = hdr.producer.load(Ordering::Acquire);

    assert_eq!(producer, 0, "ring should be empty when gate is disabled");
}
