#![no_std]

#[cfg(test)]
extern crate std;

use tx_hal::{
    AllocError, Arch, ArchAuxvFacts, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf,
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask, DmaIf,
    EntropyIf, FaultInfo, FpSimdIf, InitIf, IpiKind, IrqDispatchTable, IrqHandled, IrqIf,
    KernelTrapSink, MemoryRegion, MemoryRegionKind, MmioFlags, MmioRegion, ObserverIf, PercpuIf,
    PhysAddr, PhysRange, PlatformConfig, PlatformInfo, PlatformInfoIf, PmapError, PmapIf,
    PmapInvalidation, PmapPermissions, PmapReservation, PmapReservationIntermediates,
    PmapReserveKind, PmapRoot, PmapUnmapResult, Pod, PowerIf, PtNode, PtNodeAllocator,
    SavedSignalFrame, SecondaryEntry, SignalFrameIf, SignalFrameWrite,
    SignalHandlerRegs, SmpIf, TimeIf, TrapAction, TrapClass, TrapFrameMut, TrapFrameMutVtable,
    TrapFrameSnapshot, TrapFrameView, TrapIf, TrapPreviousMode, UserFpContext, UserPtr,
    UserSignalMaskAbi, UserTrapContext, VirtAddr, VirtRange,
};

pub use boot_args::capture_loongarch64_qemu_boot_args;
use boot_facts::ensure_static_boot_facts;
use la64_irq_trap::classify_la64_trap;
pub use la64_irq_trap::{dispatch_trap_frame, return_to_userspace};
#[cfg(target_arch = "loongarch64")]
use la64_pmap::la64_kernel_addr_to_phys;
use la64_pmap::{la64_cached_virt, la64_uncached_virt, uart_put_byte, uart_try_get_byte};

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

pub(crate) use la64_consts::*;
pub use la64_trap_frame::La64TrapFrame;
pub(crate) use la64_signal_frame::La64SignalFrame;

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

pub struct Platform;

#[cfg(target_arch = "loongarch64")]
struct La64RawFixupEntry {
    pc_start: unsafe extern "C" fn(),
    pc_end: unsafe extern "C" fn(),
    recovery_pc: unsafe extern "C" fn(),
}

#[cfg(target_arch = "loongarch64")]
unsafe impl Sync for La64RawFixupEntry {}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_cfu_ld_s();
    fn tx_la64_cfu_ld_e();
    fn tx_la64_cfu_fault();
    fn tx_la64_ctu_st_s();
    fn tx_la64_ctu_st_e();
    fn tx_la64_ctu_fault();
    fn tx_la64_cfu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
    fn tx_la64_ctu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
}

#[cfg(target_arch = "loongarch64")]
static LA64_FIXUP_TABLE: [La64RawFixupEntry; 2] = [
    La64RawFixupEntry {
        pc_start: tx_la64_cfu_ld_s,
        pc_end: tx_la64_cfu_ld_e,
        recovery_pc: tx_la64_cfu_fault,
    },
    La64RawFixupEntry {
        pc_start: tx_la64_ctu_st_s,
        pc_end: tx_la64_ctu_st_e,
        recovery_pc: tx_la64_ctu_fault,
    },
];

