#![no_std]

#[cfg(test)]
extern crate std;

use tx_hal::{
    AllocError, Arch, ArchAuxvFacts, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf,
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask,
    CpuPinGuard, DeadlineTimerIf, DmaIf, EntropyIf, FaultInfo, FpSimdIf, InitIf,
    InterruptWaitState, IpiKind, IrqDispatchTable, IrqHandled, IrqIf, KernelTrapSink,
    LocalExecutionGuard, MemoryRegion, MemoryRegionKind, MmioFlags, MmioRegion, MonotonicCounterIf,
    ObserverIf, PercpuIf, PersistentClockError, PersistentClockIf, PhysAddr, PhysRange,
    PlatformConfig, PlatformInfo, PlatformInfoIf, PmapError, PmapIf, PmapInvalidation,
    PmapPermissions, PmapReservation, PmapReservationIntermediates, PmapReserveKind, PmapRoot,
    PmapUnmapResult, Pod, PowerIf, PtNode, PtNodeAllocator, SavedSignalFrame, SecondaryEntry,
    SignalFrameIf, SignalFrameWrite, SignalHandlerRegs, SmpIf, TrapAction, TrapClass, TrapFrameMut,
    TrapFrameMutVtable, TrapFrameSnapshot, TrapFrameView, TrapIf, TrapPreviousMode, UserFpContext,
    UserPtr, UserSignalMaskAbi, UserTrapContext, VirtAddr, VirtRange,
};

pub use boot_args::capture_loongarch64_qemu_boot_args;
use boot_facts::ensure_static_boot_facts;
use la64_irq_trap::classify_la64_trap;
pub use la64_irq_trap::{dispatch_trap_frame, return_to_userspace};
pub(crate) use la64_percpu::la64_current_cpu_id;
#[cfg(target_arch = "loongarch64")]
use la64_pmap::la64_kernel_addr_to_phys;
use la64_pmap::{la64_cached_virt, la64_uncached_virt, uart_put_byte, uart_try_get_byte};

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

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

const QEMU_LA64_RAM_BASE: usize = 0;
// Low-RAM window only. On QEMU `virt`, [0, 256 MiB) is low RAM; the rest of the
// guest RAM (anything past `-m 256M`) lives in the *high* aperture at
// `QEMU_LA64_HIGH_RAM_BASE`, separated by the MMIO/PCIe hole. `RAM_END` is the
// low-RAM/IO-hole boundary and the no-firmware fallback size — NOT the total.
const QEMU_LA64_RAM_SIZE: usize = 0x1000_0000;
const QEMU_LA64_RAM_END: usize = QEMU_LA64_RAM_BASE + QEMU_LA64_RAM_SIZE;
// QEMU `virt` places high RAM (guest memory beyond the low 256 MiB) at this
// physical base, above the MMIO/PCIe apertures. Documents the layout the DTB
// high-RAM region (recovered by the DMW-wide direct map) lands in; referenced
// by the direct-map coverage test.
#[allow(dead_code)]
const QEMU_LA64_HIGH_RAM_BASE: usize = 0x9000_0000;
// The cached DMW window (VSEG 0x9) hardware-maps the *entire* physical address
// space at a fixed offset, so the kernel direct map spans all of it — high RAM
// is reachable with no per-page mappings. This bounds the direct-map bookkeeping
// (`direct_map_covers_phys_end`, `extend_direct_map`); the actual frame-metadata
// span is still carved from the real firmware memory map, not from this size.
const QEMU_LA64_DIRECT_MAP_SIZE: usize = LA64_PHYS_ADDR_MASK + 1;
const QEMU_LA64_KERNEL_LOAD_BASE: usize = 0x0020_0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_PCH_PIC_BASE: usize = 0x1000_0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_GED_REG_BASE: usize = 0x100e_001c;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_GED_SLEEP_CTL: usize = QEMU_LA64_GED_REG_BASE;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_GED_SLEEP_VALUE_S5: u8 = (5 << 2) | (1 << 5);
const QEMU_LA64_GSI_BASE: u32 = 64;
const QEMU_LA64_PCH_PIC_IRQS: u32 = 64;
#[cfg_attr(not(test), allow(dead_code))]
const QEMU_LA64_UART0_IRQ: u32 = 66;
const QEMU_LA64_RTC_IRQ: u32 = QEMU_LA64_GSI_BASE + 6;
// QEMU's LoongArch `virt` GPEX host routes PCI INTx outputs to PCH-PIC
// inputs 16..19. The board profile pins virtio-net-pci at slot 2 and the
// device uses INTA (pin index 0), so the standard PCI swizzle selects input
// 16 + ((0 + 2) % 4) = 18. Public PCH-PIC IRQs carry the GSI base.
const QEMU_LA64_PCI_INTX_BASE: u32 = 16;
const QEMU_LA64_VIRTIO_NET_PCI_SLOT: u32 = 2;
const QEMU_LA64_VIRTIO_NET_PCI_PIN: u32 = 0;
const QEMU_LA64_NET_IRQ: u32 = QEMU_LA64_GSI_BASE
    + QEMU_LA64_PCI_INTX_BASE
    + ((QEMU_LA64_VIRTIO_NET_PCI_PIN + QEMU_LA64_VIRTIO_NET_PCI_SLOT) % 4);
