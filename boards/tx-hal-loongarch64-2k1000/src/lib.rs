#![no_std]

#[cfg(test)]
extern crate std;

use tx_hal::CpuPinReason;
use tx_hal::{
    AllocError, Arch, ArchAuxvFacts, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf,
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask,
    CpuPinGuard, DeadlineTimerIf, DeviceId, DeviceLocalId, DeviceMatchId, DeviceResource,
    DeviceResourceGraph, DeviceStatus, DmaCoherency, DmaConstraints, DmaDirection, DmaDomain,
    DmaDomainId, DmaDomainRef, DmaIf, DmaTranslation, EntropyIf, FaultInfo, FpSimdIf, InitIf,
    InterruptWaitState, IpiKind, IrqDispatchTable, IrqHandled, IrqIf, IrqPolarity, IrqResource,
    IrqSharing, IrqTrigger, KernelTrapSink, LocalExecutionGuard, MacAddressSource, MemoryRegion,
    MemoryRegionKind, MmioFlags, MmioRegion, MmioResource, MonotonicCounterIf, ObserverIf,
    PercpuIf, PersistentClockIf, PhyRef, PhysAddr, PhysRange, PlatformConfig, PlatformInfo,
    PlatformInfoIf, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReservationIntermediates, PmapReserveKind, PmapRoot, PmapUnmapResult, Pod, PowerIf, PtNode,
    PtNodeAllocator, ResourceOrigin, ResourceOriginKind, ResourceProviderId, ResourceRole,
    SavedSignalFrame, SecondaryEntry, SignalFrameIf, SignalFrameWrite, SignalHandlerRegs, SmpIf,
    TrapAction, TrapClass, TrapFrameMut, TrapFrameMutVtable, TrapFrameSnapshot, TrapFrameView,
    TrapIf, TrapPreviousMode, UserFpContext, UserPtr, UserSignalMaskAbi, UserTrapContext, VirtAddr,
    VirtRange,
};

use boot_facts::ensure_static_boot_facts;
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use la64_irq_trap::classify_la64_trap;
pub use la64_irq_trap::{dispatch_trap_frame, return_to_userspace};
use la64_pmap::la64_uncached_virt;
#[cfg(target_arch = "loongarch64")]
use la64_pmap::{la64_cached_virt, la64_kernel_addr_to_phys};

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

const LA2K1000_RAM0_BASE: usize = 0;
const LA2K1000_RAM0_SIZE: usize = 0x1000_0000;
const LA2K1000_RAM1_BASE: usize = 0x9000_0000;
const LA2K1000_RAM1_SIZE: usize = 0x3000_0000;
const LA2K1000_KERNEL_LOAD_BASE: usize = 0x9800_0000;
const LA2K1000_FDT_SCRATCH_BASE: usize = 0x0a00_0000;
const LA2K1000_FDT_SCRATCH_SIZE: usize = 0x0001_0000;
const LA2K1000_FRAMEBUFFER_BASE: usize = 0x0b00_0000;
const LA2K1000_FRAMEBUFFER_SIZE: usize = 0x0400_0000;
const LA2K1000_BOOTPARAM_BASE: usize = 0x0f00_0000;
const LA2K1000_BOOTPARAM_SIZE: usize = 0x0100_0000;
const LA2K1000_UART_BASE: usize = 0x1fe2_0000;
const LA2K1000_UART_SIZE: usize = 0x100;
const LA2K1000_AHCI_BASE: usize = 0x400e_0000;
const LA2K1000_AHCI_SIZE: usize = 0x0001_0000;
const LA2K1000_GMAC1_BASE: usize = 0x4005_0000;
const LA2K1000_GMAC1_SIZE: usize = 0x0000_8000;
#[cfg(target_arch = "loongarch64")]
const LA2K1000_GMAC_PINMUX_PHYS: usize = 0x1fe0_0420;
const LA2K1000_GMAC1_PINMUX_BIT: u32 = 1 << 3;
/// GMAC1's live PHY is the C22 address-zero node under its integrated MDIO bus.
pub const LA2K1000_GMAC1_PHY_ADDRESS: u8 = 0;
/// The Nebula wiring uses RGMII with internal TX/RX delay.
pub const LA2K1000_GMAC1_PHY_MODE: &str = "rgmii-id";
/// Stable locally administered address used when firmware does not publish one.
pub const LA2K1000_GMAC1_MAC_ADDRESS: [u8; 6] = [0x0e, 0x40, 0xdf, 0xbc, 0x58, 0x4e];
const LA64_MAX_BOOT_CPUS: usize = 2;
const LA64_DEFAULT_POSSIBLE_CPUS: usize = LA64_MAX_BOOT_CPUS;
const LA64_DMW_CACHED_BASE: usize = 0x9000_0000_0000_0000;
const LA64_DMW_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
const LA64_PHYS_ADDR_MASK: usize = (1usize << 48) - 1;
const LA64_DIRECT_MAP_SIZE: usize = LA64_PHYS_ADDR_MASK + 1;
const LA64_DMW_MAPPED_PHYS_BASE: usize = 0;
const LA64_CSR_CRMD: usize = 0x00;
const LA64_CSR_EUEN: usize = 0x02;
const LA64_CSR_ECFG: usize = 0x04;
const LA64_CSR_EENTRY: usize = 0x0c;
#[cfg(target_arch = "loongarch64")]
const LA64_CSR_KSAVE0: usize = 0x30;
const LA64_CSR_ASID: usize = 0x18;
const LA64_CSR_PGDL: usize = 0x19;
const LA64_CSR_PGDH: usize = 0x1a;
const LA64_CSR_PWCL: usize = 0x1c;
const LA64_CSR_PWCH: usize = 0x1d;
const LA64_CSR_STLBPS: usize = 0x1e;
const LA64_CSR_TLBRENTRY: usize = 0x88;
const LA64_CSR_TLBREHI: usize = 0x8e;
const LA64_CSR_MERRENTRY: usize = 0x93;
const LA64_CSR_TCFG: usize = 0x41;
const LA64_CSR_TICLR: usize = 0x44;
const LA64_CRMD_IE: usize = 1 << 2;
const LA64_CRMD_PG: usize = 1 << 4;
const LA64_CRMD_DATF_CC: usize = 0b01 << 5;
const LA64_CRMD_DATM_CC: usize = 0b01 << 7;
const LA64_EUEN_FPE: usize = 1 << 0;
const LA64_EUEN_SXE: usize = 1 << 1;
const LA64_EUEN_ASXE: usize = 1 << 2;
const LA64_ASID_MASK: usize = 0x3ff;
const LA64_TCFG_ENABLE: usize = 1 << 0;
const LA64_TCFG_TICK_MASK: usize = !0x3;
const LA64_TICLR_CLEAR_TIMER: usize = 1 << 0;
const LA64_CPUCFG2_LLFTP: u32 = 1 << 14;
const LA64_CPUCFG2: usize = 0x2;
const LA64_CPUCFG4: usize = 0x4;
const LA64_CPUCFG5: usize = 0x5;
const LA64_ESTAT_IS_HWI_MASK: usize = 0xff << 2;
const LA64_ESTAT_IS_HWI1: usize = 1 << 3;
const LA64_ESTAT_IS_TIMER: usize = 1 << 11;
const LA64_ESTAT_IS_IPI: usize = 1 << 12;
const LA64_ESTAT_ECODE_SHIFT: usize = 16;
const LA64_ESTAT_ECODE_MASK: usize = 0x3f;
const LA64_ECODE_INT: usize = 0;
const LA64_ECODE_PIL: usize = 1;
const LA64_ECODE_PIS: usize = 2;
const LA64_ECODE_PIF: usize = 3;
const LA64_ECODE_PME: usize = 4;
const LA64_ECODE_PNR: usize = 5;
const LA64_ECODE_PNX: usize = 6;
const LA64_ECODE_PPI: usize = 7;
const LA64_ESTAT_ESUBCODE_SHIFT: usize = 22;
const LA64_ESTAT_ESUBCODE_MASK: usize = 0x1ff;
const LA64_ECODE_ADE: usize = 8;
const LA64_ESUBCODE_ADEF: usize = 0;
const LA64_ECODE_ALE: usize = 9;
const LA64_ECODE_SYS: usize = 11;
const LA64_ECODE_BRK: usize = 12;
const LA64_ECODE_INE: usize = 13;
const LA64_ECODE_IPE: usize = 14;
const LA64_ECODE_FPD: usize = 15;
const LA64_ECODE_SXD: usize = 16;
const LA64_ECODE_ASXD: usize = 17;
const LA64_USER_TOP: usize = 0x0000_4000_0000_0000;
const LA64_PTE_PFN_MASK: u64 = ((1u64 << 48) - 1) & !((1u64 << 12) - 1);
const LA64_PTE_V: u64 = 1 << 0;
const LA64_PTE_A: u64 = 1 << 0;
const LA64_PTE_D: u64 = 1 << 1;
const LA64_PTE_PLV_USER: u64 = 0b11 << 2;
const LA64_PTE_MAT_SUC: u64 = 0b00 << 4;
const LA64_PTE_MAT_CC: u64 = 0b01 << 4;
const LA64_PTE_G: u64 = 1 << 6;
const LA64_PTE_PRESENT: u64 = 1 << 7;
const LA64_PTE_W: u64 = 1 << 8;
const LA64_PTE_M: u64 = 1 << 9;
const LA64_PTE_NR: u64 = 1 << 61;
const LA64_PTE_NX: u64 = 1 << 62;
#[cfg(test)]
const LA64_PTE_RPLV: u64 = 1 << 63;
const LA64_PRMD_PPLV_MASK: usize = 0x3;
const LA64_PRMD_PPLV_USER: usize = 0x3;
const LA64_PRMD_PIE: usize = 1 << 2;
const LA64_R_RA: usize = 1;
const LA64_R_TLS: usize = 2;
const LA64_R_SP: usize = 3;
const LA64_R_A0: usize = 4;
const LA64_R_A1: usize = 5;
const LA64_R_A2: usize = 6;
const LA64_R_A3: usize = 7;
const LA64_R_A4: usize = 8;
const LA64_R_A5: usize = 9;
const LA64_R_A7: usize = 11;
const LA64_SIGFRAME_ALIGN: usize = 16;
const LA64_SIGFRAME_MAGIC: u64 = 0x5458_5632_4c41_5331;
const LA64_SIGFRAME_VERSION: u32 = 1;
const LA64_RT_SIGRETURN_SYSCALL: u32 = 139;
const LA64_ADDI_D_R11_ZERO_RT_SIGRETURN: u32 = la64_addi_d(11, 0, LA64_RT_SIGRETURN_SYSCALL);
const LA64_SYSCALL_0: u32 = 0x002b_0000;
const LA64_SIGRETURN_TRAMPOLINE: [u32; 2] = [LA64_ADDI_D_R11_ZERO_RT_SIGRETURN, LA64_SYSCALL_0];