static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
static LA64_TIMEBASE_HZ: AtomicU64 = AtomicU64::new(0);
static LA64_POSSIBLE_CPU_COUNT: AtomicUsize = AtomicUsize::new(LA64_DEFAULT_POSSIBLE_CPUS);
static LA64_ONLINE_CPUS: AtomicU64 = AtomicU64::new(1);
static LA64_IPI_ACKED_CPUS: AtomicU64 = AtomicU64::new(0);
static LA64_IRQ_CONTEXT_DEPTHS: [AtomicUsize; LA64_MAX_BOOT_CPUS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
static LA64_IRQ_DISPATCH_TABLE: AtomicUsize = AtomicUsize::new(0);
/// LA64 supports a 10-bit ASID space (`ASID_BITS = 10`, `LA64_ASID_MASK =
/// 0x3ff`), i.e. 1024 ASIDs. The allocator must cover that whole space so that
/// EBR-deferred address-space reclaim (retired-but-not-yet-freed `PmapRoot`s
/// each still holding their ASID) cannot exhaust the pool under fork-heavy
/// workloads — a single `u64` (63 usable ASIDs) starved `fork()` with `EAGAIN`
/// once ~63 roots were in flight. Mirrors the RV64 16-word bitmap.
const LA64_ASID_BITMAP_WORDS: usize = 16;
const LA64_ASID_CAPACITY: usize = LA64_ASID_BITMAP_WORDS * u64::BITS as usize;
static LA64_ALLOCATED_ASIDS: [AtomicU64; LA64_ASID_BITMAP_WORDS] =
    [const { AtomicU64::new(0) }; LA64_ASID_BITMAP_WORDS];
static LA64_KERNEL_PGDH_PHYS: AtomicUsize = AtomicUsize::new(0);
static LA64_KERNEL_PGDH_BOOTSTRAP_MAPPED: AtomicBool = AtomicBool::new(false);
static LA64_ACTIVE_PGDL: AtomicUsize = AtomicUsize::new(0);
static LA64_ACTIVE_PGDH: AtomicUsize = AtomicUsize::new(0);
static LA64_ACTIVE_ASID: AtomicUsize = AtomicUsize::new(0);
static LA64_COMMITTED_PT_NODE_REGISTRY_LOCK: AtomicBool = AtomicBool::new(false);
const LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS: usize = 4096;
static LA64_COMMITTED_PT_NODES: La64CommittedPtNodeRegistry = La64CommittedPtNodeRegistry(
    UnsafeCell::new([None; LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS]),
);
#[cfg(target_arch = "loongarch64")]
static LA64_KERNEL_TLS_VALID: [AtomicBool; LA64_MAX_BOOT_CPUS] = [
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
    AtomicBool::new(false),
];
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_KERNEL_TLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_EIOINTC_ENABLE0: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_EIOINTC_COREISR0: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_PCH_PIC_MASK: AtomicU64 = AtomicU64::new(u64::MAX);

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const LA64_FW_CFG_INITRD_CAPACITY: usize = 8 * 1024 * 1024;

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
#[repr(C, align(4096))]
struct La64FwCfgInitrdBuffer {
    bytes: [u8; LA64_FW_CFG_INITRD_CAPACITY],
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static mut LA64_FW_CFG_INITRD_BUFFER: La64FwCfgInitrdBuffer = La64FwCfgInitrdBuffer {
    bytes: [0; LA64_FW_CFG_INITRD_CAPACITY],
};

#[repr(C, align(8))]
pub struct KernelResumeCtx {
    pub sp: usize,
    pub ra: usize,
    pub r21: usize,
    pub tp: usize,
    pub r22: [usize; 10],
}

const _: () = assert!(core::mem::size_of::<KernelResumeCtx>() == 14 * 8);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, sp) == 0);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, ra) == 8);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, r21) == 16);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, tp) == 24);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, r22) == 32);

#[repr(transparent)]
pub struct PerHartCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for PerHartCell<T> {}

impl<T> PerHartCell<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0.get()
    }
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static LA64_KERNEL_RESUME_CTX: [PerHartCell<KernelResumeCtx>; LA64_MAX_BOOT_CPUS] = [
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
];

const LA64_TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
pub struct La64TrapStack(pub [u8; LA64_TRAP_STACK_SIZE]);

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static LA64_TRAP_STACKS: [PerHartCell<La64TrapStack>; LA64_MAX_BOOT_CPUS] = [
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
];

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) fn la64_trap_stack_top_for_cpu(cpu: CpuId) -> usize {
    let stack = LA64_TRAP_STACKS[cpu.0].as_ptr();
    #[cfg(target_arch = "loongarch64")]
    {
        la64_cached_virt(la64_kernel_addr_to_phys(stack as usize)) + LA64_TRAP_STACK_SIZE
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        stack as usize + LA64_TRAP_STACK_SIZE
    }
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) fn la64_kernel_resume_ctx_ptr_for_cpu(cpu: CpuId) -> *mut KernelResumeCtx {
    let ptr = LA64_KERNEL_RESUME_CTX[cpu.0].as_ptr();
    #[cfg(target_arch = "loongarch64")]
    {
        la64_cached_virt(la64_kernel_addr_to_phys(ptr as usize)) as *mut KernelResumeCtx
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        ptr
    }
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static LA64_ENTRY_TRAP_FRAMES: [PerHartCell<La64TrapFrame>; LA64_MAX_BOOT_CPUS] = [
    PerHartCell::new(La64TrapFrame::empty()),
    PerHartCell::new(La64TrapFrame::empty()),
    PerHartCell::new(La64TrapFrame::empty()),
    PerHartCell::new(La64TrapFrame::empty()),
];

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) fn la64_entry_trap_frame_ptr_for_cpu(cpu: CpuId) -> *mut La64TrapFrame {
    let ptr = LA64_ENTRY_TRAP_FRAMES[cpu.0].as_ptr();
    #[cfg(target_arch = "loongarch64")]
    {
        la64_cached_virt(la64_kernel_addr_to_phys(ptr as usize)) as *mut La64TrapFrame
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        ptr
    }
}

struct La64CommittedPtNodeRegistry(
    UnsafeCell<[Option<PtNode>; LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS]>,
);

unsafe impl Sync for La64CommittedPtNodeRegistry {}

struct La64CommittedPtNodeRegistryGuard;

