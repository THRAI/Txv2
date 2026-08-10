//! Pre-ELF Phase 5 (item 9) IRQ-dispatch tests.
//!
//! Exercises the boot-time `install_irq_handlers` step end-to-end:
//! the dispatch table is populated, published to the platform via the
//! captured `install_dispatch_table` call, and `dispatch_irq` routes
//! a fake UART RX byte through `tty::execution::step_ingest` into the
//! registered console TTY's input queue.
//!
//! Per Open Q #4 the registration mechanism is explicit (no linkme),
//! which means tests can install a controlled subset of handlers,
//! reset the table between runs, and capture
//! `install_dispatch_table` arguments through the test platform.
//!
//! All three tests share the kernel-wide `KERNEL_TEST_LOCK` so they
//! serialise against the boot-wiring tests in `init::tests` (both
//! sets touch `INIT_PROCESS`, the TTY registry, and the global IRQ
//! dispatch table).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Mutex;

use tx_hal::{
    AllocError, Arch, Asid, BootHandoff, BootInfo, BootPlatformIf, BootProtocol, ConsoleIf, InitIf,
    IrqDispatchTable, IrqHandled, IrqIf, ObserverIf, PhysAddr, PlatformConfig, PlatformInfo,
    PmapError, PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot, PtNode,
};

use crate::init::{console_tty, CoreInit};
use crate::irq::{
    drain_net_rx_irq, handler_for, install_irq_handlers, net_irq_stats,
    poll_console_rx_into_pending, publish_deferred_net_claim, register_irq_handler,
    rtc_alarm_irq_handler, try_acquire_console_rx_ingest, try_read_console_bytes,
    uart_rx_irq_handler, UART_RX_PENDING,
};
use crate::test_serialise::KERNEL_TEST_LOCK as IRQ_TEST_LOCK;
use tx_subsystems::device_binding::BoundDeviceKey;
use tx_subsystems::net::device::{reset_net_registry_for_test, NetDeviceIrqOutcome};
use tx_subsystems::signal::Signum;

const TEST_PAGE_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Fake platform with a queueable RX source + capture for
// `install_dispatch_table`.
// ---------------------------------------------------------------------------

pub(crate) struct IrqTestPlatform;

/// Bytes to feed to the next `read_bytes` call.
static IRQ_TEST_RX_QUEUE: Mutex<std::vec::Vec<u8>> = Mutex::new(std::vec::Vec::new());

/// Records the static reference handed to `install_dispatch_table`.
/// Captured as the raw pointer so we can compare against
/// `crate::irq::IRQ_DISPATCH_TABLE`'s address from the test side.
static IRQ_TEST_INSTALLED_TABLE_PTR: AtomicUsize = AtomicUsize::new(0);

/// Records the most recent `set_priority` call.
static IRQ_TEST_LAST_PRIORITY: AtomicU32 = AtomicU32::new(0);

/// Records that `unmask` was called for the UART IRQ.
static IRQ_TEST_UART_UNMASKED: AtomicBool = AtomicBool::new(false);

/// Records that `unmask` was called for the RTC IRQ.
static IRQ_TEST_RTC_UNMASKED: AtomicBool = AtomicBool::new(false);

static IRQ_TEST_RUNTIME_UART_IRQ: AtomicU32 = AtomicU32::new(17);
static IRQ_TEST_RUNTIME_RTC_IRQ: AtomicU32 = AtomicU32::new(18);
const IRQ_TEST_DEVICE_IRQ: u32 = 19;

static IRQ_TEST_CURRENT_CPU: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_COMPLETED_IRQ: AtomicU32 = AtomicU32::new(0);
static IRQ_TEST_COMPLETE_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_NET_ACK_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_NET_MASK_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_NET_UNMASK_COUNT: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_ACK_ORDER: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_COMPLETE_ORDER: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_UNMASK_ORDER: AtomicUsize = AtomicUsize::new(0);
static IRQ_TEST_LOCAL_EXCLUSION_COUNT: AtomicUsize = AtomicUsize::new(0);

static REENTRANT_CONSOLE_READS: AtomicUsize = AtomicUsize::new(0);

struct ReentrantConsole;

impl ConsoleIf for ReentrantConsole {
    fn write_bytes(_bytes: &[u8]) {}