const QEMU_LA64_PCIE_ECAM_BASE: usize = 0x2000_0000;
const QEMU_LA64_PCIE_ECAM_SIZE: usize = 0x0800_0000;
const QEMU_LA64_PCIE_MMIO32_BASE: usize = 0x4000_0000;
const QEMU_LA64_PCIE_MMIO32_SIZE: usize = 0x4000_0000;
const QEMU_LA64_PCH_MSI_BASE: usize = 0x2ff0_0000;
const QEMU_LA64_PCH_MSI_SIZE: usize = 0x8;
const QEMU_LA64_RTC_BASE: usize = 0x100d_0100;
const QEMU_LA64_RTC_SIZE: usize = 0x100;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_FW_CFG_BASE: usize = 0x1e02_0000;
const QEMU_LA64_FDT_BASE: usize = 0x0010_0000;
// Keep this in sync with the 12 per-hart boot stacks reserved by the linker
// and with the QEMU evaluation profile (`-smp 12`).
const LA64_MAX_BOOT_CPUS: usize = 12;
#[cfg(target_arch = "loongarch64")]
const LA64_DEFAULT_POSSIBLE_CPUS: usize = LA64_MAX_BOOT_CPUS;
#[cfg(not(target_arch = "loongarch64"))]
const LA64_DEFAULT_POSSIBLE_CPUS: usize = 1;
const LA64_DMW_CACHED_BASE: usize = 0x9000_0000_0000_0000;
const LA64_DMW_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
const LA64_PHYS_ADDR_MASK: usize = (1usize << 48) - 1;
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
const LA64_TCFG_PERIODIC: usize = 1 << 1;
const LA64_TCFG_TICK_MASK: usize = !0x3;
const LA64_TICLR_CLEAR_TIMER: usize = 1 << 0;
const LA64_CPUCFG2_LLFTP: u32 = 1 << 14;
const LA64_CPUCFG2: usize = 0x2;
const LA64_CPUCFG4: usize = 0x4;
const LA64_CPUCFG5: usize = 0x5;
const LA64_ESTAT_IS_HWI_MASK: usize = 0xff << 2;
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
// ADE is Ecode 8. ADEF/ADEM are its EsubCodes; ALE is Ecode 9.
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
const LA64_SIGFRAME_MAGIC: u64 = 0x5458_5632_4c41_5331; // "TXV2LAS1"
const LA64_SIGFRAME_VERSION: u32 = 1;
const LA64_RT_SIGRETURN_SYSCALL: u32 = 139;
const LA64_ADDI_D_R11_ZERO_RT_SIGRETURN: u32 = la64_addi_d(11, 0, LA64_RT_SIGRETURN_SYSCALL);
const LA64_SYSCALL_0: u32 = 0x002b_0000;
const LA64_SIGRETURN_TRAMPOLINE: [u32; 2] = [LA64_ADDI_D_R11_ZERO_RT_SIGRETURN, LA64_SYSCALL_0];
const LA64_EIOINTC_BASE: usize = 0x1400;
const LA64_EIOINTC_ENABLE_START: usize = 0x200;
const LA64_EIOINTC_COREISR_START: usize = 0x400;
const LA64_EIOINTC_IRQS: u32 = 256;
const LA64_PCH_PIC_MASK_START: usize = 0x20;
const LA64_PCH_PIC_CLEAR_START: usize = 0x80;
const LA64_PCH_PIC_HTMSI_VECTOR_START: usize = 0x200;
const LS7A_RTC_TOYWRITE0: usize = 0x24;
const LS7A_RTC_TOYWRITE1: usize = 0x28;
const LS7A_RTC_TOYREAD0: usize = 0x2c;
const LS7A_RTC_TOYREAD1: usize = 0x30;
const LS7A_RTC_TOYMATCH0: usize = 0x34;
const LS7A_RTC_CTRL: usize = 0x40;
const LS7A_RTC_CTRL_EO: u32 = 1 << 8;
const LS7A_RTC_CTRL_TOYEN: u32 = 1 << 11;
const NANOS_PER_SEC: u64 = 1_000_000_000;

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
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_EIOINTC_ENABLE0: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_EIOINTC_COREISR0: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_PCH_PIC_MASK: AtomicU64 = AtomicU64::new(u64::MAX);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_PCH_PIC_HTMSI_VECTOR: [AtomicU8; QEMU_LA64_PCH_PIC_IRQS as usize] =
    [const { AtomicU8::new(0) }; QEMU_LA64_PCH_PIC_IRQS as usize];