impl Drop for La64CommittedPtNodeRegistryGuard {
    fn drop(&mut self) {
        LA64_COMMITTED_PT_NODE_REGISTRY_LOCK.store(false, Ordering::Release);
    }
}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_qemu_fp_save_context(ctx: *mut UserFpContext) -> usize;
    fn tx_la64_qemu_fp_restore_context(ctx: *const UserFpContext);
}

pub(crate) fn la64_capture_fp_context() -> UserFpContext {
    let mut fp = <Platform as FpSimdIf>::init_state();
    <Platform as FpSimdIf>::save(&mut fp);
    fp
}

pub(crate) fn la64_restore_fp_context(fp: &UserFpContext) {
    <Platform as FpSimdIf>::restore(fp);
}

#[cfg(target_arch = "loongarch64")]
fn la64_set_fpu_enabled(enabled: bool) {
    let mut euen = la64_irq_trap::read_la64_csr(LA64_CSR_EUEN);
    if enabled {
        euen |= LA64_EUEN_FPE;
    } else {
        euen &= !(LA64_EUEN_FPE | LA64_EUEN_SXE | LA64_EUEN_ASXE);
    }
    la64_irq_trap::write_la64_csr(LA64_CSR_EUEN, euen);
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_set_fpu_enabled(_enabled: bool) {}

#[cfg(target_arch = "loongarch64")]
fn la64_save_fp_context(state: &mut UserFpContext) {
    let saved = unsafe { tx_la64_qemu_fp_save_context(core::ptr::addr_of_mut!(*state)) };
    if saved == 0 {
        *state = UserFpContext::empty();
    }
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_save_fp_context(state: &mut UserFpContext) {
    *state = UserFpContext::empty();
}

#[cfg(target_arch = "loongarch64")]
fn la64_restore_fp_context_raw(state: &UserFpContext) {
    unsafe { tx_la64_qemu_fp_restore_context(core::ptr::addr_of!(*state)) };
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_restore_fp_context_raw(state: &UserFpContext) {
    #[cfg(test)]
    {
        *TEST_RESTORED_FP_CONTEXT
            .lock()
            .expect("LA64 restored FP test mutex poisoned") = *state;
    }

    #[cfg(not(test))]
    {
        let _ = state;
    }
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
static TEST_RESTORED_FP_CONTEXT: std::sync::Mutex<UserFpContext> =
    std::sync::Mutex::new(UserFpContext::empty());

#[cfg(all(test, not(target_arch = "loongarch64")))]
fn la64_test_reset_restored_fp_context() {
    *TEST_RESTORED_FP_CONTEXT
        .lock()
        .expect("LA64 restored FP test mutex poisoned") = UserFpContext::empty();
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
fn la64_test_restored_fp_context() -> UserFpContext {
    *TEST_RESTORED_FP_CONTEXT
        .lock()
        .expect("LA64 restored FP test mutex poisoned")
}

// QEMU loongson3-virt exposes the first serial port as an 8250-compatible
// UART at 0x1fe0_01e0; Linux examples use earlycon=uart,mmio,0x1fe001e0.
const QEMU_LA64_UART0_BASE: usize = 0x1fe0_01e0;
/// LS2K1000 board UART0 (NPUcore-BLOSSOM's proven base; their code
/// warns it MUST be reached through the uncached DMW window, which
/// `la64_uncached_virt` already applies).
const LS2K1000_UART0_BASE: usize = 0x1fe2_0000;

/// Runtime board profile, selected by `tx.board=ls2k1000` on the boot
/// cmdline (U-Boot bootargs on the real board; absent on QEMU). The
/// switch happens inside `publish_static_boot_facts` right after the
/// cmdline is resolved — before the first boot sentinel prints — so
/// all regular console output already uses the right UART. Only
/// pre-parse output (feature-gated boot traces, very early traps)
/// still hits the QEMU default base.
/// The `la-board-ls2k1000` cargo feature bakes the board profile in
/// at compile time — the deterministic choice for board uimages,
/// because if the 2K1000 U-Boot hands over neither a cmdline nor a
/// device tree, the runtime `tx.board=` switch never gets a chance
/// to run and the console would sit on the wrong UART forever.
static LA64_IS_LS2K1000: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(cfg!(feature = "la-board-ls2k1000"));
static LA64_UART_BASE: AtomicUsize = AtomicUsize::new(if cfg!(feature = "la-board-ls2k1000") {
    LS2K1000_UART0_BASE
} else {
    QEMU_LA64_UART0_BASE
});

pub(crate) fn la64_board_is_ls2k1000() -> bool {
    LA64_IS_LS2K1000.load(Ordering::Relaxed)
}

pub(crate) fn la64_uart_base() -> usize {
    LA64_UART_BASE.load(Ordering::Relaxed)
}

pub(crate) fn la64_select_board_ls2k1000() {
    LA64_UART_BASE.store(LS2K1000_UART0_BASE, Ordering::Relaxed);
    LA64_IS_LS2K1000.store(true, Ordering::Relaxed);
}
const QEMU_LA64_UART0_SIZE: usize = 0x100;
#[cfg_attr(not(test), allow(dead_code))]
const QEMU_LA64_UART0_PAGE_BASE: usize = 0x1fe0_0000;
#[cfg(target_arch = "loongarch64")]
const UART_RBR: usize = 0x00;
const UART_THR: usize = 0x00;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const UART_IER: usize = 0x01;
#[cfg_attr(not(any(target_arch = "loongarch64", test)), allow(dead_code))]
const UART_IER_ERBFI: u8 = 1 << 0;
const UART_LSR: usize = 0x05;
#[cfg(target_arch = "loongarch64")]
const UART_LSR_DR: u8 = 1 << 0;
const UART_LSR_THRE: u8 = 1 << 5;

// The early UART is reachable through QEMU's current direct/identity execution
// convention. Phase-3 substrate MMIO mapping treats this exact page as already
// covered; all non-identity requests remain unsupported until LA64 owns real
// DMW/page-table mutation.
static MMIO_REGIONS: &[MmioRegion] = &[
    MmioRegion {
        name: "uart0",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_UART0_BASE),
            size: QEMU_LA64_UART0_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_BASE)),
            size: QEMU_LA64_UART0_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "pcie-ecam",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_PCIE_ECAM_BASE),
            size: QEMU_LA64_PCIE_ECAM_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_PCIE_ECAM_BASE)),
            size: QEMU_LA64_PCIE_ECAM_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "pcie-mmio32",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_PCIE_MMIO32_BASE),
            size: QEMU_LA64_PCIE_MMIO32_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_PCIE_MMIO32_BASE)),
            size: QEMU_LA64_PCIE_MMIO32_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "pch-msi",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_PCH_MSI_BASE),
            size: QEMU_LA64_PCH_MSI_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_PCH_MSI_BASE)),
            size: QEMU_LA64_PCH_MSI_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
];

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::LoongArch64;
    const BOARD: &'static str = "qemu-loongarch64-virt";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = 48;
    const VIRT_ADDR_BITS: u8 = 48;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(LA64_DMW_CACHED_BASE);
    const DIRECT_MAP_SIZE: usize = QEMU_LA64_DIRECT_MAP_SIZE;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(la64_cached_virt(QEMU_LA64_KERNEL_LOAD_BASE));
    const USER_TOP: VirtAddr = VirtAddr(LA64_USER_TOP);
    const KERNEL_STACK_SIZE: usize = 128 * 1024;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 4;
    const ASID_BITS: u8 = 10;
    const CACHE_LINE_SIZE: usize = 64;
    const DMA_COHERENT: bool = true;
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::LoongArchFirmware;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        // la 比 riscv 少一步"建引导页表":龙芯有 DMW 硬件直映射窗口(见 boot_asm.rs),
        // 早期汇编已设好,内核靠硬件窗口即可访问物理内存,无需软件页表。
        boot_args::record_legacy_firmware_arg(firmware_arg); // 记下固件参数(la 固件约定,供后续取用)
        ensure_static_boot_facts();                          // 采集并发布 BootInfo/PlatformInfo(等价 riscv 的发布步)

        // 套壳成交接单返回
        BootHandoff {
            cpu_id: CpuId(cpu_id),
            firmware_arg: BootArg(firmware_arg),
            protocol: Self::BOOT_PROTOCOL,
        }
    }
}

