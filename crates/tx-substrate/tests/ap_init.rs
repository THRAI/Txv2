use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tx_hal::{
    Arch, AuxvIf, BootInfo, BootInfoIf, BootPlatformIf, BootProtocol, CacheIf, ConsoleIf, CpuId,
    CpuMask, CpuPinGuard, CpuPinReason, DeadlineTimerIf, DmaIf, EntropyIf, InitIf, IrqIf,
    MemoryRegion, MonotonicCounterIf, ObserverIf, PercpuIf, PersistentClockIf, PhysRange,
    PlatformConfig, PlatformInfo, PlatformInfoIf, PmapIf, PowerIf, SignalFrameIf, SmpIf, TrapIf,
    VirtAddr,
};
use tx_substrate::zone::{Zone, ZoneAllocated};
use tx_substrate::{epoch, zone};

static AP_INIT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static POSSIBLE_CPUS: AtomicU64 = AtomicU64::new(0b11);
static ONLINE_CPUS: AtomicU64 = AtomicU64::new(0b1);
static RECLAIM_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOCAL_EXECUTION_DEPTHS: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
static CPU_PIN_DEPTHS: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];
static LOCAL_EXCLUSION_CALLS: AtomicUsize = AtomicUsize::new(0);
static PINS_WHILE_EXCLUDED: AtomicUsize = AtomicUsize::new(0);
static UNPINS_WHILE_EXCLUDED: AtomicUsize = AtomicUsize::new(0);
const CPU_PIN_EVENT_CAPACITY: usize = 128;
static CPU_PIN_EVENT_COUNT: AtomicUsize = AtomicUsize::new(0);
static CPU_PIN_EVENTS: [AtomicUsize; CPU_PIN_EVENT_CAPACITY] =
    [const { AtomicUsize::new(0) }; CPU_PIN_EVENT_CAPACITY];

static_assertions::assert_not_impl_any!(CpuPinGuard: Send, Sync);
static_assertions::assert_not_impl_any!(epoch::Guard<'static>: Send, Sync);

fn encode_cpu_pin_event(pin: bool, reason: CpuPinReason, before: usize, after: usize) -> usize {
    usize::from(pin) | ((reason as usize) << 1) | ((before & 0xff) << 8) | ((after & 0xff) << 16)
}

fn record_cpu_pin_event(pin: bool, reason: CpuPinReason, before: usize, after: usize) {
    let index = CPU_PIN_EVENT_COUNT.fetch_add(1, Ordering::AcqRel);
    if index < CPU_PIN_EVENT_CAPACITY {
        CPU_PIN_EVENTS[index].store(
            encode_cpu_pin_event(pin, reason, before, after),
            Ordering::Release,
        );
    }
}

fn cpu_pin_events_contain_reason(reason: CpuPinReason) -> bool {
    let count = CPU_PIN_EVENT_COUNT
        .load(Ordering::Acquire)
        .min(CPU_PIN_EVENT_CAPACITY);
    CPU_PIN_EVENTS[..count]
        .iter()
        .any(|event| (event.load(Ordering::Acquire) >> 1) & 0x7f == reason as usize)
}

#[derive(Debug, Eq, PartialEq)]
struct ApZoneObject(u64);

static AP_ZONE: Zone<ApZoneObject> = Zone::const_new();

unsafe impl ZoneAllocated for ApZoneObject {
    fn zone() -> &'static Zone<Self> {
        &AP_ZONE
    }
}

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
    board: "ap-init-test",
    spi_sd: None,
    mmio_regions: &[],
    device_resources: &tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH,
    timebase_frequency_hz: 1,
    possible_cpu_count: 2,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "ap-init-test";
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
unsafe fn restore_test_local_execution(saved_state: usize) {
    let cpu = saved_state >> 16;
    let previous_depth = saved_state & 0xffff;
    assert_eq!(CURRENT_CPU.load(Ordering::Acquire), cpu);
    let observed = LOCAL_EXECUTION_DEPTHS[cpu].fetch_sub(1, Ordering::AcqRel);
    assert_eq!(observed, previous_depth + 1);
    if previous_depth == 0 {
        assert_eq!(CPU_PIN_DEPTHS[cpu].load(Ordering::Acquire), 0);
    }
}
impl IrqIf for TestPlatform {
    fn exclude_local_execution() -> tx_hal::LocalExecutionGuard {
        let cpu = CURRENT_CPU.load(Ordering::Acquire);
        let previous_depth = LOCAL_EXECUTION_DEPTHS[cpu].fetch_add(1, Ordering::AcqRel);
        assert!(previous_depth < 0xffff);
        LOCAL_EXCLUSION_CALLS.fetch_add(1, Ordering::AcqRel);
        unsafe {
            tx_hal::LocalExecutionGuard::new(
                (cpu << 16) | previous_depth,
                restore_test_local_execution,
            )
        }
    }
}
impl EntropyIf for TestPlatform {}