    fn read_bytes(buf: &mut [u8]) -> usize {
        REENTRANT_CONSOLE_READS.fetch_add(1, Ordering::AcqRel);
        let mut nested = [0u8; 1];
        assert_eq!(
            try_read_console_bytes::<Self>(&mut nested),
            0,
            "an IRQ-style nested reader must not re-enter the console source",
        );
        buf[0] = b'R';
        1
    }
}

static BLOCKING_CONSOLE_ENTERED: AtomicBool = AtomicBool::new(false);
static BLOCKING_CONSOLE_RELEASE: AtomicBool = AtomicBool::new(false);
static BLOCKING_CONSOLE_READS: AtomicUsize = AtomicUsize::new(0);

struct BlockingConsole;

impl ConsoleIf for BlockingConsole {
    fn write_bytes(_bytes: &[u8]) {}

    fn read_bytes(buf: &mut [u8]) -> usize {
        BLOCKING_CONSOLE_READS.fetch_add(1, Ordering::AcqRel);
        BLOCKING_CONSOLE_ENTERED.store(true, Ordering::Release);
        while !BLOCKING_CONSOLE_RELEASE.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        buf[0] = b'S';
        1
    }
}

struct IrqTestNetDevice;

impl tx_subsystems::net::NetDeviceOps for IrqTestNetDevice {
    fn receive(&self) -> Option<tx_subsystems::net::RxFrame> {
        None
    }

    fn transmit(
        &self,
        _frame: &[u8],
        _guard: &tx_subsystems::execution::Guard<'_>,
    ) -> tx_subsystems::execution::StepOutcome<()> {
        tx_subsystems::execution::StepOutcome::Done(())
    }

    fn mac_addr(&self) -> tx_subsystems::net::EthernetAddress {
        tx_subsystems::net::EthernetAddress::new([0x02, 0, 0, 0, 0, 0x71])
    }

    fn mtu(&self) -> u16 {
        1500
    }

    fn ack_interrupt_and_fire(&self) -> NetDeviceIrqOutcome {
        IRQ_TEST_NET_ACK_COUNT.fetch_add(1, Ordering::AcqRel);
        let order = IRQ_TEST_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
        IRQ_TEST_ACK_ORDER.store(order, Ordering::Release);
        NetDeviceIrqOutcome::default()
    }
}

static IRQ_TEST_NET_DEVICE: IrqTestNetDevice = IrqTestNetDevice;
static IRQ_TEST_NET_REGISTRATION: tx_subsystems::net::NetDeviceRegistration =
    tx_subsystems::net::NetDeviceRegistration {
        devt: tx_subsystems::device::DevT::new(97, 0),
        name: "eth0",
        ops: &IRQ_TEST_NET_DEVICE,
    };
fn drain_rx_queue(buf: &mut [u8]) -> usize {
    let mut queue = IRQ_TEST_RX_QUEUE.lock().expect("rx queue lock");
    let n = queue.len().min(buf.len());
    for (slot, byte) in buf.iter_mut().take(n).zip(queue.drain(..n)) {
        *slot = byte;
    }
    n
}

impl PlatformConfig for IrqTestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "tx-kernel-irq-test";
}

impl BootPlatformIf for IrqTestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for IrqTestPlatform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

static EMPTY_BOOT_INFO: BootInfo = BootInfo::empty();
static IRQ_TEST_PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: IrqTestPlatform::BOARD,
    spi_sd: None,
    mmio_regions: &[],
    device_resources: &tx_hal::EMPTY_DEVICE_RESOURCE_GRAPH,
    timebase_frequency_hz: 0,
    possible_cpu_count: 1,
};

impl tx_hal::BootInfoIf for IrqTestPlatform {
    fn boot_info() -> &'static BootInfo {
        &EMPTY_BOOT_INFO
    }
}

impl tx_hal::PlatformInfoIf for IrqTestPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &IRQ_TEST_PLATFORM_INFO
    }
}

impl tx_hal::AuxvIf for IrqTestPlatform {}

impl ConsoleIf for IrqTestPlatform {
    fn write_bytes(_bytes: &[u8]) {}

