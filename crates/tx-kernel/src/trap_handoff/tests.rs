//! Trap-handoff tests (Phase 1).
//!
//! Synthesise a `TrapFrameView` directly from `tx_hal` constructors
//! and assert the platform-neutral translation rules. No `TrapFrameMut`
//! is required for these tests because `translate_*` functions take
//! the read-only view.
//!
//! The test platform `TestPlatform` is a minimal `TxPlatform` impl
//! used only to satisfy the `<P>` type parameter on the public
//! translation helpers. None of its trait methods are actually called
//! from these tests; the translation paths are platform-erased today
//! (the `<P>` parameter is reserved for a future generic
//! `TrapFrameView<'_, P>` shape pinned in the plan).

use tx_hal::{
    Arch, BootHandoff, BootInfo, BootPlatformIf, BootProtocol, ConsoleIf, FaultInfo, InitIf,
    ObserverIf, PlatformConfig, PlatformInfo, TrapFrameView, TrapPreviousMode, VirtAddr,
};

use crate::trap_handoff::{
    translate_syscall, translate_user_pf, AccessKind, PageFaultInfo as HandoffPageFaultInfo,
};

struct TestPlatform;

impl PlatformConfig for TestPlatform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "trap-handoff-test";
}

impl BootPlatformIf for TestPlatform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvDirect;
}

impl InitIf for TestPlatform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

static EMPTY_BOOT_INFO: BootInfo = BootInfo::empty();
static TEST_PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: TestPlatform::BOARD,
    spi_sd: None,
    mmio_regions: &[],
    timebase_frequency_hz: 0,
    possible_cpu_count: 1,
};

impl tx_hal::BootInfoIf for TestPlatform {
    fn boot_info() -> &'static BootInfo {
        &EMPTY_BOOT_INFO
    }
}

impl tx_hal::PlatformInfoIf for TestPlatform {
    fn platform_info() -> &'static PlatformInfo {
        &TEST_PLATFORM_INFO
    }
}

impl tx_hal::AuxvIf for TestPlatform {}

impl ConsoleIf for TestPlatform {
    fn write_bytes(_bytes: &[u8]) {}
}

impl tx_hal::PmapIf for TestPlatform {}

impl tx_hal::TrapIf for TestPlatform {}

impl tx_hal::SignalFrameIf for TestPlatform {}

impl tx_hal::IrqIf for TestPlatform {}

impl tx_hal::TimeIf for TestPlatform {
    fn read_ns() -> u64 {
        0
    }
    fn set_deadline_ns(_deadline: u64) {}
    fn cancel_deadline() {}
    fn frequency_hz() -> u64 {
        1_000_000_000
    }
}

impl tx_hal::PercpuIf for TestPlatform {}

impl tx_hal::CacheIf for TestPlatform {}

impl tx_hal::DmaIf for TestPlatform {}

impl tx_hal::SmpIf for TestPlatform {}

impl tx_hal::EntropyIf for TestPlatform {}
impl ObserverIf for TestPlatform {}

impl tx_hal::PowerIf for TestPlatform {
    fn system_off() -> ! {
        #[allow(clippy::empty_loop)]
        loop {}
    }
}

fn make_view(syscall_number: u64, syscall_args: [u64; 6]) -> TrapFrameView {
    TrapFrameView::new(
        VirtAddr(0x1000),
        VirtAddr(0x7fff_ffff_0000),
        syscall_number,
        syscall_args,
        None,
        None,
        TrapPreviousMode::User,
        true,
        0,
    )
}

fn make_pf_view() -> TrapFrameView {
    TrapFrameView::new(
        VirtAddr(0x1000),
        VirtAddr(0x7fff_ffff_0000),
        0,
        [0; 6],
        Some(VirtAddr(0xdead_0000)),
        Some(VirtAddr(0x1000)),
        TrapPreviousMode::User,
        true,
        0,
    )
}

#[test]
fn translate_syscall_packs_a7_to_nr_and_a0_a5_to_args() {
    // RV64 ABI: a7 = syscall number, a0..a5 = args[0..6].
    // The HAL trap-frame already lays a7 into `syscall_number` and
    // a0..a5 into `syscall_args[0..6]`, so the trap-handoff
    // translation is a 1:1 copy. The test pins that contract.
    let view = make_view(64 /* NR_WRITE on RV64 */, [1, 0xdead_beef, 6, 0, 0, 0]);
    let req = translate_syscall::<TestPlatform>(&view);
    assert_eq!(req.nr, 64);
    assert_eq!(req.args, [1, 0xdead_beef, 6, 0, 0, 0]);
}

#[test]
fn translate_user_pf_marks_write_when_store_fault() {
    // A store-side user page fault must yield AccessKind::Write
    // and surface from_user=true so the trap shell hands off to
    // the active userspace-run wait.
    let view = make_pf_view();
    let fault = FaultInfo {
        address: VirtAddr(0xdead_0000),
        write: true,
        instruction: false,
        from_user: true,
    };

    let info: HandoffPageFaultInfo = translate_user_pf::<TestPlatform>(&view, &fault);
    assert_eq!(info.access, AccessKind::Write);
    assert!(info.from_user);
    assert_eq!(info.addr.raw(), 0xdead_0000);
    // Phase 1: present is conservatively false; the canonical fault
    // script re-derives presence from the address-space recipe.
    assert!(!info.present);
}

#[test]
fn translate_user_pf_marks_read_when_load_fault() {
    let view = make_pf_view();
    let fault = FaultInfo {
        address: VirtAddr(0x4000_0000),
        write: false,
        instruction: false,
        from_user: true,
    };

    let info = translate_user_pf::<TestPlatform>(&view, &fault);
    assert_eq!(info.access, AccessKind::Read);
    assert!(info.from_user);
}

#[test]
fn translate_user_pf_marks_execute_when_instruction_fault() {
    let view = make_pf_view();
    let fault = FaultInfo {
        address: VirtAddr(0x1234_5678),
        write: false,
        instruction: true,
        from_user: true,
    };

    let info = translate_user_pf::<TestPlatform>(&view, &fault);
    assert_eq!(info.access, AccessKind::Execute);
}

#[test]
fn translate_user_pf_propagates_kernel_mode_fault_marker() {
    let view = make_pf_view();
    let fault = FaultInfo {
        address: VirtAddr(0xffff_8000_0000_0000),
        write: false,
        instruction: false,
        from_user: false,
    };

    let info = translate_user_pf::<TestPlatform>(&view, &fault);
    assert!(!info.from_user);
}