impl MonotonicCounterIf for TestPlatform {
    fn read_ns() -> u64 {
        0
    }

    fn frequency_hz() -> u64 {
        PLATFORM_INFO.timebase_frequency_hz
    }
}

impl DeadlineTimerIf for TestPlatform {
    fn set_deadline_ns(_deadline: u64) {}

    fn cancel_deadline() {}
}

impl PersistentClockIf for TestPlatform {}

impl PercpuIf for TestPlatform {
    fn current_cpu_id() -> CpuId {
        CpuId(CURRENT_CPU.load(Ordering::Acquire))
    }

    fn pin_current_cpu() -> CpuPinGuard {
        Self::pin_current_cpu_for(CpuPinReason::Unclassified)
    }

    fn pin_current_cpu_for(reason: CpuPinReason) -> CpuPinGuard {
        let cpu = CURRENT_CPU.load(Ordering::Acquire);
        if LOCAL_EXECUTION_DEPTHS[cpu].load(Ordering::Acquire) > 0 {
            PINS_WHILE_EXCLUDED.fetch_add(1, Ordering::AcqRel);
        }
        let before = CPU_PIN_DEPTHS[cpu].fetch_add(1, Ordering::AcqRel);
        record_cpu_pin_event(true, reason, before, before + 1);
        CpuPinGuard::with_reasoned_unpin(CpuId(cpu), reason, unpin_test_cpu)
    }
}

fn unpin_test_cpu(cpu: CpuId, reason: CpuPinReason) {
    if LOCAL_EXECUTION_DEPTHS[cpu.0].load(Ordering::Acquire) > 0 {
        UNPINS_WHILE_EXCLUDED.fetch_add(1, Ordering::AcqRel);
    }
    let previous = CPU_PIN_DEPTHS[cpu.0].fetch_sub(1, Ordering::AcqRel);
    assert!(previous > 0);
    record_cpu_pin_event(false, reason, previous, previous - 1);
}

impl ObserverIf for TestPlatform {}
impl CacheIf for TestPlatform {}
impl DmaIf for TestPlatform {}

impl SmpIf for TestPlatform {
    fn possible_cpus() -> CpuMask {
        CpuMask::from_bits(POSSIBLE_CPUS.load(Ordering::Acquire))
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(ONLINE_CPUS.load(Ordering::Acquire))
    }
}

impl PowerIf for TestPlatform {
    fn system_off() -> ! {
        loop {
            core::hint::spin_loop();
        }
    }
}

fn reset_runtime(possible_cpus: CpuMask, current_cpu: CpuId, online_cpus: CpuMask) {
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
        zone::testing::reset_for_test();
    }
    POSSIBLE_CPUS.store(possible_cpus.bits(), Ordering::Release);
    CURRENT_CPU.store(current_cpu.0, Ordering::Release);
    ONLINE_CPUS.store(online_cpus.bits(), Ordering::Release);
    RECLAIM_COUNT.store(0, Ordering::Release);
    LOCAL_EXCLUSION_CALLS.store(0, Ordering::Release);
    PINS_WHILE_EXCLUDED.store(0, Ordering::Release);
    UNPINS_WHILE_EXCLUDED.store(0, Ordering::Release);
    CPU_PIN_EVENT_COUNT.store(0, Ordering::Release);
    for event in &CPU_PIN_EVENTS {
        event.store(0, Ordering::Release);
    }
    for depth in &LOCAL_EXECUTION_DEPTHS {
        depth.store(0, Ordering::Release);
    }
    for depth in &CPU_PIN_DEPTHS {
        depth.store(0, Ordering::Release);
    }
    epoch::init_on_bsp::<TestPlatform>().expect("epoch bsp init");
    zone::init_on_bsp::<TestPlatform>().expect("zone bsp init");
    zone::testing::set_direct_map_base_for_test(
        tx_substrate::page_allocator::testing::direct_map_base_for_test(),
    )
    .expect("host zone direct map");
}