    fn read_bytes(buf: &mut [u8]) -> usize {
        drain_rx_queue(buf)
    }
}

impl tx_hal::TrapIf for IrqTestPlatform {}
impl tx_hal::SignalFrameIf for IrqTestPlatform {}

unsafe fn restore_test_local_execution(_saved_state: usize) {}

impl IrqIf for IrqTestPlatform {
    const MAX_IRQ: u32 = 64;

    /// Pick a non-zero IRQ so the test isn't accidentally aliased to
    /// the IRQ-0 sentinel that `KernelTrapDispatcher::on_external_irq`
    /// short-circuits on.
    const UART_IRQ: u32 = 7;
    const RTC_IRQ: u32 = 8;

    fn uart_irq() -> u32 {
        IRQ_TEST_RUNTIME_UART_IRQ.load(Ordering::Acquire)
    }

    fn rtc_irq() -> u32 {
        IRQ_TEST_RUNTIME_RTC_IRQ.load(Ordering::Acquire)
    }

    fn exclude_local_execution() -> tx_hal::LocalExecutionGuard {
        IRQ_TEST_LOCAL_EXCLUSION_COUNT.fetch_add(1, Ordering::AcqRel);
        unsafe { tx_hal::LocalExecutionGuard::new(0, restore_test_local_execution) }
    }

    fn install_dispatch_table(table: &'static IrqDispatchTable) {
        IRQ_TEST_INSTALLED_TABLE_PTR
            .store(table as *const IrqDispatchTable as usize, Ordering::Release);
    }

    fn set_priority(_irq: u32, priority: u8) {
        IRQ_TEST_LAST_PRIORITY.store(priority as u32, Ordering::Release);
    }

    fn complete(irq: u32) {
        IRQ_TEST_COMPLETED_IRQ.store(irq, Ordering::Release);
        IRQ_TEST_COMPLETE_COUNT.fetch_add(1, Ordering::AcqRel);
        let order = IRQ_TEST_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
        IRQ_TEST_COMPLETE_ORDER.store(order, Ordering::Release);
    }