impl InitIf for Platform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

impl BootInfoIf for Platform {
    fn boot_info() -> &'static BootInfo {
        boot_facts::boot_info()
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        boot_facts::platform_info()
    }
}

impl AuxvIf for Platform {
    fn arch_auxv_facts() -> ArchAuxvFacts {
        ArchAuxvFacts::new(Self::PAGE_SIZE, 0, 0, "loongarch64")
    }
}
impl ConsoleIf for Platform {
    fn write_bytes(bytes: &[u8]) {
        for &byte in bytes {
            uart_put_byte(byte);
        }
    }

    fn read_bytes(buf: &mut [u8]) -> usize {
        let mut read = 0;
        for byte in buf {
            let Some(next) = uart_try_get_byte() else {
                break;
            };
            *byte = next;
            read += 1;
        }
        read
    }
}

impl ObserverIf for Platform {}

mod boot_args;
mod boot_asm;
mod boot_facts;
mod boot_firmware;
mod boot_smp;
mod dtb;
mod la64_irq_trap;
mod la64_consts;
mod la64_liointc;
mod la64_percpu;
mod la64_pmap;
mod la64_signal_frame;
mod la64_trap_frame;
mod la64_unaligned;
mod platform_impls;
mod trap_asm;

#[cfg(test)]
mod tests;
