//! OBS-8: `Index::commit` L6 `MutationIndexCommit` emit test.
//!
//! Verifies that calling `IndexReservation::commit` with
//! `INDEX_MUTATION_EMIT_ENABLED = true` emits exactly one
//! `TxTraceKind::Instant` / `TxPayloadTag::MutationIndexCommit` record.
//!
//! The record must carry:
//! - `kind = Instant`
//! - `level = Mutation (6)`
//! - `payload_tag = MutationIndexCommit (51)`
//!
//! Spec ref: `docs/Txv3/08_OBSERVATION_v1.md` §16 OBS-8, §6 L6.
//! OBS-A-1: the emit is at a substrate convergence point
//! (`IndexReservation::commit`), not inside a `StepOp::step` body.

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
use tx_substrate::index::{Index, INDEX_MUTATION_EMIT_ENABLED};

// ── Backing ring storage ──────────────────────────────────────────────────────

const RING_BYTES: usize = 4096;
#[repr(align(8))]
struct AlignedRingStorage([u8; RING_BYTES]);

static mut RING_STORAGE: AlignedRingStorage = AlignedRingStorage([0u8; RING_BYTES]);

/// Serialise all tests: they share ring storage and emitter state.
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static TS_COUNTER: AtomicUsize = AtomicUsize::new(0);

// ── Mock platform ─────────────────────────────────────────────────────────────

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
    board: "tx-substrate-obs8-index-test",
    spi_sd: None,
    mmio_regions: &[],
    device_resources: &tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH,
    timebase_frequency_hz: 1_000_000_000,
    possible_cpu_count: 1,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-substrate-obs8-index-test";
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
    tx_observe::init::<TestPlatform>(CpuId(0)).expect("observe init");
    guard
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// When `INDEX_MUTATION_EMIT_ENABLED` is set, `IndexReservation::commit`
/// emits one `Instant / MutationIndexCommit` record.
#[test]
fn index_commit_emits_mutation_index_commit_record_when_gate_enabled() {
    let _guard = setup();
    INDEX_MUTATION_EMIT_ENABLED.store(true, Ordering::Release);

    let index = Index::<u32, u64, 4>::new();
    let reservation = index.reserve(42u32).expect("reserve key 42");
    reservation.commit(0xABCD_EF01u64);

    // SAFETY: producer done; holding TEST_LOCK.
    let (hdr, slots) = unsafe { read_ring(4) };
    let producer = hdr.producer.load(Ordering::Acquire);

    assert_eq!(producer, 1, "ring should have exactly 1 record");

    let rec = &slots[0];

    // Kind: Instant.
    assert_eq!(
        rec.kind,
        TxTraceKind::Instant as u8,
        "record kind should be Instant"
    );

    // Level: Mutation (6).
    assert_eq!(
        rec.level,
        TxTraceLevel::Mutation as u8,
        "record level should be Mutation"
    );

    // Payload tag: MutationIndexCommit (51).
    assert_eq!(
        rec.payload_tag,
        TxPayloadTag::MutationIndexCommit as u16,
        "payload_tag should be MutationIndexCommit"
    );

    // Payload length: 16 bytes.
    assert_eq!(
        rec.payload_len,
        core::mem::size_of::<tx_observe_types::PayloadMutationIndexCommit>() as u16,
        "payload_len should equal sizeof PayloadMutationIndexCommit"
    );

    // Reset gate.
    INDEX_MUTATION_EMIT_ENABLED.store(false, Ordering::Release);
}

/// When gate is disabled (default), no emit happens.
#[test]
fn index_commit_does_not_emit_when_gate_disabled() {
    let _guard = setup();
    INDEX_MUTATION_EMIT_ENABLED.store(false, Ordering::Release);

    let index = Index::<u32, u64, 4>::new();
    let reservation = index.reserve(7u32).expect("reserve");
    reservation.commit(99u64);

    // SAFETY: holding TEST_LOCK.
    let (hdr, _) = unsafe { read_ring(2) };
    let producer = hdr.producer.load(Ordering::Acquire);

    assert_eq!(producer, 0, "ring should be empty when gate is disabled");
}