    fn mask(irq: u32) {
        if irq == IRQ_TEST_DEVICE_IRQ {
            IRQ_TEST_NET_MASK_COUNT.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn unmask(irq: u32) {
        if irq == Self::uart_irq() {
            IRQ_TEST_UART_UNMASKED.store(true, Ordering::Release);
        }
        if irq == Self::rtc_irq() {
            IRQ_TEST_RTC_UNMASKED.store(true, Ordering::Release);
        }
        if irq == IRQ_TEST_DEVICE_IRQ {
            IRQ_TEST_NET_UNMASK_COUNT.fetch_add(1, Ordering::AcqRel);
            let order = IRQ_TEST_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
            IRQ_TEST_UNMASK_ORDER.store(order, Ordering::Release);
        }
    }
}

impl tx_hal::MonotonicCounterIf for IrqTestPlatform {
    fn read_ns() -> u64 {
        0
    }

    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl tx_hal::DeadlineTimerIf for IrqTestPlatform {
    fn set_deadline_ns(_deadline: u64) {}

    fn cancel_deadline() {}
}

impl tx_hal::PersistentClockIf for IrqTestPlatform {}

impl tx_hal::PercpuIf for IrqTestPlatform {
    fn current_cpu_id() -> tx_hal::CpuId {
        tx_hal::CpuId(IRQ_TEST_CURRENT_CPU.load(Ordering::Acquire))
    }
}
impl tx_hal::CacheIf for IrqTestPlatform {}
impl tx_hal::DmaIf for IrqTestPlatform {}
impl tx_hal::SmpIf for IrqTestPlatform {
    fn current_cpu_id() -> tx_hal::CpuId {
        tx_hal::CpuId(IRQ_TEST_CURRENT_CPU.load(Ordering::Acquire))
    }
}

impl tx_hal::EntropyIf for IrqTestPlatform {}
impl ObserverIf for IrqTestPlatform {}

impl tx_hal::PowerIf for IrqTestPlatform {
    fn system_off() -> ! {
        #[allow(clippy::empty_loop)]
        loop {}
    }
}

static IRQ_TEST_PMAP_NEXT_ROOT: AtomicUsize = AtomicUsize::new(1);

impl tx_hal::PmapIf for IrqTestPlatform {
    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        let root_id = IRQ_TEST_PMAP_NEXT_ROOT.fetch_add(1, Ordering::AcqRel);
        Ok(PmapRoot::new(
            PtNode::boot_pool(PhysAddr(root_id * TEST_PAGE_SIZE)),
            Asid(root_id as u16),
        ))
    }

    fn destroy_pmap_root(_root: PmapRoot) {}

    fn reserve_mapping(
        _root: &PmapRoot,
        virt: tx_hal::VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        Ok(Some(PmapReservation::new(virt, phys, kind)))
    }

    fn rollback_mapping(_root: &PmapRoot, _reservation: PmapReservation) {}

    fn commit_mapping(
        _root: &PmapRoot,
        _reservation: PmapReservation,
        _permissions: PmapPermissions,
    ) {
    }

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        Err(AllocError::Exhausted)
    }
}

// ---------------------------------------------------------------------------
// Setup / fixtures
// ---------------------------------------------------------------------------

fn setup() -> std::sync::MutexGuard<'static, ()> {
    let guard = IRQ_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    tx_test_support::init_host();
    let _ = tx_subsystems::zones::register_all();
    tx_subsystems::cross_crate_test_support::reset_init_process();
    tx_subsystems::cross_crate_test_support::reset_pid_counter();
    tx_subsystems::cross_crate_test_support::reset_tid_counter();
    tx_subsystems::cross_crate_test_support::reset_mount_table();
    tx_subsystems::cross_crate_test_support::reset_mount_id_counter();
    tx_subsystems::cross_crate_test_support::reset_dev_id_counter();
    crate::init::reset_boot_state_for_test();
    crate::irq::reset_dispatch_table_for_test();
    crate::irq::reset_pending_uart_rx_for_test();
    crate::irq::reset_pending_net_irq_for_test();
    reset_net_registry_for_test();
    IRQ_TEST_RX_QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    IRQ_TEST_INSTALLED_TABLE_PTR.store(0, Ordering::Release);
    IRQ_TEST_LAST_PRIORITY.store(0, Ordering::Release);
    IRQ_TEST_UART_UNMASKED.store(false, Ordering::Release);
    IRQ_TEST_RTC_UNMASKED.store(false, Ordering::Release);
    IRQ_TEST_RUNTIME_UART_IRQ.store(17, Ordering::Release);
    IRQ_TEST_RUNTIME_RTC_IRQ.store(18, Ordering::Release);
    IRQ_TEST_CURRENT_CPU.store(0, Ordering::Release);
    IRQ_TEST_COMPLETED_IRQ.store(0, Ordering::Release);
    IRQ_TEST_COMPLETE_COUNT.store(0, Ordering::Release);
    IRQ_TEST_NET_ACK_COUNT.store(0, Ordering::Release);
    IRQ_TEST_NET_MASK_COUNT.store(0, Ordering::Release);
    IRQ_TEST_NET_UNMASK_COUNT.store(0, Ordering::Release);
    IRQ_TEST_SEQUENCE.store(0, Ordering::Release);
    IRQ_TEST_ACK_ORDER.store(0, Ordering::Release);
    IRQ_TEST_COMPLETE_ORDER.store(0, Ordering::Release);
    IRQ_TEST_UNMASK_ORDER.store(0, Ordering::Release);
    IRQ_TEST_LOCAL_EXCLUSION_COUNT.store(0, Ordering::Release);
    REENTRANT_CONSOLE_READS.store(0, Ordering::Release);
    BLOCKING_CONSOLE_ENTERED.store(false, Ordering::Release);
    BLOCKING_CONSOLE_RELEASE.store(false, Ordering::Release);
    BLOCKING_CONSOLE_READS.store(0, Ordering::Release);
    tx_fs::devfs::reset_rtc_backend_for_test();
    guard
}

fn bootstrap_init_for_irq_test() {
    let aspace = tx_subsystems::vm::AddressSpace::new_cap_for_platform::<IrqTestPlatform>()
        .expect("test aspace");
    let _init = tx_subsystems::process::bootstrap_init_process(aspace).expect("bootstrap init");
}

