//! Reusable test helpers for the observation subsystem.
//!
//! This module provides [`TestPlatform`] / [`TestObservation`], a RAII
//! harness that sets up a synthetic in-process observation ring, drives
//! emit operations through the production `tx-observe` code, and makes it
//! easy to assert on the resulting records — without copy-pasting 150+ lines
//! of `TestPlatform` boilerplate into every test file.
//!
//! # Usage
//!
//! ```ignore
//! use tx_observe::testing::TestPlatform;
//!
//! #[test]
//! fn my_test() {
//!     let obs = TestPlatform::new().init();
//!     obs.emitter().instant(
//!         tx_observe::TxTraceLevel::Drive,
//!         tx_observe::EventNameId::from_raw(0x1),
//!         tx_observe::SpanId::NONE,
//!         tx_observe_types::TxPayloadTag::None,
//!         &[],
//!     );
//!     let records = obs.records();
//!     assert_eq!(records.len(), 1);
//! }
//! ```
//!
//! Tests that use this module serialize on a crate-global spin lock so
//! concurrent test runs do not race on the observation statics.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, DmaIf, EntropyIf, InitIf, IrqIf, MemoryRegion, MonotonicCounterIf, ObserverIf,
    PercpuIf, PhysRange, PlatformConfig, PlatformInfo, PlatformInfoIf, PmapIf, PowerIf,
    RingDescriptor, SignalFrameIf, SmpIf, TrapIf, VirtAddr,
};
use tx_observe_types::{TxTraceHartRing, TxTraceKind, TxTraceRecord};

use crate::HartEmitter;

// ── Global test lock ───────────────────────────────────────────────────────────

/// Observation tests must not run concurrently because they share `HART_SLOTS`,
/// `EMITTERS`, `TS_FN`, and `CPU_ID_FN` statics.  All tests that call
/// `TestPlatform::init()` automatically serialize on this lock.
static TEST_LOCK: AtomicBool = AtomicBool::new(false);

struct TestLockGuard;

impl TestLockGuard {
    fn acquire() -> Self {
        while TEST_LOCK
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        Self
    }
}

impl Drop for TestLockGuard {
    fn drop(&mut self) {
        TEST_LOCK.store(false, Ordering::Release);
    }
}

// ── Ring storage ───────────────────────────────────────────────────────────────

/// Default ring size: power of two ≥ header + 16 slots.
/// Header = 208 bytes, 16 slots × 80 bytes = 1280 bytes → total = 1488 → round up to 2048.
const DEFAULT_RING_BYTES: usize = 2048;

// Maximum slot count we support in testing (ring must fit DEFAULT_RING_BYTES).
const MAX_SLOT_COUNT: usize = 16;

// ── TestPlatform builder ────────────────────────────────────────────────────────

/// Builder for a synthetic observation test session.
pub struct TestPlatform {
    cpu_id: usize,
    slot_count: usize,
}

impl TestPlatform {
    /// Construct with defaults: cpu id 0, 16 slots.
    pub fn new() -> Self {
        Self {
            cpu_id: 0,
            slot_count: MAX_SLOT_COUNT,
        }
    }

    /// Override the simulated cpu id (default 0).
    pub fn with_cpu_id(mut self, cpu: CpuId) -> Self {
        self.cpu_id = cpu.0;
        self
    }

    /// Override the ring slot count (default 16, must be a power of two ≤ 16).
    ///
    /// The internal backing store is fixed at [`DEFAULT_RING_BYTES`]; slot
    /// counts larger than `MAX_SLOT_COUNT` are clamped to the default.
    pub fn with_slot_count(mut self, n: usize) -> Self {
        assert!(
            n.is_power_of_two() && n <= MAX_SLOT_COUNT,
            "slot_count must be a power of two and ≤ {MAX_SLOT_COUNT}"
        );
        self.slot_count = n;
        self
    }