#[cfg(all(not(target_arch = "loongarch64"), test))]
struct HostLs7aRtcState {
    registers: [u32; QEMU_LA64_RTC_SIZE / core::mem::size_of::<u32>()],
    read_offsets: [usize; 16],
    read_len: usize,
    write_offsets: [usize; 16],
    write_values: [u32; 16],
    write_len: usize,
}

#[cfg(all(not(target_arch = "loongarch64"), test))]
impl HostLs7aRtcState {
    const fn new() -> Self {
        Self {
            registers: [0; QEMU_LA64_RTC_SIZE / core::mem::size_of::<u32>()],
            read_offsets: [0; 16],
            read_len: 0,
            write_offsets: [0; 16],
            write_values: [0; 16],
            write_len: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn read_u32(&mut self, offset: usize) -> u32 {
        self.record_read(offset);
        self.registers
            .get(offset / core::mem::size_of::<u32>())
            .copied()
            .unwrap_or(0)
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.record_write(offset, value);
        if let Some(register) = self.registers.get_mut(offset / core::mem::size_of::<u32>()) {
            *register = value;
        }
    }

    fn record_read(&mut self, offset: usize) {
        if let Some(slot) = self.read_offsets.get_mut(self.read_len) {
            *slot = offset;
            self.read_len += 1;
        }
    }

    fn record_write(&mut self, offset: usize, value: u32) {
        if let Some(slot) = self.write_offsets.get_mut(self.write_len) {
            *slot = offset;
        }
        if let Some(slot) = self.write_values.get_mut(self.write_len) {
            *slot = value;
            self.write_len += 1;
        }
    }

    fn read_log(&self) -> &[usize] {
        &self.read_offsets[..self.read_len]
    }

    fn write_log(&self) -> &[usize] {
        &self.write_offsets[..self.write_len]
    }

    fn write_values(&self) -> &[u32] {
        &self.write_values[..self.write_len]
    }
}

#[cfg(all(not(target_arch = "loongarch64"), test))]
static LA64_HOST_LS7A_RTC_STATE: std::sync::Mutex<HostLs7aRtcState> =
    std::sync::Mutex::new(HostLs7aRtcState::new());

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
    fn tx_la64_qemu_fp_save_context(ctx: *mut UserFpContext) -> usize;
    fn tx_la64_qemu_fp_restore_context(ctx: *const UserFpContext);
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
    MmioRegion {
        name: "ls7a-rtc",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_RTC_BASE),
            size: QEMU_LA64_RTC_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_RTC_BASE)),
            size: QEMU_LA64_RTC_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
];