fn init_has_pending_sigint() -> bool {
    let init = tx_subsystems::process::execution::init_process().expect("init process");
    let leader = init.nth_thread(0).expect("init leader thread");
    leader
        .payload_cap()
        .expect("leader payload")
        .pending()
        .is_pending(Signum::SIGINT)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn console_rx_reader_rejects_nested_and_cross_hart_consumers() {
    let _setup = setup();

    let mut byte = [0u8; 1];
    assert_eq!(try_read_console_bytes::<ReentrantConsole>(&mut byte), 1);
    assert_eq!(byte, [b'R']);
    assert_eq!(
        REENTRANT_CONSOLE_READS.load(Ordering::Acquire),
        1,
        "the nested contender must not call the platform reader",
    );

    let owner = std::thread::spawn(|| {
        let mut byte = [0u8; 1];
        let n = try_read_console_bytes::<BlockingConsole>(&mut byte);
        (n, byte)
    });
    while !BLOCKING_CONSOLE_ENTERED.load(Ordering::Acquire) {
        std::thread::yield_now();
    }

    let mut contender_byte = [0u8; 1];
    assert_eq!(
        try_read_console_bytes::<BlockingConsole>(&mut contender_byte),
        0,
        "a concurrent hart must skip instead of reading the UART twice",
    );
    assert_eq!(contender_byte, [0]);
    BLOCKING_CONSOLE_RELEASE.store(true, Ordering::Release);

    let (n, owner_byte) = owner.join().expect("console owner thread");
    assert_eq!(n, 1);
    assert_eq!(owner_byte, [b'S']);
    assert_eq!(
        BLOCKING_CONSOLE_READS.load(Ordering::Acquire),
        1,
        "only one concurrent consumer may reach ConsoleIf::read_bytes",
    );
}

/// `register_irq_handler(VIRT_UART_IRQ, fake_handler)` populates the
/// global dispatch table at the slot indexed by the IRQ number.
///
/// Asserts the explicit-registration shape pinned by Open Q #4: the
/// dispatch table is mutated in-place, not via a link-time aggregator.
#[test]
fn register_irq_handler_populates_dispatch_table_slot() {
    let _setup = setup();

    fn fake(_irq: u32) -> IrqHandled {
        IrqHandled::Done
    }

    assert!(
        handler_for(<IrqTestPlatform as IrqIf>::UART_IRQ).is_none(),
        "table should start empty after reset"
    );

    register_irq_handler(<IrqTestPlatform as IrqIf>::UART_IRQ, fake);

    let installed = handler_for(<IrqTestPlatform as IrqIf>::UART_IRQ).expect("registered");
    assert_eq!(
        (installed as *const ()),
        (fake as *const ()),
        "dispatch slot should hold the registered handler",
    );
}

/// `install_irq_handlers::<P>()` registers handlers under the IRQ numbers
/// returned by the platform's runtime accessors and publishes the dispatch
/// table via `install_dispatch_table`.
///
/// The test deliberately makes those values differ from the associated
/// constant fallbacks so it catches any regression to constant-only wiring.
#[test]
fn install_irq_handlers_publishes_table_to_platform() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();

    install_irq_handlers::<IrqTestPlatform>();

    let uart_irq = <IrqTestPlatform as IrqIf>::uart_irq();
    let rtc_irq = <IrqTestPlatform as IrqIf>::rtc_irq();

    assert_ne!(uart_irq, <IrqTestPlatform as IrqIf>::UART_IRQ);
    assert_ne!(rtc_irq, <IrqTestPlatform as IrqIf>::RTC_IRQ);

    let installed = handler_for(uart_irq).expect("UART handler");
    assert_eq!(
        (installed as *const ()),
        (uart_rx_irq_handler::<IrqTestPlatform> as *const ()),
        "UART_IRQ slot should hold the canonical UART RX handler",
    );

    let table_ptr = IRQ_TEST_INSTALLED_TABLE_PTR.load(Ordering::Acquire);
    assert!(
        table_ptr != 0,
        "install_dispatch_table must have been called",
    );

    assert_eq!(
        IRQ_TEST_LAST_PRIORITY.load(Ordering::Acquire),
        1,
        "UART IRQ should be made deliverable once the IRQ-safe handler is installed",
    );
    assert!(
        IRQ_TEST_UART_UNMASKED.load(Ordering::Acquire),
        "UART IRQ should be unmasked so console input wakes the reactor",
    );

    let installed = handler_for(rtc_irq).expect("RTC handler");
    assert_eq!(
        (installed as *const ()),
        (rtc_alarm_irq_handler::<IrqTestPlatform> as *const ()),
        "RTC_IRQ slot should hold the canonical RTC alarm handler",
    );
    assert!(
        IRQ_TEST_RTC_UNMASKED.load(Ordering::Acquire),
        "RTC IRQ should be unmasked after its event source is initialized",
    );
}