const UART_RBR: usize = 0;
const UART_THR: usize = 0;
const UART_IER: usize = 1;
const UART_FCR: usize = 2;
const UART_LCR: usize = 3;
const UART_MCR: usize = 4;
const UART_LSR: usize = 5;
const UART_LSR_DR: u8 = 1 << 0;
const UART_LSR_THRE: u8 = 1 << 5;
const UART_LSR_TEMT: u8 = 1 << 6;
const UART_FCR_ENABLE_FIFO: u8 = 1 << 0;
const UART_FCR_CLEAR_TX: u8 = 1 << 2;
const UART_LCR_DLAB: u8 = 1 << 7;
const UART_DIVISOR_125MHZ_115200: u16 = 68;
const UART_PORT_OWNER_WAIT_LIMIT: usize = 100_000;
const UART_TX_READY_WAIT_LIMIT: usize = 100_000;
const UART_TX_RECOVERY_WAIT_LIMIT: usize = 10_000;

const fn la64_addi_d(rd: u32, rj: u32, imm12: u32) -> u32 {
    0x02c0_0000 | ((imm12 & 0x0fff) << 10) | ((rj & 0x1f) << 5) | (rd & 0x1f)
}

static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
static LA64_TIMEBASE_HZ: AtomicU64 = AtomicU64::new(0);
static LA64_POSSIBLE_CPU_COUNT: AtomicUsize = AtomicUsize::new(LA64_DEFAULT_POSSIBLE_CPUS);
static LA64_ONLINE_CPUS: AtomicU64 = AtomicU64::new(1);
static LA64_IRQ_CONTEXT_DEPTHS: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_CPU_PIN_DEPTHS: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
const LA64_CPU_PIN_TRACE_CAPACITY: usize = 32;
// Keep the diagnostic ring out of `.bss` so it does not insert padding between
// the existing BSS neighbours of `LA64_CPU_PIN_DEPTHS`. The real-board witness
// must not accidentally hide a short adjacent overwrite by moving the counter.
#[link_section = ".data.tx_cpu_pin_trace"]
static LA64_CPU_PIN_TRACE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
#[link_section = ".data.tx_cpu_pin_trace"]
static LA64_CPU_PIN_TRACE_EVENTS: [AtomicU64; LA64_CPU_PIN_TRACE_CAPACITY] =
    [const { AtomicU64::new(0) }; LA64_CPU_PIN_TRACE_CAPACITY];