unsafe fn count_reclaim(_ptr: *mut u8) {
    RECLAIM_COUNT.fetch_add(1, Ordering::AcqRel);
}

fn retired_ptr() -> *mut u8 {
    NonNull::<u8>::dangling().as_ptr()
}

#[test]
fn substrate_ap_init_makes_epoch_guard_valid_on_secondary_cpu() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::from_bits(0b11),
        CpuId(0),
        CpuMask::single(CpuId(0)),
    );

    CURRENT_CPU.store(1, Ordering::Release);
    ONLINE_CPUS.store(0b11, Ordering::Release);
    tx_substrate::init_on_ap(CpuId(1)).expect("ap substrate init");

    let guard = epoch::guard();

    assert_eq!(guard.cpu_id(), CpuId(1));
}

#[test]
fn nested_epoch_cpu_pins_balance_in_non_lifo_drop_order() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::from_bits(0b11),
        CpuId(0),
        CpuMask::single(CpuId(0)),
    );

    let outer = epoch::guard();
    assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 1);
    let inner = epoch::guard();
    assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 2);

    drop(outer);
    assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 1);
    drop(inner);
    assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 0);

    let expected = [
        encode_cpu_pin_event(true, CpuPinReason::EpochGuard, 0, 1),
        encode_cpu_pin_event(true, CpuPinReason::EpochGuard, 1, 2),
        encode_cpu_pin_event(false, CpuPinReason::EpochGuard, 2, 1),
        encode_cpu_pin_event(false, CpuPinReason::EpochGuard, 1, 0),
    ];
    assert_eq!(CPU_PIN_EVENT_COUNT.load(Ordering::Acquire), expected.len());
    for (index, expected) in expected.into_iter().enumerate() {
        assert_eq!(CPU_PIN_EVENTS[index].load(Ordering::Acquire), expected);
    }
}

#[test]
fn epoch_cpu_pin_is_zero_at_every_poll_boundary() {
    use std::future::{poll_fn, Future};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct NoopWake;
    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::from_bits(0b11),
        CpuId(0),
        CpuMask::single(CpuId(0)),
    );

    let mut polls = 0usize;
    let mut future = std::pin::pin!(poll_fn(|cx| {
        let _pin = epoch::guard();
        assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 1);
        polls += 1;
        if polls == 1 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }));
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);

    assert!(Future::poll(future.as_mut(), &mut context).is_pending());
    assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 0);
    assert!(Future::poll(future.as_mut(), &mut context).is_ready());
    assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 0);
}

#[test]
fn zone_bucket_and_keg_exclude_local_execution_on_each_cpu() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::from_bits(0b11),
        CpuId(0),
        CpuMask::single(CpuId(0)),
    );
    zone::register_zone_for::<ApZoneObject>().expect("register AP zone");

    let bsp_cap = zone::sign(ApZoneObject(10)).expect("BSP zone sign");
    let bsp_clone = bsp_cap.clone();
    assert_eq!(bsp_clone.0, 10);
    drop(bsp_clone);

    CURRENT_CPU.store(1, Ordering::Release);
    ONLINE_CPUS.store(0b11, Ordering::Release);
    tx_substrate::init_on_ap(CpuId(1)).expect("AP substrate init");

    let ap_cap = zone::sign(ApZoneObject(20)).expect("AP zone sign");
    let ap_clone = ap_cap.clone();
    assert_eq!(ap_clone.0, 20);
    drop(ap_clone);

    assert!(LOCAL_EXCLUSION_CALLS.load(Ordering::Acquire) > 0);
    assert!(PINS_WHILE_EXCLUDED.load(Ordering::Acquire) > 0);
    assert!(UNPINS_WHILE_EXCLUDED.load(Ordering::Acquire) > 0);
    assert!(cpu_pin_events_contain_reason(CpuPinReason::ZoneBucketPop));
    assert!(cpu_pin_events_contain_reason(CpuPinReason::ZoneBucketMerge));
    assert_eq!(LOCAL_EXECUTION_DEPTHS[0].load(Ordering::Acquire), 0);
    assert_eq!(LOCAL_EXECUTION_DEPTHS[1].load(Ordering::Acquire), 0);
    assert_eq!(CPU_PIN_DEPTHS[0].load(Ordering::Acquire), 0);
    assert_eq!(CPU_PIN_DEPTHS[1].load(Ordering::Acquire), 0);
}