#[test]
fn install_irq_handlers_skips_runtime_zero_sentinels() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();
    IRQ_TEST_RUNTIME_UART_IRQ.store(32, Ordering::Release);
    IRQ_TEST_RUNTIME_RTC_IRQ.store(0, Ordering::Release);

    install_irq_handlers::<IrqTestPlatform>();

    assert!(
        handler_for(32).is_some(),
        "runtime UART IRQ must be installed"
    );
    assert!(handler_for(<IrqTestPlatform as IrqIf>::UART_IRQ).is_none());
    assert!(handler_for(<IrqTestPlatform as IrqIf>::RTC_IRQ).is_none());
    assert!(IRQ_TEST_UART_UNMASKED.load(Ordering::Acquire));
    assert!(!IRQ_TEST_RTC_UNMASKED.load(Ordering::Acquire));
}

#[test]
fn net_irq_bottom_half_acks_device_before_same_hart_completion() {
    let _setup = setup();

    assert_eq!(
        publish_deferred_net_claim::<IrqTestPlatform>(
            IRQ_TEST_DEVICE_IRQ,
            BoundDeviceKey(0),
            &IRQ_TEST_NET_REGISTRATION,
        ),
        IrqHandled::DeferredWake
    );
    assert_eq!(
        IRQ_TEST_COMPLETE_COUNT.load(Ordering::Acquire),
        0,
        "top half must leave controller completion outstanding",
    );
    assert_eq!(IRQ_TEST_NET_MASK_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(IRQ_TEST_NET_UNMASK_COUNT.load(Ordering::Acquire), 0);

    assert!(drain_net_rx_irq::<IrqTestPlatform>());
    assert_eq!(IRQ_TEST_NET_ACK_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(IRQ_TEST_COMPLETE_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(
        IRQ_TEST_COMPLETED_IRQ.load(Ordering::Acquire),
        IRQ_TEST_DEVICE_IRQ
    );
    assert_eq!(IRQ_TEST_ACK_ORDER.load(Ordering::Acquire), 1);
    assert_eq!(
        IRQ_TEST_COMPLETE_ORDER.load(Ordering::Acquire),
        2,
        "device ACK/poll must precede controller completion",
    );
    assert_eq!(IRQ_TEST_NET_UNMASK_COUNT.load(Ordering::Acquire), 1);
    assert_eq!(
        IRQ_TEST_UNMASK_ORDER.load(Ordering::Acquire),
        3,
        "controller completion must precede unmask",
    );
    assert!(!drain_net_rx_irq::<IrqTestPlatform>());
    assert_eq!(
        net_irq_stats(),
        crate::irq::NetIrqStats {
            claims: 1,
            completions: 1,
            wrong_hart_drains: 0,
            missing_device_drains: 0,
        }
    );
}

#[test]
fn net_irq_claim_can_only_be_completed_by_its_claimant_hart() {
    let _setup = setup();

    IRQ_TEST_CURRENT_CPU.store(0, Ordering::Release);
    assert_eq!(
        publish_deferred_net_claim::<IrqTestPlatform>(
            IRQ_TEST_DEVICE_IRQ,
            BoundDeviceKey(0),
            &IRQ_TEST_NET_REGISTRATION,
        ),
        IrqHandled::DeferredWake
    );

    IRQ_TEST_CURRENT_CPU.store(1, Ordering::Release);
    assert!(!drain_net_rx_irq::<IrqTestPlatform>());
    assert_eq!(IRQ_TEST_NET_ACK_COUNT.load(Ordering::Acquire), 0);
    assert_eq!(IRQ_TEST_COMPLETE_COUNT.load(Ordering::Acquire), 0);

    IRQ_TEST_CURRENT_CPU.store(0, Ordering::Release);
    assert!(drain_net_rx_irq::<IrqTestPlatform>());
    let stats = net_irq_stats();
    assert_eq!(stats.claims, 1);
    assert_eq!(stats.completions, 1);
    assert_eq!(
        stats.wrong_hart_drains, 0,
        "a non-owner hart checks its own idle slot rather than touching the claimant's slot",
    );
}

#[test]
fn rtc_irq_handler_publishes_alarm_event_to_devfs_rtc_state() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();
    install_irq_handlers::<IrqTestPlatform>();

    let guard = tx_subsystems::tty::adapter::step_engine::guard();
    let rtc_ops = tx_fs::devfs::RTC_CHAR_BINDING
        .ops
        .rtc_ops()
        .expect("rtc ops");
    assert_eq!(
        rtc_ops.poll_events(&guard),
        Ok(tx_subsystems::device::RtcEventMask::empty())
    );

    assert_eq!(
        rtc_alarm_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::rtc_irq()),
        IrqHandled::Wake
    );
    assert_eq!(
        rtc_ops.poll_events(&guard),
        Ok(tx_subsystems::device::RtcEventMask::ALARM)
    );
}

/// End-to-end IRQ → TTY ingest path: with the UART RX handler
/// registered, queue a byte through the fake platform's RX source,
/// invoke the handler directly (the same code path
/// `Platform::dispatch_irq` walks through the installed table), and
/// observe that the handler only requests a reactor wake. The actual
/// TTY ingest must run later in normal kernel context, where creating
/// an epoch guard is legal.
#[test]
fn dispatch_irq_routes_uart_rx_to_tty_deferred_ingest() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();
    install_irq_handlers::<IrqTestPlatform>();

    // Fake UART bytes ready in the platform's RX source. The boot
    // console TTY runs in cooked mode (ICANON), so a complete line
    // (`X\n`) is required for the ldisc to flush bytes into the
    // user-visible input queue.
    //
    // 2026-05-13: Per the IRQ-context-safety rewrite, the handler no
    // longer calls `step_ingest` inline (epoch::guard's
    // `debug_assert!` rejects creation in IRQ context). Bytes land in
    // `UART_RX_PENDING` instead, and `drain_uart_rx_pending` is the
    // non-IRQ counterpart that feeds them to the line discipline. The
    // test exercises both halves to match the production wiring (the
    // reactor loop calls `drain_uart_rx_pending` after every WFI
    // wake).
    {
        let mut queue = IRQ_TEST_RX_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.extend_from_slice(b"X\n");
    }

    let handled = uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::uart_irq());
    assert_eq!(
        handled,
        IrqHandled::Wake,
        "buffered bytes should request a reactor wake",
    );

    let tty = console_tty().expect("CONSOLE_TTY populated by register_console_hardware");
    let payload = tty
        .live_payload()
        .expect("console TTY payload alive before deferred ingest");
    assert!(
        IRQ_TEST_RX_QUEUE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty(),
        "IRQ handler should drain platform RX into the IRQ-safe pending buffer",
    );
    let before: std::vec::Vec<u8> = payload.with_input_queue(|queue| {
        let mut tmp = [0u8; 64];
        let n = queue.drain_to_slice(&mut tmp);
        tmp[..n].to_vec()
    });
    assert!(before.is_empty(), "IRQ handler must not ingest into TTY");

    // Drain the deferred buffer the way the reactor loop does on every
    // WFI return.
    let exclusions_before = IRQ_TEST_LOCAL_EXCLUSION_COUNT.load(Ordering::Acquire);
    let drained = CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty();
    assert_eq!(drained, 2, "drain_uart_rx_pending should consume X\\n");
    assert_eq!(
        IRQ_TEST_LOCAL_EXCLUSION_COUNT.load(Ordering::Acquire),
        exclusions_before + 3,
        "the snapshot, final empty check, and owner-release handoff recheck must exclude the local IRQ top half",
    );

    // Normal-context drain should now contain the committed line.
    let snapshot: std::vec::Vec<u8> = payload.with_input_queue(|queue| {
        let mut tmp = [0u8; 64];
        let n = queue.drain_to_slice(&mut tmp);
        tmp[..n].to_vec()
    });
    assert_eq!(
        snapshot, b"X\n",
        "step_ingest should have queued the committed line into the input queue",
    );
}