static LA64_IRQ_DISPATCH_TABLE: AtomicUsize = AtomicUsize::new(0);
static LA2K1000_UART_IRQ_OBSERVED: AtomicBool = AtomicBool::new(false);
static LA2K1000_UART_PORT_OWNED: AtomicBool = AtomicBool::new(false);
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
const LA64_PGDH_BOOTSTRAP_UNINIT: u8 = 0;
const LA64_PGDH_BOOTSTRAP_BUILDING: u8 = 1;
const LA64_PGDH_BOOTSTRAP_READY: u8 = 2;
/// One-time state for the shared high-half kernel mapping.
///
/// A boolean "done" flag is insufficient under SMP because two first users
/// can concurrently mutate the same PGDH tree. BUILDING gives waiters an
/// explicit lock-free progress state; a failed builder returns to UNINIT so a
/// later caller can complete the partially materialised, still-valid tree.
static LA64_KERNEL_PGDH_BOOTSTRAP_STATE: AtomicU8 = AtomicU8::new(LA64_PGDH_BOOTSTRAP_UNINIT);
/// Sequence protecting each hart's software view of its hardware
/// ASID/PGDL/PGDH transition. Even values are stable; odd values mean the hart
/// is between publication and completion of a hardware pmap switch.
static LA64_PMAP_SWITCH_SEQ: [AtomicU64; LA64_MAX_BOOT_CPUS] =
    [const { AtomicU64::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_ACTIVE_PGDL: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_ACTIVE_PGDH: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_ACTIVE_ASID: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
/// Address-space transition published before hardware starts using a new
/// PGDL.  Root teardown consults both this tuple and the active tuple so it
/// can distinguish a live in-progress switch from a stale residency bit.
static LA64_SWITCHING_PGDL: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_SWITCHING_ASID: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_ASID_RESIDENCY: [AtomicU64; LA64_ASID_CAPACITY] =
    [const { AtomicU64::new(0) }; LA64_ASID_CAPACITY];
/// Per-hart full-TLB shootdown mailbox.
///
/// LoongArch's board-level IPI is a maskable supervisor interrupt. Txv2 keeps
/// CRMD.IE clear while executing syscall/fault handlers, so a sender must not
/// rely exclusively on the interrupt trap to make progress. Request/completion
/// generations let the target service the same mailbox either from its IPI
/// handler or from an explicitly safe lock-contention point. Multiple senders
/// naturally coalesce because every LA64 request currently performs INVTLB-all.
static LA64_TLB_SHOOTDOWN_REQUESTED: [AtomicU64; LA64_MAX_BOOT_CPUS] =
    [const { AtomicU64::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_TLB_SHOOTDOWN_COMPLETED: [AtomicU64; LA64_MAX_BOOT_CPUS] =
    [const { AtomicU64::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_TLB_SHOOTDOWN_SERVICING: [AtomicBool; LA64_MAX_BOOT_CPUS] =
    [const { AtomicBool::new(false) }; LA64_MAX_BOOT_CPUS];
/// CPUs which still accept new synchronous shootdown acquisitions.
///
/// This differs from scheduler online state during the short AP shutdown
/// drain. A sender pins one target user across request publication and
/// completion; an offlining CPU clears this mask first and waits for its user
/// count to reach zero before disabling interrupts permanently.
static LA64_TLB_ACCEPTING_CPUS: AtomicU64 = AtomicU64::new(1);
static LA64_TLB_TARGET_USERS: [AtomicUsize; LA64_MAX_BOOT_CPUS] =
    [const { AtomicUsize::new(0) }; LA64_MAX_BOOT_CPUS];
static LA64_COMMITTED_PT_NODE_REGISTRY_LOCK: AtomicBool = AtomicBool::new(false);
/// Physical-address ownership registry for committed intermediate page-table
/// nodes. Keep the capacity and lookup shape aligned with RV64: BuildStorm can
/// keep thousands of address-space nodes live, and a linear 4096-entry scan
/// under one global lock becomes both a capacity limit and an SMP bottleneck.
const LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS: usize = 8192;
static LA64_COMMITTED_PT_NODES: La64CommittedPtNodeRegistry = La64CommittedPtNodeRegistry(
    UnsafeCell::new([La64CommittedPtNodeEntry::Empty; LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS]),
);
#[cfg(target_arch = "loongarch64")]
static LA64_KERNEL_TLS_VALID: [AtomicBool; LA64_MAX_BOOT_CPUS] =
    [const { AtomicBool::new(false) }; LA64_MAX_BOOT_CPUS];
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_KERNEL_TLS: AtomicUsize = AtomicUsize::new(0);
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
static LA64_KERNEL_RESUME_CTX: [PerHartCell<KernelResumeCtx>; LA64_MAX_BOOT_CPUS] = [const {
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    })
};
    LA64_MAX_BOOT_CPUS];

const LA64_TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
pub struct La64TrapStack(pub [u8; LA64_TRAP_STACK_SIZE]);

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static LA64_TRAP_STACKS: [PerHartCell<La64TrapStack>; LA64_MAX_BOOT_CPUS] =
    [const { PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])) }; LA64_MAX_BOOT_CPUS];

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

#[inline]
pub(crate) fn la64_current_stack_is_trap_stack() -> bool {
    #[cfg(target_arch = "loongarch64")]
    {
        let sp: usize;
        unsafe {
            core::arch::asm!(
                "move {sp}, $sp",
                sp = out(reg) sp,
                options(nomem, nostack)
            );
        }
        let cpu = <Platform as SmpIf>::current_cpu_id();
        let top = la64_trap_stack_top_for_cpu(cpu);
        return (top.saturating_sub(LA64_TRAP_STACK_SIZE)..top).contains(&sp);
    }

    #[cfg(not(target_arch = "loongarch64"))]
    false
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
static LA64_ENTRY_TRAP_FRAMES: [PerHartCell<La64TrapFrame>; LA64_MAX_BOOT_CPUS] =
    [const { PerHartCell::new(La64TrapFrame::empty()) }; LA64_MAX_BOOT_CPUS];

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
    UnsafeCell<[La64CommittedPtNodeEntry; LA64_COMMITTED_PT_NODE_REGISTRY_SLOTS]>,
);

unsafe impl Sync for La64CommittedPtNodeRegistry {}

#[derive(Clone, Copy)]
enum La64CommittedPtNodeEntry {
    Empty,
    Tombstone,
    Occupied(PtNode),
}

struct La64CommittedPtNodeRegistryGuard;

impl Drop for La64CommittedPtNodeRegistryGuard {
    fn drop(&mut self) {
        LA64_COMMITTED_PT_NODE_REGISTRY_LOCK.store(false, Ordering::Release);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct La64TrapFrame {
    pub r: [usize; 32],
    pub estat: usize,
    pub era: usize,
    pub badv: usize,
    pub crmd: usize,
    pub prmd: usize,
}

impl La64TrapFrame {
    pub const fn empty() -> Self {
        Self {
            r: [0; 32],
            estat: 0,
            era: 0,
            badv: 0,
            crmd: 0,
            prmd: 0,
        }
    }

    pub const fn snapshot(&self) -> TrapFrameSnapshot {
        // `TrapFrameSnapshot` still carries RV64-flavoured field names. For
        // LA64, `scause/sepc/stval` transport `ESTAT/ERA/BADV` respectively.
        TrapFrameSnapshot {
            scause: self.estat,
            sepc: self.era,
            stval: self.badv,
        }
    }

    pub const fn previous_mode(&self) -> TrapPreviousMode {
        match self.prmd & LA64_PRMD_PPLV_MASK {
            LA64_PRMD_PPLV_USER => TrapPreviousMode::User,
            0 => TrapPreviousMode::Supervisor,
            _ => TrapPreviousMode::Unknown,
        }
    }

    pub const fn fault_address(&self) -> Option<VirtAddr> {
        match classify_la64_trap(self.estat) {
            TrapClass::PageFault { .. } | TrapClass::AlignmentFault { .. } => {
                Some(VirtAddr(self.badv))
            }
            _ => None,
        }
    }

    pub const fn faulting_instruction(&self) -> Option<VirtAddr> {
        match classify_la64_trap(self.estat) {
            TrapClass::TimerInterrupt
            | TrapClass::ExternalInterrupt
            | TrapClass::InterprocessorInterrupt
            | TrapClass::UnknownInterrupt => None,
            _ => Some(VirtAddr(self.era)),
        }
    }

    pub const fn interrupts_enabled_before(&self) -> bool {
        self.prmd & LA64_PRMD_PIE != 0
    }

    pub const fn view(&self) -> TrapFrameView {
        TrapFrameView::new(
            VirtAddr(self.era),
            VirtAddr(self.r[LA64_R_SP]),
            self.r[LA64_R_A7] as u64,
            [
                self.r[LA64_R_A0] as u64,
                self.r[LA64_R_A1] as u64,
                self.r[LA64_R_A2] as u64,
                self.r[LA64_R_A3] as u64,
                self.r[LA64_R_A4] as u64,
                self.r[LA64_R_A5] as u64,
            ],
            self.fault_address(),
            self.faulting_instruction(),
            self.previous_mode(),
            self.interrupts_enabled_before(),
            self.r[LA64_R_TLS] as u64,
        )
    }

    pub fn view_mut(&mut self) -> TrapFrameMut<'_> {
        let view = TrapFrameView::new(
            VirtAddr(self.era),
            VirtAddr(self.r[LA64_R_SP]),
            self.r[LA64_R_A7] as u64,
            [
                self.r[LA64_R_A0] as u64,
                self.r[LA64_R_A1] as u64,
                self.r[LA64_R_A2] as u64,
                self.r[LA64_R_A3] as u64,
                self.r[LA64_R_A4] as u64,
                self.r[LA64_R_A5] as u64,
            ],
            self.fault_address(),
            self.faulting_instruction(),
            self.previous_mode(),
            self.interrupts_enabled_before(),
            self.r[LA64_R_TLS] as u64,
        );
        let raw = NonNull::from(&mut *self).cast::<()>();
        unsafe { TrapFrameMut::from_raw_parts(view, raw, &LA64_TRAP_FRAME_MUT_VTABLE) }
    }

    fn set_pc(&mut self, pc: VirtAddr) {
        self.era = pc.0;
    }

    fn set_sp(&mut self, sp: VirtAddr) {
        self.r[LA64_R_SP] = sp.0;
    }

    fn set_syscall_return(&mut self, value: i64) {
        self.r[LA64_R_A0] = value as usize;
    }

    fn set_syscall_error(&mut self, errno: i32) {
        self.r[LA64_R_A0] = (-(errno as isize)) as usize;
    }

    fn set_user_tls_register(&mut self, value: u64) {
        self.r[LA64_R_TLS] = value as usize;
    }

    fn capture_user_context(&self) -> UserTrapContext {
        UserTrapContext {
            regs: self.r,
            pc: self.era,
            status: self.prmd,
            fp: la64_capture_fp_context(),
        }
    }

    fn restore_user_context(&mut self, context: &UserTrapContext) {
        self.r = context.regs;
        self.r[0] = 0;
        self.era = context.pc;
        self.prmd = context.status;
        la64_restore_fp_context(&context.fp);
        self.prepare_user_return();
    }

    fn set_signal_handler_regs(&mut self, regs: SignalHandlerRegs) {
        self.r[LA64_R_RA] = regs.return_pc.0;
        self.r[LA64_R_A0] = regs.args[0];
        self.r[LA64_R_A1] = regs.args[1];
        self.r[LA64_R_A2] = regs.args[2];
    }

    fn rewind_pc(&mut self, bytes: usize) {
        self.era = self.era.saturating_sub(bytes);
    }

    pub fn prepare_user_return(&mut self) {
        self.prmd &= !LA64_PRMD_PPLV_MASK;
        self.prmd |= LA64_PRMD_PPLV_USER | LA64_PRMD_PIE;
    }
}

static LA64_TRAP_FRAME_MUT_VTABLE: TrapFrameMutVtable = TrapFrameMutVtable {
    read_view: la64_read_view,
    set_pc: la64_set_pc,
    set_sp: la64_set_sp,
    set_syscall_return: la64_set_syscall_return,
    set_syscall_error: la64_set_syscall_error,
    set_user_tls_register: la64_set_user_tls_register,
    capture_user_context: la64_capture_user_context,
    restore_user_context: la64_restore_user_context,
    set_signal_handler_regs: la64_set_signal_handler_regs,
    rewind_pc: la64_rewind_pc,
};

fn la64_frame_ptr(raw: NonNull<()>) -> *mut La64TrapFrame {
    raw.cast::<La64TrapFrame>().as_ptr()
}

fn la64_read_view(raw: NonNull<()>) -> TrapFrameView {
    unsafe { (*la64_frame_ptr(raw)).view() }
}

fn la64_set_pc(raw: NonNull<()>, pc: VirtAddr) {
    unsafe { (*la64_frame_ptr(raw)).set_pc(pc) };
}

fn la64_set_sp(raw: NonNull<()>, sp: VirtAddr) {
    unsafe { (*la64_frame_ptr(raw)).set_sp(sp) };
}

fn la64_set_syscall_return(raw: NonNull<()>, value: i64) {
    unsafe { (*la64_frame_ptr(raw)).set_syscall_return(value) };
}

fn la64_set_syscall_error(raw: NonNull<()>, errno: i32) {
    unsafe { (*la64_frame_ptr(raw)).set_syscall_error(errno) };
}

fn la64_set_user_tls_register(raw: NonNull<()>, value: u64) {
    unsafe { (*la64_frame_ptr(raw)).set_user_tls_register(value) };
}

fn la64_capture_user_context(raw: NonNull<()>) -> UserTrapContext {
    unsafe { (*la64_frame_ptr(raw)).capture_user_context() }
}

fn la64_restore_user_context(raw: NonNull<()>, context: &UserTrapContext) {
    unsafe { (*la64_frame_ptr(raw)).restore_user_context(context) };
}

fn la64_set_signal_handler_regs(raw: NonNull<()>, regs: SignalHandlerRegs) {
    unsafe { (*la64_frame_ptr(raw)).set_signal_handler_regs(regs) };
}

fn la64_rewind_pc(raw: NonNull<()>, bytes: usize) {
    unsafe { (*la64_frame_ptr(raw)).rewind_pc(bytes) };
}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_fp_save_context(ctx: *mut UserFpContext) -> usize;
    fn tx_la64_fp_restore_context(ctx: *const UserFpContext);
}

#[cfg(target_arch = "loongarch64")]
fn la64_capture_fp_context() -> UserFpContext {
    let mut fp = <Platform as FpSimdIf>::init_state();
    <Platform as FpSimdIf>::save(&mut fp);
    fp
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_capture_fp_context() -> UserFpContext {
    let mut fp = <Platform as FpSimdIf>::init_state();
    <Platform as FpSimdIf>::save(&mut fp);
    fp
}

#[cfg(target_arch = "loongarch64")]
fn la64_restore_fp_context(fp: &UserFpContext) {
    <Platform as FpSimdIf>::restore(fp);
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_restore_fp_context(fp: &UserFpContext) {
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
    let saved = unsafe { tx_la64_fp_save_context(core::ptr::addr_of_mut!(*state)) };
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
    unsafe { tx_la64_fp_restore_context(core::ptr::addr_of!(*state)) };
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

struct UartPortOwner;

impl Drop for UartPortOwner {
    fn drop(&mut self) {
        LA2K1000_UART_PORT_OWNED.store(false, Ordering::Release);
    }
}

fn try_acquire_uart_port_owner() -> Option<UartPortOwner> {
    LA2K1000_UART_PORT_OWNED
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .ok()
        .map(|_| UartPortOwner)
}

fn acquire_uart_port_owner() -> Option<(LocalExecutionGuard, UartPortOwner)> {
    let mut wait = tx_hal::TlbProgressSpinWait::new();
    for _ in 0..UART_PORT_OWNER_WAIT_LIMIT {
        let local_execution = <Platform as IrqIf>::exclude_local_execution();
        if let Some(owner) = try_acquire_uart_port_owner() {
            return Some((local_execution, owner));
        }
        drop(local_execution);
        wait.spin_with(|| {
            la64_pmap::service_la64_pending_tlb_shootdown();
        });
    }
    None
}

fn wait_for_uart_tx_ready_with<R, P>(limit: usize, mut read_lsr: R, mut progress: P) -> bool
where
    R: FnMut() -> u8,
    P: FnMut(),
{
    let mut wait = tx_hal::TlbProgressSpinWait::new();
    for _ in 0..limit {
        if read_lsr() & UART_LSR_THRE != 0 {
            return true;
        }
        wait.spin_with(&mut progress);
    }
    false
}

fn uart_put_byte(byte: u8) {
    let Some((local_execution, owner)) = acquire_uart_port_owner() else {
        return;
    };
    let base = la64_uncached_virt(LA2K1000_UART_BASE) as *mut u8;
    unsafe {
        let mut ready = wait_for_uart_tx_ready_with(
            UART_TX_READY_WAIT_LIMIT,
            || core::ptr::read_volatile(base.add(UART_LSR)),
            || {
                la64_pmap::service_la64_pending_tlb_shootdown();
            },
        );
        if !ready {
            core::ptr::write_volatile(base.add(UART_FCR), UART_FCR_ENABLE_FIFO | UART_FCR_CLEAR_TX);
            la64_irq_trap::la64_dbar();
            ready = wait_for_uart_tx_ready_with(
                UART_TX_RECOVERY_WAIT_LIMIT,
                || core::ptr::read_volatile(base.add(UART_LSR)),
                || {
                    la64_pmap::service_la64_pending_tlb_shootdown();
                },
            );
        }
        if ready {
            core::ptr::write_volatile(base.add(UART_THR), byte);
            la64_irq_trap::la64_dbar();
        }
    }
    // A pending UART IRQ may run as soon as local execution is restored. Make
    // the port available to that handler before opening the interrupt window.
    drop(owner);
    drop(local_execution);
}

fn uart_try_get_byte() -> Option<u8> {
    #[cfg(target_arch = "loongarch64")]
    {
        let (local_execution, owner) = acquire_uart_port_owner()?;
        let base = la64_uncached_virt(LA2K1000_UART_BASE) as *const u8;
        let byte = unsafe {
            let lsr = core::ptr::read_volatile(base.add(UART_LSR));
            (lsr & UART_LSR_DR != 0).then(|| core::ptr::read_volatile(base.add(UART_RBR)))
        };
        drop(owner);
        drop(local_execution);
        byte
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        None
    }
}

fn reinit_uart_115200() {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let base = la64_uncached_virt(LA2K1000_UART_BASE) as *mut u8;
        while core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_TEMT == 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(base.add(UART_IER), 0);
        core::ptr::write_volatile(base.add(UART_LCR), UART_LCR_DLAB);
        core::ptr::write_volatile(base.add(UART_THR), UART_DIVISOR_125MHZ_115200 as u8);
        core::ptr::write_volatile(base.add(UART_IER), (UART_DIVISOR_125MHZ_115200 >> 8) as u8);
        core::ptr::write_volatile(base.add(UART_LCR), 0x03);
        core::ptr::write_volatile(base.add(UART_FCR), 0x07);
        core::ptr::write_volatile(base.add(UART_MCR), 0x03);
    }
}

pub fn early_console_write(bytes: &[u8]) {
    for &byte in bytes {
        if byte == b'\n' {
            uart_put_byte(b'\r');
        }
        uart_put_byte(byte);
    }
}

pub fn initialize_early_board() {
    early_console_write(b"txkernel:loongson-2k1000:h2:uart-inherited:ok\n");
    reinit_uart_115200();
    early_console_write(b"txkernel:loongson-2k1000:h2:uart-reinit:ok\n");
}

static MMIO_REGIONS: [MmioRegion; 4] = [
    MmioRegion {
        name: "uart0",
        phys: PhysRange {
            start: PhysAddr(LA2K1000_UART_BASE),
            size: LA2K1000_UART_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(LA64_DMW_UNCACHED_BASE | LA2K1000_UART_BASE),
            size: LA2K1000_UART_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "liointc",
        phys: PhysRange {
            start: PhysAddr(la2k1000_liointc::MAIN_PHYS_BASE),
            size: la2k1000_liointc::MAIN_MMIO_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(LA64_DMW_UNCACHED_BASE | la2k1000_liointc::MAIN_PHYS_BASE),
            size: la2k1000_liointc::MAIN_MMIO_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "ahci0",
        phys: PhysRange {
            start: PhysAddr(LA2K1000_AHCI_BASE),
            size: LA2K1000_AHCI_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(LA64_DMW_UNCACHED_BASE | LA2K1000_AHCI_BASE),
            size: LA2K1000_AHCI_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "gmac1",
        phys: PhysRange {
            start: PhysAddr(LA2K1000_GMAC1_BASE),
            size: LA2K1000_GMAC1_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(LA64_DMW_UNCACHED_BASE | LA2K1000_GMAC1_BASE),
            size: LA2K1000_GMAC1_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
];

const LA2K1000_SOC_PROVIDER: ResourceProviderId = ResourceProviderId("loongson-2k1000-soc");
const LA2K1000_AHCI_ORIGIN: ResourceOrigin = ResourceOrigin {
    provider: LA2K1000_SOC_PROVIDER,
    record: "/2k1000-soc/ahci@400e0000",
    kind: ResourceOriginKind::PlatformStatic,
};
const LA2K1000_GMAC1_ORIGIN: ResourceOrigin = ResourceOrigin {
    provider: LA2K1000_SOC_PROVIDER,
    record: "/2k1000-soc/ethernet@40050000",
    kind: ResourceOriginKind::PlatformStatic,
};
const LA2K1000_GMAC1_PHY_ORIGIN: ResourceOrigin = ResourceOrigin {
    provider: LA2K1000_SOC_PROVIDER,
    record: "/2k1000-soc/ethernet@40050000/mdio/ethernet-phy@0",
    kind: ResourceOriginKind::PlatformStatic,
};
const LA2K1000_AHCI_DMA_DOMAIN_ID: DmaDomainId = DmaDomainId {
    provider: LA2K1000_SOC_PROVIDER,
    local: 0,
};
const LA2K1000_GMAC1_DMA_DOMAIN_ID: DmaDomainId = DmaDomainId {
    provider: LA2K1000_SOC_PROVIDER,
    local: 1,
};
static LA2K1000_DMA_DOMAINS: [DmaDomain; 2] = [
    DmaDomain {
        id: LA2K1000_AHCI_DMA_DOMAIN_ID,
        translation: DmaTranslation::Direct { offset: 0 },
        constraints: DmaConstraints {
            dma_address_bits: 32,
            min_alignment: 1,
            segment_boundary: None,
            max_segment_len: usize::MAX,
            max_segments: u16::MAX,
        },
        coherency: DmaCoherency::Coherent,
        origin: LA2K1000_AHCI_ORIGIN,
    },
    DmaDomain {
        id: LA2K1000_GMAC1_DMA_DOMAIN_ID,
        translation: DmaTranslation::Direct { offset: 0 },
        constraints: DmaConstraints {
            dma_address_bits: 32,
            min_alignment: 1,
            segment_boundary: None,
            max_segment_len: usize::MAX,
            max_segments: u16::MAX,
        },
        coherency: DmaCoherency::NonCoherent,
        origin: LA2K1000_GMAC1_ORIGIN,
    },
];
static LA2K1000_AHCI_MATCHES: [DeviceMatchId; 1] =
    [DeviceMatchId::FirmwareCompatible("loongson,ls-ahci")];
static LA2K1000_AHCI_RESOURCES: [DeviceResource; 3] = [
    DeviceResource::Mmio(MmioResource {
        role: ResourceRole::Index(0),
        phys: PhysRange {
            start: PhysAddr(LA2K1000_AHCI_BASE),
            size: LA2K1000_AHCI_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(LA64_DMW_UNCACHED_BASE | LA2K1000_AHCI_BASE),
            size: LA2K1000_AHCI_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
        origin: LA2K1000_AHCI_ORIGIN,
    }),
    DeviceResource::Irq(IrqResource {
        role: ResourceRole::Index(0),
        line: la2k1000_liointc::AHCI_PUBLIC_IRQ,
        trigger: IrqTrigger::Level,
        polarity: IrqPolarity::High,
        sharing: IrqSharing::Exclusive,
        origin: LA2K1000_AHCI_ORIGIN,
    }),
    DeviceResource::DmaDomain(DmaDomainRef {
        role: ResourceRole::Index(0),
        domain: LA2K1000_AHCI_DMA_DOMAIN_ID,
    }),
];
static LA2K1000_GMAC1_MATCHES: [DeviceMatchId; 2] = [
    DeviceMatchId::FirmwareCompatible("snps,dwmac-3.70a"),
    DeviceMatchId::FirmwareCompatible("snps,arc-dwmac-3.70a"),
];
const LA2K1000_GMAC1_PHY_ID: DeviceId = DeviceId {
    provider: LA2K1000_SOC_PROVIDER,
    local: DeviceLocalId::FirmwarePath("/2k1000-soc/ethernet@40050000/mdio/ethernet-phy@0"),
};
static LA2K1000_GMAC1_RESOURCES: [DeviceResource; 5] = [
    DeviceResource::Mmio(MmioResource {
        role: ResourceRole::Index(0),
        phys: PhysRange {
            start: PhysAddr(LA2K1000_GMAC1_BASE),
            size: LA2K1000_GMAC1_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(LA64_DMW_UNCACHED_BASE | LA2K1000_GMAC1_BASE),
            size: LA2K1000_GMAC1_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
        origin: LA2K1000_GMAC1_ORIGIN,
    }),
    DeviceResource::Irq(IrqResource {
        role: ResourceRole::Index(0),
        line: la2k1000_liointc::GMAC1_PUBLIC_IRQ,
        trigger: IrqTrigger::Level,
        polarity: IrqPolarity::High,
        sharing: IrqSharing::Exclusive,
        origin: LA2K1000_GMAC1_ORIGIN,
    }),
    DeviceResource::DmaDomain(DmaDomainRef {
        role: ResourceRole::Index(0),
        domain: LA2K1000_GMAC1_DMA_DOMAIN_ID,
    }),
    DeviceResource::Phy(PhyRef {
        role: ResourceRole::Index(0),
        provider: LA2K1000_GMAC1_PHY_ID,
    }),
    DeviceResource::MacAddress {
        role: ResourceRole::Index(0),
        source: MacAddressSource::Firmware(LA2K1000_GMAC1_MAC_ADDRESS),
    },
];
static LA2K1000_GMAC1_PHY_MATCHES: [DeviceMatchId; 1] = [DeviceMatchId::FirmwareCompatible(
    "ethernet-phy-ieee802.3-c22",
)];
static LA2K1000_PLATFORM_DEVICES: [tx_hal::PlatformDevice; 3] = [
    tx_hal::PlatformDevice {
        id: DeviceId {
            provider: LA2K1000_SOC_PROVIDER,
            local: DeviceLocalId::FirmwarePath("/2k1000-soc/ahci@400e0000"),
        },
        status: DeviceStatus::Enabled,
        matches: &LA2K1000_AHCI_MATCHES,
        resources: &LA2K1000_AHCI_RESOURCES,
        origin: LA2K1000_AHCI_ORIGIN,
    },
    tx_hal::PlatformDevice {
        id: DeviceId {
            provider: LA2K1000_SOC_PROVIDER,
            local: DeviceLocalId::FirmwarePath("/2k1000-soc/ethernet@40050000"),
        },
        status: DeviceStatus::Enabled,
        matches: &LA2K1000_GMAC1_MATCHES,
        resources: &LA2K1000_GMAC1_RESOURCES,
        origin: LA2K1000_GMAC1_ORIGIN,
    },
    tx_hal::PlatformDevice {
        id: LA2K1000_GMAC1_PHY_ID,
        status: DeviceStatus::Enabled,
        matches: &LA2K1000_GMAC1_PHY_MATCHES,
        resources: &[],
        origin: LA2K1000_GMAC1_PHY_ORIGIN,
    },
];
static LA2K1000_DEVICE_RESOURCE_GRAPH: DeviceResourceGraph = DeviceResourceGraph {
    platform_mmio: &[],
    devices: &LA2K1000_PLATFORM_DEVICES,
    dma_domains: &LA2K1000_DMA_DOMAINS,
};

trait Gmac1PinmuxIo {
    fn read(&self) -> u32;
    fn write(&self, value: u32);
    fn barrier(&self);
}

struct Gmac1PinmuxRegister;

#[cfg(not(target_arch = "loongarch64"))]
static HOST_GMAC1_PINMUX_REGISTER: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

impl Gmac1PinmuxIo for Gmac1PinmuxRegister {
    fn read(&self) -> u32 {
        #[cfg(target_arch = "loongarch64")]
        {
            unsafe {
                core::ptr::read_volatile(la64_uncached_virt(LA2K1000_GMAC_PINMUX_PHYS) as *const u32)
            }
        }
        #[cfg(not(target_arch = "loongarch64"))]
        {
            HOST_GMAC1_PINMUX_REGISTER.load(Ordering::Acquire)
        }
    }

    fn write(&self, value: u32) {
        #[cfg(target_arch = "loongarch64")]
        unsafe {
            core::ptr::write_volatile(
                la64_uncached_virt(LA2K1000_GMAC_PINMUX_PHYS) as *mut u32,
                value,
            );
        }
        #[cfg(not(target_arch = "loongarch64"))]
        HOST_GMAC1_PINMUX_REGISTER.store(value, Ordering::Release);
    }

    fn barrier(&self) {
        #[cfg(target_arch = "loongarch64")]
        la64_irq_trap::la64_dbar();
        #[cfg(not(target_arch = "loongarch64"))]
        core::sync::atomic::fence(Ordering::SeqCst);
    }
}

fn prepare_gmac1_pinmux_with(io: &impl Gmac1PinmuxIo) -> bool {
    let before = io.read();
    let prepared = before | LA2K1000_GMAC1_PINMUX_BIT;
    if prepared != before {
        io.write(prepared);
        io.barrier();
    }
    io.read() & LA2K1000_GMAC1_PINMUX_BIT != 0
}

fn prepare_gmac1_pinmux() -> bool {
    prepare_gmac1_pinmux_with(&Gmac1PinmuxRegister)
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct La64SignalFrame {
    magic: u64,
    version: u32,
    frame_size: u32,
    sig_no: u32,
    _reserved0: u32,
    flags: u64,
    siginfo: tx_hal::UserSigInfoAbi,
    saved_mask: UserSignalMaskAbi,
    // Includes full GPR + PC + status and UserFpContext payload.
    user_context: UserTrapContext,
    trampoline: [u32; 2],
}

unsafe impl Pod for La64SignalFrame {}

impl La64SignalFrame {
    fn new(tf: &TrapFrameMut<'_>, setup: &SignalFrameWrite) -> Self {
        Self::new_from_context(&tf.capture_user_context(), setup)
    }

    fn new_from_context(context: &UserTrapContext, setup: &SignalFrameWrite) -> Self {
        Self {
            magic: LA64_SIGFRAME_MAGIC,
            version: LA64_SIGFRAME_VERSION,
            frame_size: core::mem::size_of::<Self>() as u32,
            sig_no: setup.sig_no,
            _reserved0: 0,
            flags: setup.flags.bits,
            siginfo: setup.siginfo,
            saved_mask: setup.old_mask,
            user_context: *context,
            trampoline: LA64_SIGRETURN_TRAMPOLINE,
        }
    }

    fn validate(&self, user_sp: UserPtr<u8>) -> Result<(), FaultInfo> {
        if self.magic == LA64_SIGFRAME_MAGIC
            && self.version == LA64_SIGFRAME_VERSION
            && self.frame_size as usize == core::mem::size_of::<Self>()
            && self.trampoline == LA64_SIGRETURN_TRAMPOLINE
        {
            Ok(())
        } else {
            Err(FaultInfo {
                address: VirtAddr(user_sp.addr()),
                write: false,
                instruction: false,
                from_user: false,
            })
        }
    }
}

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::LoongArch64;
    const BOARD: &'static str = "loongson-2k1000";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = 48;
    const VIRT_ADDR_BITS: u8 = 48;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(LA64_DMW_CACHED_BASE);
    const DIRECT_MAP_SIZE: usize = LA64_DIRECT_MAP_SIZE;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(LA64_DMW_CACHED_BASE | LA2K1000_KERNEL_LOAD_BASE);
    const USER_TOP: VirtAddr = VirtAddr(LA64_USER_TOP);
    const KERNEL_STACK_SIZE: usize = 512 * 1024;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 4;
    const ASID_BITS: u8 = 10;
    const CACHE_LINE_SIZE: usize = 64;
    const DMA_COHERENT: bool = false;
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::LoongArchFirmware;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        ensure_static_boot_facts();
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

    fn prepare_platform_device(
        device: &'static tx_hal::PlatformDevice,
    ) -> Result<(), tx_hal::PlatformDevicePrepareError> {
        let is_ahci = device.matches.iter().any(|candidate| {
            matches!(candidate, DeviceMatchId::FirmwareCompatible(value)
                if *value == "loongson,ls-ahci" || *value == "snps,spear-ahci")
        });
        let is_gmac1 = device.matches.iter().any(|candidate| {
            matches!(candidate, DeviceMatchId::FirmwareCompatible(value)
                if *value == "snps,dwmac-3.70a" || *value == "snps,arc-dwmac-3.70a")
        });
        if !is_ahci && !is_gmac1 {
            return Ok(());
        }

        let irq = device.resources.iter().find_map(|resource| match resource {
            DeviceResource::Irq(irq) if irq.role == ResourceRole::Index(0) => Some(*irq),
            _ => None,
        });
        let Some(irq) = irq else {
            return Err(tx_hal::PlatformDevicePrepareError::MalformedFirmwareProperty);
        };
        let expected_irq = if is_gmac1 {
            la2k1000_liointc::GMAC1_PUBLIC_IRQ
        } else {
            la2k1000_liointc::AHCI_PUBLIC_IRQ
        };
        if irq.line != expected_irq
            || irq.trigger != IrqTrigger::Level
            || irq.polarity != IrqPolarity::High
            || irq.sharing != IrqSharing::Exclusive
        {
            return Err(tx_hal::PlatformDevicePrepareError::MalformedFirmwareProperty);
        }
        if is_gmac1 && !prepare_gmac1_pinmux() {
            return Err(tx_hal::PlatformDevicePrepareError::MalformedFirmwareProperty);
        }
        // U-Boot has already established the live PHY/RGMII state. Platform
        // preparation only confirms muxing and IRQ routing; it invents no
        // undocumented clock/reset writes or PHY reinitialization sequence.
        if !la2k1000_liointc::prepare_level_high_source(irq.line) {
            return Err(tx_hal::PlatformDevicePrepareError::MalformedFirmwareProperty);
        }
        Ok(())
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
        // UART0 and its LIOINTC route are owned by CPU0. AP reactor polling
        // must not race the routed top half for the same physical FIFO.
        if <Self as SmpIf>::current_cpu_id().0 != 0 {
            return 0;
        }
        let mut count = 0;
        for slot in buf {
            let Some(byte) = uart_try_get_byte() else {
                break;
            };
            *slot = byte;
            count += 1;
        }
        count
    }
}

impl ObserverIf for Platform {}

pub use boot_args::capture_loongarch64_2k1000_boot_args;

mod boot_args;
mod boot_asm;
mod boot_facts;
mod boot_smp;
mod la2k1000_liointc;
mod la64_dma_cache;
#[path = "../../tx-hal-loongarch64-common/src/la64_ipi.rs"]
mod la64_ipi;
#[path = "../../tx-hal-loongarch64-common/src/la64_irq_trap.rs"]
mod la64_irq_trap;
#[path = "../../tx-hal-loongarch64-common/src/la64_percpu.rs"]
mod la64_percpu;
#[path = "../../tx-hal-loongarch64-common/src/la64_pmap.rs"]
mod la64_pmap;
#[path = "../../tx-hal-loongarch64-common/src/la64_unaligned.rs"]
mod la64_unaligned;
mod platform_impls;
#[path = "../../tx-hal-loongarch64-common/src/trap_asm.rs"]
mod trap_asm;

#[cfg(test)]
mod uart_tx_tests {
    use super::*;

    #[test]
    fn uart_port_owner_is_exclusive_and_released_by_guard() {
        LA2K1000_UART_PORT_OWNED.store(false, Ordering::Release);
        let owner = try_acquire_uart_port_owner().expect("first UART port owner");
        assert!(try_acquire_uart_port_owner().is_none());
        drop(owner);
        let owner = try_acquire_uart_port_owner().expect("UART port owner after release");
        drop(owner);
    }

    #[test]
    fn uart_tx_ready_wait_stops_at_the_supplied_limit() {
        let mut reads = 0usize;
        let mut progress = 0usize;
        assert!(!wait_for_uart_tx_ready_with(
            5,
            || {
                reads += 1;
                0
            },
            || progress += 1,
        ));
        assert_eq!(reads, 5);
        assert_eq!(progress, 1);
    }

    #[test]
    fn uart_tx_ready_wait_returns_on_thre() {
        let mut reads = 0usize;
        assert!(wait_for_uart_tx_ready_with(
            5,
            || {
                reads += 1;
                if reads == 3 {
                    UART_LSR_THRE
                } else {
                    0
                }
            },
            || {},
        ));
        assert_eq!(reads, 3);
    }
}

#[cfg(test)]
mod device_resource_tests {
    use super::*;

    fn frozen_graph() -> &'static DeviceResourceGraph {
        tx_hal::DeviceGraphBuilder::from_seed(&LA2K1000_DEVICE_RESOURCE_GRAPH)
            .expect("2K1000 resource seed is valid")
            .freeze()
            .expect("2K1000 resource graph freezes")
    }

    #[test]
    fn ahci_resource_graph_matches_live_board_facts() {
        let graph = frozen_graph();
        assert_eq!(graph.devices.len(), 3);
        assert_eq!(graph.dma_domains.len(), 2);

        let ahci = graph
            .devices
            .iter()
            .find(|device| {
                device.matches.iter().any(|candidate| {
                    matches!(
                        candidate,
                        DeviceMatchId::FirmwareCompatible("loongson,ls-ahci")
                    )
                })
            })
            .expect("AHCI platform device");
        assert_eq!(ahci.status, DeviceStatus::Enabled);
        assert!(ahci.matches.iter().any(|candidate| matches!(
            candidate,
            DeviceMatchId::FirmwareCompatible("loongson,ls-ahci")
        )));

        let mmio = ahci.resources.iter().find_map(|resource| match resource {
            DeviceResource::Mmio(mmio) => Some(*mmio),
            _ => None,
        });
        let irq = ahci.resources.iter().find_map(|resource| match resource {
            DeviceResource::Irq(irq) => Some(*irq),
            _ => None,
        });
        let dma = ahci.resources.iter().find_map(|resource| match resource {
            DeviceResource::DmaDomain(dma) => Some(*dma),
            _ => None,
        });

        let mmio = mmio.expect("AHCI MMIO resource");
        assert_eq!(mmio.phys.start, PhysAddr(LA2K1000_AHCI_BASE));
        assert_eq!(mmio.phys.size, LA2K1000_AHCI_SIZE);
        assert_eq!(
            mmio.virt.start,
            VirtAddr(LA64_DMW_UNCACHED_BASE | LA2K1000_AHCI_BASE)
        );

        let irq = irq.expect("AHCI IRQ resource");
        assert_eq!(irq.line, la2k1000_liointc::AHCI_PUBLIC_IRQ);
        assert_eq!(irq.trigger, IrqTrigger::Level);
        assert_eq!(irq.polarity, IrqPolarity::High);
        assert_eq!(irq.sharing, IrqSharing::Exclusive);

        let dma = dma.expect("AHCI DMA domain reference");
        assert_eq!(dma.domain, LA2K1000_AHCI_DMA_DOMAIN_ID);
        let domain = graph
            .dma_domains
            .iter()
            .find(|domain| domain.id == dma.domain)
            .expect("AHCI DMA domain");
        assert_eq!(domain.translation, DmaTranslation::Direct { offset: 0 });
        assert_eq!(domain.constraints.dma_address_bits, 32);
        assert_eq!(domain.coherency, DmaCoherency::Coherent);
        assert!(!<Platform as PlatformConfig>::DMA_COHERENT);
        assert!(!<Platform as DmaIf>::DMA_COHERENT);
    }

    #[test]
    fn gmac1_resource_graph_matches_live_board_facts() {
        let graph = frozen_graph();
        let gmac = graph
            .devices
            .iter()
            .find(|device| {
                device.matches.iter().any(|candidate| {
                    matches!(
                        candidate,
                        DeviceMatchId::FirmwareCompatible("snps,dwmac-3.70a")
                    )
                })
            })
            .expect("GMAC1 platform device");
        assert_eq!(gmac.status, DeviceStatus::Enabled);
        assert!(gmac.matches.iter().any(|candidate| matches!(
            candidate,
            DeviceMatchId::FirmwareCompatible("snps,arc-dwmac-3.70a")
        )));

        let mmio = gmac.resources.iter().find_map(|resource| match resource {
            DeviceResource::Mmio(mmio) => Some(*mmio),
            _ => None,
        });
        let irq_resources = gmac
            .resources
            .iter()
            .filter_map(|resource| match resource {
                DeviceResource::Irq(irq) => Some(*irq),
                _ => None,
            })
            .collect::<std::vec::Vec<_>>();
        let dma = gmac.resources.iter().find_map(|resource| match resource {
            DeviceResource::DmaDomain(dma) => Some(*dma),
            _ => None,
        });
        let phy = gmac.resources.iter().find_map(|resource| match resource {
            DeviceResource::Phy(phy) => Some(*phy),
            _ => None,
        });
        let mac = gmac.resources.iter().find_map(|resource| match resource {
            DeviceResource::MacAddress { source, .. } => Some(*source),
            _ => None,
        });

        let mmio = mmio.expect("GMAC1 MMIO resource");
        assert_eq!(mmio.phys.start, PhysAddr(LA2K1000_GMAC1_BASE));
        assert_eq!(mmio.phys.size, LA2K1000_GMAC1_SIZE);
        assert_eq!(
            mmio.virt.start,
            VirtAddr(LA64_DMW_UNCACHED_BASE | LA2K1000_GMAC1_BASE)
        );

        assert_eq!(irq_resources.len(), 1, "wake IRQ remains unbound");
        let irq = irq_resources[0];
        assert_eq!(irq.line, la2k1000_liointc::GMAC1_PUBLIC_IRQ);
        assert_eq!(irq.trigger, IrqTrigger::Level);
        assert_eq!(irq.polarity, IrqPolarity::High);
        assert_eq!(irq.sharing, IrqSharing::Exclusive);

        let dma = dma.expect("GMAC1 DMA domain reference");
        assert_eq!(dma.domain, LA2K1000_GMAC1_DMA_DOMAIN_ID);
        let domain = graph
            .dma_domains
            .iter()
            .find(|domain| domain.id == dma.domain)
            .expect("GMAC1 DMA domain");
        assert_eq!(domain.translation, DmaTranslation::Direct { offset: 0 });
        assert_eq!(domain.constraints.dma_address_bits, 32);
        assert_eq!(domain.coherency, DmaCoherency::NonCoherent);

        let phy = phy.expect("GMAC1 PHY reference");
        assert_eq!(phy.provider, LA2K1000_GMAC1_PHY_ID);
        assert_eq!(
            phy.provider.local,
            DeviceLocalId::FirmwarePath("/2k1000-soc/ethernet@40050000/mdio/ethernet-phy@0")
        );
        assert_eq!(LA2K1000_GMAC1_PHY_ADDRESS, 0);
        assert_eq!(LA2K1000_GMAC1_PHY_MODE, "rgmii-id");
        assert_eq!(
            mac,
            Some(MacAddressSource::Firmware([
                0x0e, 0x40, 0xdf, 0xbc, 0x58, 0x4e
            ]))
        );
        assert!(!gmac.resources.iter().any(|resource| matches!(
            resource,
            DeviceResource::Clock(_) | DeviceResource::Reset(_)
        )));
    }

    struct TestPinmuxIo {
        value: core::cell::Cell<u32>,
        writes: core::cell::Cell<usize>,
        barriers: core::cell::Cell<usize>,
    }

    impl Gmac1PinmuxIo for TestPinmuxIo {
        fn read(&self) -> u32 {
            self.value.get()
        }

        fn write(&self, value: u32) {
            self.value.set(value);
            self.writes.set(self.writes.get() + 1);
        }

        fn barrier(&self) {
            self.barriers.set(self.barriers.get() + 1);
        }
    }

    #[test]
    fn gmac1_pinmux_prepare_changes_only_bit3_and_is_idempotent() {
        let before = 0xa5a5_00f0 & !LA2K1000_GMAC1_PINMUX_BIT;
        let io = TestPinmuxIo {
            value: core::cell::Cell::new(before),
            writes: core::cell::Cell::new(0),
            barriers: core::cell::Cell::new(0),
        };

        assert!(prepare_gmac1_pinmux_with(&io));
        assert_eq!(io.value.get(), before | LA2K1000_GMAC1_PINMUX_BIT);
        assert_eq!(io.value.get() ^ before, LA2K1000_GMAC1_PINMUX_BIT);
        assert_eq!(io.writes.get(), 1);
        assert_eq!(io.barriers.get(), 1);

        assert!(prepare_gmac1_pinmux_with(&io));
        assert_eq!(io.value.get(), before | LA2K1000_GMAC1_PINMUX_BIT);
        assert_eq!(io.writes.get(), 1);
        assert_eq!(io.barriers.get(), 1);
    }
}