    /// Initialize observation against this platform.
    ///
    /// Acquires the global `TEST_LOCK`, resets any previous observation state
    /// for this hart, initialises `tx_observe` with a synthetic ring, and
    /// returns a [`TestObservation`].  The lock is held until `TestObservation`
    /// is dropped.
    pub fn init(self) -> TestObservation {
        let guard = TestLockGuard::acquire();

        // Allocate ring storage on the heap.
        let ring_bytes = DEFAULT_RING_BYTES;
        let mut storage: Vec<u8> = vec![0u8; ring_bytes];

        // Record what we need before moving `self`.
        let cpu_id = self.cpu_id;

        // Install platform state: cpu-id and timestamp counters.
        CURRENT_CPU_HELPER.store(cpu_id, Ordering::Release);
        TS_COUNTER_HELPER.store(0, Ordering::Relaxed);

        // SAFETY: The ring is backed by a heap allocation that lives at least
        // as long as the `TestObservation`; the pointer is non-null.
        let ptr = storage.as_mut_ptr();

        // Register the platform with tx_observe.
        // SAFETY: we hold the test lock; no concurrent observer access.
        unsafe { crate::testing_reset(cpu_id) };

        // Build a RingDescriptor pointing at our storage.
        // We temporarily store the pointer in a thread-local so the platform
        // impl can retrieve it.
        CURRENT_RING_PTR.store(ptr as usize, Ordering::Release);
        CURRENT_RING_LEN.store(ring_bytes, Ordering::Release);

        // Run tx_observe::init using our synthetic platform.
        crate::init::<SyntheticPlatform>(CpuId(cpu_id)).expect("TestPlatform: observe init failed");

        TestObservation {
            _guard: guard,
            _storage: storage,
            cpu_id,
        }
    }
}

impl Default for TestPlatform {
    fn default() -> Self {
        Self::new()
    }
}

// ── TestObservation RAII guard ──────────────────────────────────────────────────

/// A drop-guarded observation test session.
///
/// The `Drop` implementation resets the global observation statics so that
/// the next test starts with a clean slate.  The `TEST_LOCK` is held for the
/// lifetime of this value.
pub struct TestObservation {
    _guard: TestLockGuard,
    _storage: Vec<u8>,
    cpu_id: usize,
}

impl TestObservation {
    /// Get the emitter for the simulated hart.
    pub fn emitter(&self) -> &HartEmitter {
        crate::current().expect("TestObservation: no emitter registered")
    }

    /// Snapshot the ring as a `Vec<TxTraceRecord>` of all currently visible records.
    ///
    /// Reads from producer cursor 0 to the current producer position.
    pub fn records(&self) -> Vec<TxTraceRecord> {
        let ptr = CURRENT_RING_PTR.load(Ordering::Acquire) as *const u8;
        if ptr.is_null() {
            return Vec::new();
        }
        // SAFETY: We hold the test lock; no concurrent producer; the storage
        // lives for the lifetime of this TestObservation.
        unsafe {
            let hdr = &*(ptr as *const TxTraceHartRing);
            let producer = hdr.producer.load(Ordering::Acquire);
            let slots_ptr =
                ptr.add(core::mem::size_of::<TxTraceHartRing>()) as *const TxTraceRecord;
            let slot_count = (CURRENT_RING_LEN.load(Ordering::Acquire)
                - core::mem::size_of::<TxTraceHartRing>())
                / core::mem::size_of::<TxTraceRecord>();
            // Use prev_power_of_two to match the actual runtime slot count.
            let actual_slot_count = prev_power_of_two(slot_count);
            let mut out = Vec::new();
            for i in 0..producer.min(actual_slot_count as u64) {
                out.push(core::ptr::read(slots_ptr.add(i as usize)));
            }
            out
        }
    }

    /// The ring header, for cursor / lost-counter inspection.
    pub fn header(&self) -> TxTraceHartRing {
        let ptr = CURRENT_RING_PTR.load(Ordering::Acquire) as *const u8;
        assert!(!ptr.is_null(), "TestObservation: ring not initialised");
        // SAFETY: see records().
        unsafe { core::ptr::read(ptr as *const TxTraceHartRing) }
    }

    /// Convenience: records of a specific `TxTraceKind`.
    pub fn records_of_kind(&self, kind: TxTraceKind) -> Vec<TxTraceRecord> {
        self.records()
            .into_iter()
            .filter(|r| r.kind == kind as u8)
            .collect()
    }
}

impl Drop for TestObservation {
    fn drop(&mut self) {
        // Reset global observation state for this hart so the next test is clean.
        // SAFETY: we hold the test lock; no concurrent access.
        unsafe { crate::testing_reset(self.cpu_id) };
        CURRENT_RING_PTR.store(0, Ordering::Relaxed);
        CURRENT_RING_LEN.store(0, Ordering::Relaxed);
        CURRENT_CPU_HELPER.store(0, Ordering::Relaxed);
        TS_COUNTER_HELPER.store(0, Ordering::Relaxed);
    }
}