#[test]
fn uart_rx_irq_leaves_fifo_untouched_when_pending_buffer_is_contended() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();

    {
        let mut queue = IRQ_TEST_RX_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.extend_from_slice(b"Y\n");
    }

    let pending = UART_RX_PENDING.lock();
    let handled = uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::uart_irq());
    assert_eq!(handled, IrqHandled::Done);
    assert_eq!(
        IRQ_TEST_RX_QUEUE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_slice(),
        b"Y\n",
        "a contended top half must not consume bytes from the hardware FIFO",
    );
    drop(pending);

    assert_eq!(
        uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::uart_irq()),
        IrqHandled::Wake,
    );
    assert_eq!(
        CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty(),
        2,
    );
}

#[test]
fn console_rx_polling_cannot_overtake_an_irq_buffered_chunk() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();

    let ingest_owner = try_acquire_console_rx_ingest().expect("test owns deferred ingest");
    {
        let mut queue = IRQ_TEST_RX_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.extend_from_slice(b"A");
    }
    assert_eq!(
        uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::uart_irq()),
        IrqHandled::Wake,
    );
    assert_eq!(
        CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty(),
        0,
        "a competing reactor must not enter the TTY ingest path",
    );

    {
        let mut queue = IRQ_TEST_RX_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.extend_from_slice(b"B\n");
    }
    assert_eq!(poll_console_rx_into_pending::<IrqTestPlatform>(), 2);
    drop(ingest_owner);

    assert_eq!(
        CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty(),
        3,
    );
    let tty = console_tty().expect("console TTY");
    let payload = tty.live_payload().expect("console TTY payload");
    let snapshot: std::vec::Vec<u8> = payload.with_input_queue(|queue| {
        let mut tmp = [0u8; 8];
        let n = queue.drain_to_slice(&mut tmp);
        tmp[..n].to_vec()
    });
    assert_eq!(snapshot, b"AB\n");
}

