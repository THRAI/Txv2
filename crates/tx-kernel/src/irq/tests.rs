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
    handler_for, install_irq_handlers, register_irq_handler, rtc_alarm_irq_handler,
    uart_rx_irq_handler,
};
use crate::test_serialise::KERNEL_TEST_LOCK as IRQ_TEST_LOCK;
use tx_subsystems::signal::Signum;

const TEST_PAGE_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// Fake platform with a queueable RX source + capture for
// `install_dispatch_table`.
// ---------------------------------------------------------------------------

struct IrqTestPlatform;

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
    /// Pick a non-zero IRQ so the test isn't accidentally aliased to
    /// the IRQ-0 sentinel that `KernelTrapDispatcher::on_external_irq`
    /// short-circuits on.
    const UART_IRQ: u32 = 7;
    const RTC_IRQ: u32 = 8;

    fn exclude_local_execution() -> tx_hal::LocalExecutionGuard {
        unsafe { tx_hal::LocalExecutionGuard::new(0, restore_test_local_execution) }
    }

    fn install_dispatch_table(table: &'static IrqDispatchTable) {
        IRQ_TEST_INSTALLED_TABLE_PTR
            .store(table as *const IrqDispatchTable as usize, Ordering::Release);
    }

    fn set_priority(_irq: u32, priority: u8) {
        IRQ_TEST_LAST_PRIORITY.store(priority as u32, Ordering::Release);
    }

    fn unmask(irq: u32) {
        if irq == <Self as IrqIf>::UART_IRQ {
            IRQ_TEST_UART_UNMASKED.store(true, Ordering::Release);
        }
        if irq == <Self as IrqIf>::RTC_IRQ {
            IRQ_TEST_RTC_UNMASKED.store(true, Ordering::Release);
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

impl tx_hal::PercpuIf for IrqTestPlatform {}
impl tx_hal::CacheIf for IrqTestPlatform {}
impl tx_hal::DmaIf for IrqTestPlatform {}
impl tx_hal::SmpIf for IrqTestPlatform {}

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
    IRQ_TEST_RX_QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    IRQ_TEST_INSTALLED_TABLE_PTR.store(0, Ordering::Release);
    IRQ_TEST_LAST_PRIORITY.store(0, Ordering::Release);
    IRQ_TEST_UART_UNMASKED.store(false, Ordering::Release);
    IRQ_TEST_RTC_UNMASKED.store(false, Ordering::Release);
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

/// `install_irq_handlers::<P>()` registers the UART RX handler under
/// `<P as IrqIf>::UART_IRQ` and publishes the dispatch table to the
/// platform via `install_dispatch_table`.
///
/// Asserts Open Q #6's late-binding shape: tx-kernel reads the IRQ
/// number through `<P as IrqIf>::UART_IRQ` only — no `pub const
/// VIRT_UART_IRQ` baked into tx-kernel.
#[test]
fn install_irq_handlers_publishes_table_to_platform() {
    let _setup = setup();
    bootstrap_init_for_irq_test();
    CoreInit::<IrqTestPlatform>::register_console_hardware();

    install_irq_handlers::<IrqTestPlatform>();

    let installed = handler_for(<IrqTestPlatform as IrqIf>::UART_IRQ).expect("UART handler");
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

    let installed = handler_for(<IrqTestPlatform as IrqIf>::RTC_IRQ).expect("RTC handler");
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
        rtc_alarm_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::RTC_IRQ),
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

    let handled = uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::UART_IRQ);
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
    let drained = CoreInit::<IrqTestPlatform>::drain_pending_uart_rx_into_tty();
    assert_eq!(drained, 2, "drain_uart_rx_pending should consume X\\n");

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

    let handled = uart_rx_irq_handler::<IrqTestPlatform>(<IrqTestPlatform as IrqIf>::UART_IRQ);
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