// ── Synthetic platform thread-locals / atomics ─────────────────────────────────

/// Current cpu-id for the synthetic platform.
static CURRENT_CPU_HELPER: AtomicUsize = AtomicUsize::new(0);
/// Monotone timestamp counter for the synthetic platform.
static TS_COUNTER_HELPER: AtomicUsize = AtomicUsize::new(0);
/// Pointer to the current ring backing storage (as usize).
static CURRENT_RING_PTR: AtomicUsize = AtomicUsize::new(0);
/// Length of the current ring backing storage.
static CURRENT_RING_LEN: AtomicUsize = AtomicUsize::new(0);

// ── Static platform data ───────────────────────────────────────────────────────

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
    board: "tx-observe-test-helpers",
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 1_000_000_000,
    possible_cpu_count: 1,
};

// ── SyntheticPlatform — minimal TxPlatform impl ─────────────────────────────────

struct SyntheticPlatform;

impl PlatformConfig for SyntheticPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-observe-test-helpers";
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(0xffff_ffc0_0000_0000);
}

impl BootPlatformIf for SyntheticPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for SyntheticPlatform {
    fn init_early(_: tx_hal::BootHandoff) {}
    fn init_later(_: tx_hal::BootHandoff) {}
}

impl BootInfoIf for SyntheticPlatform {
    fn boot_info() -> &'static BootInfo {
        &BOOT_INFO
    }
}

impl PlatformInfoIf for SyntheticPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &PLATFORM_INFO
    }
}

impl AuxvIf for SyntheticPlatform {}

impl ConsoleIf for SyntheticPlatform {
    fn write_bytes(_: &[u8]) {}
}

impl PmapIf for SyntheticPlatform {}
impl TrapIf for SyntheticPlatform {}
impl SignalFrameIf for SyntheticPlatform {}
unsafe fn restore_synthetic_local_execution(_saved_state: usize) {}

impl IrqIf for SyntheticPlatform {
    fn exclude_local_execution() -> tx_hal::LocalExecutionGuard {
        unsafe { tx_hal::LocalExecutionGuard::new(0, restore_synthetic_local_execution) }
    }
}
impl EntropyIf for SyntheticPlatform {}
impl CacheIf for SyntheticPlatform {}
impl DmaIf for SyntheticPlatform {}

impl SmpIf for SyntheticPlatform {
    fn possible_cpus() -> CpuMask {
        CpuMask::first(1)
    }
    fn online_cpus() -> CpuMask {
        CpuMask::first(1)
    }
}

impl PowerIf for SyntheticPlatform {
    fn system_off() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}

impl MonotonicCounterIf for SyntheticPlatform {
    fn read_ns() -> u64 {
        TS_COUNTER_HELPER.fetch_add(1, Ordering::Relaxed) as u64 + 1
    }

    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl PercpuIf for SyntheticPlatform {
    fn current_cpu_id() -> CpuId {
        CpuId(CURRENT_CPU_HELPER.load(Ordering::Acquire))
    }
}

impl ObserverIf for SyntheticPlatform {
    fn observation_ring(hart: CpuId) -> Option<RingDescriptor> {
        let expected_cpu = CURRENT_CPU_HELPER.load(Ordering::Acquire);
        if hart.0 != expected_cpu {
            return None;
        }
        let raw_ptr = CURRENT_RING_PTR.load(Ordering::Acquire);
        if raw_ptr == 0 {
            return None;
        }
        let len = CURRENT_RING_LEN.load(Ordering::Acquire);
        // SAFETY: caller guarantees the backing Vec is alive for the duration
        // of the TestObservation (it is stored in TestObservation::_storage).
        let base = unsafe { NonNull::new_unchecked(raw_ptr as *mut u8) };
        Some(RingDescriptor {
            base,
            size: len,
            doorbell: None,
        })
    }
}

// ── Utility ────────────────────────────────────────────────────────────────────

/// Largest power of two ≤ `n`.
fn prev_power_of_two(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    1usize << (usize::BITS - n.leading_zeros() - 1)
}