#[test]
fn console_rx_deferred_ingest_stays_on_the_boot_hart() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();

    {
        let mut queue = IRQ_TEST_RX_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.extend_from_slice(b"C\n");
    }
    assert_eq!(
        uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::uart_irq()),
        IrqHandled::Wake,
    );

    IRQ_TEST_CURRENT_CPU.store(1, Ordering::Release);
    assert_eq!(
        CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty(),
        0,
        "an AP reactor must leave the boot console's pending bytes untouched",
    );

    IRQ_TEST_CURRENT_CPU.store(0, Ordering::Release);
    assert_eq!(
        CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty(),
        2,
    );
    let tty = console_tty().expect("console TTY");
    let payload = tty.live_payload().expect("console TTY payload");
    let snapshot: std::vec::Vec<u8> = payload.with_input_queue(|queue| {
        let mut tmp = [0u8; 4];
        let n = queue.drain_to_slice(&mut tmp);
        tmp[..n].to_vec()
    });
    assert_eq!(snapshot, b"C\n");
}

#[test]
fn dispatch_irq_vintr_delivers_sigint_to_foreground_pgrp() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();
    install_irq_handlers::<IrqTestPlatform>();

    let init = tx_subsystems::process::execution::init_process().expect("init process");
    let tty = console_tty().expect("CONSOLE_TTY populated by register_console_hardware");
    let guard = tx_subsystems::tty::adapter::step_engine::guard();
    assert!(matches!(
        tx_subsystems::tty::execution::step_ioctl_tiocsctty_for_process(&tty, &init, &guard),
        tx_subsystems::tty::adapter::step_engine::StepOutcome::Done(_)
    ));
    drop(guard);

    {
        let mut queue = IRQ_TEST_RX_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.push(0x03);
    }

    let handled = uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::uart_irq());
    assert_eq!(handled, IrqHandled::Wake);
    assert_eq!(
        CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty(),
        1
    );
    assert!(
        init_has_pending_sigint(),
        "VINTR through the hardware console drain must post SIGINT to the foreground pgrp"
    );
}
