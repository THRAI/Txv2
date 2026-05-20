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
    SavedSignalFrame, SecondaryEntry, SignalFrameIf, SignalFramePlacement, SignalFrameWrite,
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
const QEMU_LA64_RAM_SIZE: usize = 0x1000_0000;
const QEMU_LA64_RAM_END: usize = QEMU_LA64_RAM_BASE + QEMU_LA64_RAM_SIZE;
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
const QEMU_LA64_PCIE_ECAM_BASE: usize = 0x2000_0000;
const QEMU_LA64_PCIE_ECAM_SIZE: usize = 0x0800_0000;
const QEMU_LA64_PCIE_MMIO32_BASE: usize = 0x4000_0000;
const QEMU_LA64_PCIE_MMIO32_SIZE: usize = 0x4000_0000;
const QEMU_LA64_PCH_MSI_BASE: usize = 0x2ff0_0000;
const QEMU_LA64_PCH_MSI_SIZE: usize = 0x8;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_FW_CFG_BASE: usize = 0x1e02_0000;
const QEMU_LA64_FDT_BASE: usize = 0x0010_0000;
const LA64_MAX_BOOT_CPUS: usize = 4;
#[cfg(target_arch = "loongarch64")]
const LA64_DEFAULT_POSSIBLE_CPUS: usize = LA64_MAX_BOOT_CPUS;
#[cfg(not(target_arch = "loongarch64"))]
const LA64_DEFAULT_POSSIBLE_CPUS: usize = 1;
const LA64_DMW_CACHED_BASE: usize = 0x9000_0000_0000_0000;
const LA64_DMW_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
const LA64_PHYS_ADDR_MASK: usize = (1usize << 48) - 1;
const LA64_CSR_CRMD: usize = 0x00;
#[cfg(target_arch = "loongarch64")]
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
#[cfg(target_arch = "loongarch64")]
const LA64_EUEN_FPE: usize = 1 << 0;
const LA64_ASID_MASK: usize = 0x3ff;
const LA64_TCFG_ENABLE: usize = 1 << 0;
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
const LA64_ECODE_ADEF: usize = 8;
const LA64_ECODE_ADEM: usize = 9;
const LA64_ECODE_ALE: usize = 10;
const LA64_ECODE_SYS: usize = 11;
const LA64_ECODE_BRK: usize = 12;
const LA64_ECODE_INE: usize = 13;
const LA64_ECODE_IPE: usize = 14;
const LA64_ECODE_FPD: usize = 15;
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

const fn la64_addi_d(rd: u32, rj: u32, imm12: u32) -> u32 {
    0x02c0_0000 | ((imm12 & 0x0fff) << 10) | ((rj & 0x1f) << 5) | (rd & 0x1f)
}

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
static LA64_ALLOCATED_ASIDS: AtomicU64 = AtomicU64::new(1);
static LA64_KERNEL_PGDH_PHYS: AtomicUsize = AtomicUsize::new(0);
static LA64_KERNEL_PGDH_BOOTSTRAP_MAPPED: AtomicBool = AtomicBool::new(false);
static LA64_ACTIVE_PGDL: AtomicUsize = AtomicUsize::new(0);
static LA64_ACTIVE_PGDH: AtomicUsize = AtomicUsize::new(0);
static LA64_ACTIVE_ASID: AtomicUsize = AtomicUsize::new(0);
static LA64_COMMITTED_PT_NODE_REGISTRY_LOCK: AtomicBool = AtomicBool::new(false);
static LA64_COMMITTED_PT_NODES: La64CommittedPtNodeRegistry =
    La64CommittedPtNodeRegistry(UnsafeCell::new([None; 256]));
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

struct La64CommittedPtNodeRegistry(UnsafeCell<[Option<PtNode>; 256]>);

unsafe impl Sync for La64CommittedPtNodeRegistry {}

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
        euen &= !LA64_EUEN_FPE;
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
];

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
    const DIRECT_MAP_SIZE: usize = QEMU_LA64_RAM_SIZE;
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
        boot_args::record_legacy_firmware_arg(firmware_arg);
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
mod la64_pmap;
mod la64_unaligned;
mod platform_impls;
mod trap_asm;

#[cfg(test)]
mod tests;