#[test]
fn substrate_ap_init_rejects_cpu_outside_possible_mask() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::from_bits(0b11),
        CpuId(0),
        CpuMask::single(CpuId(0)),
    );

    let err = tx_substrate::init_on_ap(CpuId(2)).expect_err("cpu 2 is not possible");

    assert_eq!(
        err,
        tx_substrate::ApInitError::Epoch(epoch::EpochError::InvalidCpu)
    );
}

#[test]
fn sparse_nonzero_bsp_cpu_initializes_epoch_and_zone() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    reset_runtime(
        CpuMask::single(CpuId(1)),
        CpuId(1),
        CpuMask::single(CpuId(1)),
    );

    let guard = epoch::guard();
    assert_eq!(guard.cpu_id(), CpuId(1));
    assert_eq!(epoch::summary().possible_cpus, 1);
    assert!(epoch::cpu_summary(CpuId(0)).is_none());
    assert!(epoch::cpu_summary(CpuId(1)).is_some());
}

#[test]
fn epoch_rejects_missing_bsp_without_poisoning_initialization() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    tx_substrate::testing::init_host_for_test_once();
    unsafe {
        epoch::testing::reset_for_test();
    }
    CURRENT_CPU.store(1, Ordering::Release);
    ONLINE_CPUS.store(CpuMask::single(CpuId(1)).bits(), Ordering::Release);
    POSSIBLE_CPUS.store(CpuMask::single(CpuId(4)).bits(), Ordering::Release);

    assert_eq!(
        epoch::init_on_bsp::<TestPlatform>(),
        Err(epoch::EpochError::InvalidCpu)
    );

    POSSIBLE_CPUS.store(CpuMask::single(CpuId(1)).bits(), Ordering::Release);
    epoch::init_on_bsp::<TestPlatform>().expect("retry after rejected BSP mask");
    assert_eq!(epoch::guard().cpu_id(), CpuId(1));
}

#[test]
fn sparse_ap_guard_blocks_reclaim_until_that_cpu_quiesces() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    let sparse_mask = CpuMask::from_bits((1 << 1) | (1 << 4));
    reset_runtime(sparse_mask, CpuId(1), sparse_mask);
    tx_substrate::init_on_ap(CpuId(4)).expect("sparse AP substrate init");

    CURRENT_CPU.store(4, Ordering::Release);
    let ap_guard = epoch::guard();
    assert_eq!(ap_guard.cpu_id(), CpuId(4));

    CURRENT_CPU.store(1, Ordering::Release);
    unsafe {
        epoch::testing::retire_raw_for_test(retired_ptr(), count_reclaim).expect("retire");
    }
    assert_eq!(epoch::try_drain(usize::MAX).reclaimed, 0);
    assert_eq!(epoch::try_drain(usize::MAX).reclaimed, 0);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 0);

    drop(ap_guard);
    let reclaimed: usize = (0..3).map(|_| epoch::try_drain(usize::MAX).reclaimed).sum();
    assert_eq!(reclaimed, 1);
    assert_eq!(RECLAIM_COUNT.load(Ordering::Acquire), 1);
}

#[test]
fn sparse_cpu_holes_and_out_of_range_ids_are_rejected() {
    let _guard = AP_INIT_TEST_LOCK.lock().expect("ap init test lock");
    let sparse_mask = CpuMask::from_bits((1 << 1) | (1 << 4));
    reset_runtime(sparse_mask, CpuId(1), sparse_mask);

    assert_eq!(
        epoch::init_on_ap(CpuId(2)),
        Err(epoch::EpochError::InvalidCpu)
    );
    assert_eq!(
        epoch::init_on_ap(CpuId(64)),
        Err(epoch::EpochError::InvalidCpu)
    );
    assert_eq!(
        zone::init_on_ap(CpuId(2)),
        Err(zone::ZoneError::InvalidState)
    );
    assert_eq!(
        zone::init_on_ap(CpuId(64)),
        Err(zone::ZoneError::InvalidState)
    );
}