fn ls7a_toy_registers_from_unix_ns(ns: u64) -> Result<(u32, u32), PersistentClockError> {
    let seconds = ns / NANOS_PER_SEC;
    let (year, month, day, hour, minute, second) = civil_from_unix_seconds(seconds)?;
    if !(1900..=2099).contains(&year) {
        return Err(PersistentClockError::Range);
    }

    let tm_year = (year - 1900) as u32;
    let toy0 = ((month as u32) << 26)
        | ((day as u32) << 21)
        | ((hour as u32) << 16)
        | ((minute as u32) << 10)
        | ((second as u32) << 4);
    Ok((toy0, tm_year))
}

fn ls7a_toymatch_from_unix_ns(ns: u64) -> Result<u32, PersistentClockError> {
    let seconds = ns / NANOS_PER_SEC;
    let (year, month, day, hour, minute, second) = civil_from_unix_seconds(seconds)?;
    if !(1900..=2099).contains(&year) {
        return Err(PersistentClockError::Range);
    }

    let tm_year = (year - 1900) as u32;
    Ok(((tm_year & 0x3f) << 26)
        | ((month as u32) << 22)
        | ((day as u32) << 17)
        | ((hour as u32) << 12)
        | ((minute as u32) << 6)
        | second as u32)
}

fn ls7a_unix_ns_from_toy_registers(toy0: u32, toy1: u32) -> Result<u64, PersistentClockError> {
    let month = ((toy0 >> 26) & 0x3f) as u8;
    let day = ((toy0 >> 21) & 0x1f) as u8;
    let hour = ((toy0 >> 16) & 0x1f) as u8;
    let minute = ((toy0 >> 10) & 0x3f) as u8;
    let second = ((toy0 >> 4) & 0x3f) as u8;
    let year = 1900i32
        .checked_add(toy1 as i32)
        .ok_or(PersistentClockError::Range)?;
    unix_ns_from_civil(year, month, day, hour, minute, second)
}

fn unix_ns_from_civil(
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
) -> Result<u64, PersistentClockError> {
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(PersistentClockError::Invalid);
    }

    let days = days_from_civil(year, month as u32, day as u32);
    if days < 0 {
        return Err(PersistentClockError::Range);
    }
    let seconds = (days as u64)
        .checked_mul(86_400)
        .and_then(|base| base.checked_add(u64::from(hour) * 3_600))
        .and_then(|base| base.checked_add(u64::from(minute) * 60))
        .and_then(|base| base.checked_add(u64::from(second.min(59))))
        .ok_or(PersistentClockError::Range)?;
    seconds
        .checked_mul(NANOS_PER_SEC)
        .ok_or(PersistentClockError::Range)
}

fn civil_from_unix_seconds(
    seconds: u64,
) -> Result<(i32, u8, u8, u8, u8, u8), PersistentClockError> {
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let (year, month, day) =
        civil_from_days(i64::try_from(days).map_err(|_| PersistentClockError::Range)?);
    Ok((
        year,
        month as u8,
        day as u8,
        (day_seconds / 3_600) as u8,
        ((day_seconds % 3_600) / 60) as u8,
        (day_seconds % 60) as u8,
    ))
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era) * 146_097 + i64::from(doe) - 719_468
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
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
    const BOARD: &'static str = "qemu-loongarch64-virt";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = 48;
    const VIRT_ADDR_BITS: u8 = 48;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(LA64_DMW_CACHED_BASE);
    const DIRECT_MAP_SIZE: usize = QEMU_LA64_DIRECT_MAP_SIZE;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(la64_cached_virt(QEMU_LA64_KERNEL_LOAD_BASE));
    const USER_TOP: VirtAddr = VirtAddr(LA64_USER_TOP);
    const KERNEL_STACK_SIZE: usize = 512 * 1024;
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
        ensure_static_boot_facts(); // 采集并发布 BootInfo/PlatformInfo(等价 riscv 的发布步)

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
mod la64_percpu;
mod la64_pmap;
mod la64_unaligned;
mod platform_impls;
mod trap_asm;

#[cfg(test)]
mod tests;
