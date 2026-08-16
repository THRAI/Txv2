#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, AtomicUsize, Ordering};

mod boot_static;
mod boot_trampoline;
mod debug_trace;
mod dtb;
mod pmap;
mod sbi;
mod signal_frame;
mod time;
mod trap;
mod user_access;
pub use trap::{dispatch_trap_frame, emit_panic_location, return_to_userspace, Rv64TrapFrame};

use sbi::read_sbi_console_bytes;
#[cfg(target_arch = "riscv64")]
use sbi::{
    sbi_console_putchar, sbi_hart_start, sbi_remote_fence_i, sbi_remote_sfence_vma,
    sbi_remote_sfence_vma_asid, sbi_send_ipi, sbi_shutdown,
};

use boot_static::{
    reserved_region, BootStaticBag, IdentityDropped, IdentityLive, CMDLINE_CAPACITY,
};
use dtb::parse_boot_info_from_fdt;
use pmap::topology as pmap_topology;
use tx_hal::{
    AllocError, Arch, ArchAuxvFacts, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf,
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask,
    CpuPinGuard, DeadlineTimerIf, DmaAddr, DmaDirection, DmaIf, EntropyIf, InitIf, IpiKind,
    IrqDispatchTable, IrqHandled, IrqIf, LocalExecutionGuard, MemoryRegion, MemoryRegionKind,
    MonotonicCounterIf, ObserverIf, PercpuIf, PersistentClockError, PersistentClockIf, PhysAddr,
    PlatformConfig, PlatformInfo, PlatformInfoIf, PmapError, PmapIf, PmapInvalidation,
    PmapPermissions, PmapReservation, PmapReserveKind, PmapRoot, PmapUnmapResult, PowerIf, PtNode,
    PtNodeAllocator, SecondaryEntry, SmpIf, TimeIf, VdsoCounterInfo, VirtAddr,
};

pub struct Platform;

#[cfg(any(target_arch = "riscv64", test))]
fn for_each_console_byte_for_sbi(bytes: &[u8], mut emit: impl FnMut(u8)) {
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'\r' && bytes.get(idx + 1) == Some(&b'\n') {
            emit(b'\n');
            idx += 2;
            continue;
        }
        emit(bytes[idx]);
        idx += 1;
    }
}

const QEMU_VIRT_RAM_BASE: usize = 0x8000_0000;
const QEMU_VIRT_FALLBACK_RAM_SIZE: usize = 256 * 1024 * 1024;
const MAX_BOOT_CPUS: usize = 8;
pub(crate) const PLIC_PHYS_BASE: usize = 0x0c00_0000;
/// QEMU virt machine's NS16550-compatible UART. PLIC IRQ 10
/// ([`IrqIf::UART_IRQ`]) is wired to this UART, but the device
/// itself only raises RX-data-available IRQs when its IER (offset
/// 1) has bit 0 set. SBI doesn't initialise the device-side IER
/// for us, so the kernel writes it directly during boot — see
/// `enable_uart_rx_irq`.
#[cfg(target_arch = "riscv64")]
const UART_PHYS_BASE: usize = 0x1000_0000;
#[cfg(target_arch = "riscv64")]
const UART_BASE: usize = pmap_topology::DIRECT_MAP_BASE + UART_PHYS_BASE;
const PLIC_MAX_IRQ: u32 = tx_hal::IRQ_DISPATCH_TABLE_SIZE as u32;
#[cfg(all(not(target_arch = "riscv64"), test))]
const PLIC_IRQ_SOURCES: usize = tx_hal::IRQ_DISPATCH_TABLE_SIZE;
#[cfg(all(not(target_arch = "riscv64"), test))]
const PLIC_ENABLE_WORDS: usize = PLIC_IRQ_SOURCES / u32::BITS as usize;
const PLIC_PRIORITY_BASE: usize = 0x0;
const PLIC_ENABLE_BASE: usize = 0x2000;
const PLIC_ENABLE_CONTEXT_STRIDE: usize = 0x80;
const PLIC_CONTEXT_BASE: usize = 0x20_0000;
const PLIC_CONTEXT_STRIDE: usize = 0x1000;
const PLIC_CLAIM_COMPLETE: usize = 0x4;
#[cfg(any(target_arch = "riscv64", test))]
const GOLDFISH_RTC_PHYS_BASE: usize = 0x0010_1000;
#[cfg(target_arch = "riscv64")]
const GOLDFISH_RTC_BASE: usize = pmap_topology::DIRECT_MAP_BASE + GOLDFISH_RTC_PHYS_BASE;
const GOLDFISH_RTC_IRQ: u32 = 11;
const GOLDFISH_RTC_TIME_LOW: usize = 0x00;
const GOLDFISH_RTC_TIME_HIGH: usize = 0x04;
const GOLDFISH_RTC_ALARM_LOW: usize = 0x08;
const GOLDFISH_RTC_ALARM_HIGH: usize = 0x0c;
const GOLDFISH_RTC_IRQ_ENABLED: usize = 0x10;
const GOLDFISH_RTC_CLEAR_ALARM: usize = 0x14;
const GOLDFISH_RTC_ALARM_STATUS: usize = 0x18;
const GOLDFISH_RTC_CLEAR_INTERRUPT: usize = 0x1c;
static ONLINE_CPUS: AtomicU64 = AtomicU64::new(0);
const IPI_KIND_COUNT: usize = 5;
static IPI_PENDING: [AtomicU8; MAX_BOOT_CPUS] = [const { AtomicU8::new(0) }; MAX_BOOT_CPUS];
static IPI_ACKED_CPUS: [AtomicU64; IPI_KIND_COUNT] = [const { AtomicU64::new(0) }; IPI_KIND_COUNT];
#[cfg(test)]
static TEST_IPI_TRANSPORT_MASK: AtomicU64 = AtomicU64::new(0);
/// Harts whose hardware currently has this ASID installed in `satp`.
///
/// This is a root-lifetime reference: it is cleared as soon as a hart has
/// switched away and is used to decide when page-table pages may be freed.
static ASID_RESIDENCY: [AtomicU64; pmap::ASID_CAPACITY] =
    [const { AtomicU64::new(0) }; pmap::ASID_CAPACITY];
/// Harts which may retain TLB entries tagged with this ASID.
///
/// Unlike `ASID_RESIDENCY`, a context switch must not clear this mask. RISC-V
/// permits tagged entries to survive a `satp` switch, so a hart that ran an
/// address space earlier still needs every later unmap/protection shootdown.
/// The mask is reset only after a full all-hart ASID invalidation immediately
/// before ASID reuse.
static ASID_TLB_HARTS: [AtomicU64; pmap::ASID_CAPACITY] =
    [const { AtomicU64::new(0) }; pmap::ASID_CAPACITY];
static FALLBACK_IRQ_DEPTH: AtomicUsize = AtomicUsize::new(0);
static INSTALLED_IRQ_TABLE: AtomicPtr<IrqDispatchTable> = AtomicPtr::new(core::ptr::null_mut());

#[repr(C, align(64))]
pub struct Rv64PerCpuArea {
    cpu_id: usize,
    kernel_stack_top: AtomicUsize,
    irq_depth: AtomicUsize,
    /// Dedicated architecture trap-stack top. The RV64 trap-vector reads
    /// this field directly through kernel `tp` for from-kernel traps, so
    /// its offset is part of the assembly ABI.
    trap_stack_top: AtomicUsize,
    /// Software ASID whose user root is currently installed in `satp`.
    ///
    /// This cannot be reconstructed from `satp.ASID` on hardware that
    /// implements zero ASID bits, so it must be tracked per hart.
    active_user_asid: AtomicUsize,
    /// Nesting depth of non-migratable kernel sections (EBR/zone guards).
    cpu_pin_depth: AtomicUsize,
}

impl Rv64PerCpuArea {
    pub const fn new(cpu_id: usize) -> Self {
        Self {
            cpu_id,
            kernel_stack_top: AtomicUsize::new(0),
            irq_depth: AtomicUsize::new(0),
            trap_stack_top: AtomicUsize::new(0),
            active_user_asid: AtomicUsize::new(0),
            cpu_pin_depth: AtomicUsize::new(0),
        }
    }

    pub fn cpu_id(&self) -> CpuId {
        CpuId(self.cpu_id)
    }

    pub fn kernel_stack_top(&self) -> VirtAddr {
        VirtAddr(self.kernel_stack_top.load(Ordering::Acquire))
    }

    pub fn irq_depth(&self) -> usize {
        self.irq_depth.load(Ordering::Acquire)
    }
}

const RV64_PERCPU_TRAP_STACK_TOP_OFFSET: usize = 24;
const _: () = assert!(
    core::mem::offset_of!(Rv64PerCpuArea, trap_stack_top) == RV64_PERCPU_TRAP_STACK_TOP_OFFSET
);

static RV64_PERCPU_AREAS: [Rv64PerCpuArea; MAX_BOOT_CPUS] = [
    Rv64PerCpuArea::new(0),
    Rv64PerCpuArea::new(1),
    Rv64PerCpuArea::new(2),
    Rv64PerCpuArea::new(3),
    Rv64PerCpuArea::new(4),
    Rv64PerCpuArea::new(5),
    Rv64PerCpuArea::new(6),
    Rv64PerCpuArea::new(7),
];

/// Per-hart save area for the reschedule longjmp (slice 2 of the
/// userspace-first-entry fix per
/// `docs/progress/decisions/2026-05-08-userspace-first-entry-gap.md`).
///
/// The userspace-entry shim (`tx_rv64_enter_userspace_save_resume`)
/// stashes (sp, ra, s0..s11) here before `sret`. The trap-shell
/// longjmp helper (`tx_rv64_resume_kernel_after_reschedule`)
/// restores them on `TrapAction::Reschedule` and `ret`s back to the
/// kernel-side caller of `enter_userspace_with_context`.
///
/// Field offsets are load-bearing: the asm helpers reference them
/// by literal byte offset. Keep `KERNEL_RESUME_CTX_*_OFFSET` in
/// sync with the field order.
#[repr(C, align(8))]
pub struct KernelResumeCtx {
    pub sp: usize,      // offset 0
    pub ra: usize,      // offset 8
    pub s: [usize; 12], // offset 16..112
    /// Address of the Rust userspace-entry frame's saved caller return PC.
    ///
    /// This is temporary fault-localisation state. The RV64 entry assembly
    /// records it before `sret` so the trap path can identify the first phase
    /// that damages the suspended kernel stack frame.
    pub caller_ra_slot: usize, // offset 112
    /// Value observed in `caller_ra_slot` immediately before `sret`.
    pub caller_ra_expected: usize, // offset 120
    /// Exact stack pointer loaded by the most recent resume longjmp.
    pub resume_loaded_sp: usize, // offset 128
    /// Exact return address loaded by the most recent resume longjmp.
    pub resume_loaded_ra: usize, // offset 136
    /// Monotonic generation assigned by the userspace-entry save helper.
    pub entry_generation: usize, // offset 144
    /// Entry generation observed by the most recent resume helper.
    pub resume_generation: usize, // offset 152
}

impl KernelResumeCtx {
    const fn zeroed() -> Self {
        Self {
            sp: 0,
            ra: 0,
            s: [0; 12],
            caller_ra_slot: 0,
            caller_ra_expected: 0,
            resume_loaded_sp: 0,
            resume_loaded_ra: 0,
            entry_generation: 0,
            resume_generation: 0,
        }
    }
}

// These offsets are referenced by literal byte offset in the trap-vector
// asm (`TX_RV64_RCTX_SP`, `TX_RV64_RCTX_RA`, `TX_RV64_RCTX_S0`). The
// Rust constants below pin the layout from the Rust side so a struct
// reorder triggers a compile-time mismatch with the static_assert.
const _KERNEL_RESUME_CTX_SP_OFFSET: usize = 0;
const _KERNEL_RESUME_CTX_RA_OFFSET: usize = 8;
const _KERNEL_RESUME_CTX_S0_OFFSET: usize = 16;
const _KERNEL_RESUME_CTX_CALLER_RA_SLOT_OFFSET: usize = 112;
const _KERNEL_RESUME_CTX_CALLER_RA_EXPECTED_OFFSET: usize = 120;
const _KERNEL_RESUME_CTX_LOADED_SP_OFFSET: usize = 128;
const _KERNEL_RESUME_CTX_LOADED_RA_OFFSET: usize = 136;
const _KERNEL_RESUME_CTX_ENTRY_GENERATION_OFFSET: usize = 144;
const _KERNEL_RESUME_CTX_RESUME_GENERATION_OFFSET: usize = 152;
const _: () = assert!(core::mem::size_of::<KernelResumeCtx>() == 20 * 8);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, sp) == _KERNEL_RESUME_CTX_SP_OFFSET);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, ra) == _KERNEL_RESUME_CTX_RA_OFFSET);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, s) == _KERNEL_RESUME_CTX_S0_OFFSET);
const _: () = assert!(
    core::mem::offset_of!(KernelResumeCtx, caller_ra_slot)
        == _KERNEL_RESUME_CTX_CALLER_RA_SLOT_OFFSET
);
const _: () = assert!(
    core::mem::offset_of!(KernelResumeCtx, caller_ra_expected)
        == _KERNEL_RESUME_CTX_CALLER_RA_EXPECTED_OFFSET
);
const _: () = assert!(
    core::mem::offset_of!(KernelResumeCtx, resume_loaded_sp) == _KERNEL_RESUME_CTX_LOADED_SP_OFFSET
);
const _: () = assert!(
    core::mem::offset_of!(KernelResumeCtx, resume_loaded_ra) == _KERNEL_RESUME_CTX_LOADED_RA_OFFSET
);
const _: () = assert!(
    core::mem::offset_of!(KernelResumeCtx, entry_generation)
        == _KERNEL_RESUME_CTX_ENTRY_GENERATION_OFFSET
);
const _: () = assert!(
    core::mem::offset_of!(KernelResumeCtx, resume_generation)
        == _KERNEL_RESUME_CTX_RESUME_GENERATION_OFFSET
);

/// Per-hart cell with `Sync` because the only writer/reader is the
/// local hart's trap-vector / userspace-entry shim. Cross-hart
/// concurrent access would be a load-bearing invariant violation.
#[repr(transparent)]
pub struct PerHartCell<T>(core::cell::UnsafeCell<T>);

unsafe impl<T> Sync for PerHartCell<T> {}

impl<T> PerHartCell<T> {
    pub const fn new(value: T) -> Self {
        Self(core::cell::UnsafeCell::new(value))
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0.get()
    }
}

static RV64_KERNEL_RESUME_CTX: [PerHartCell<KernelResumeCtx>; MAX_BOOT_CPUS] =
    [const { PerHartCell::new(KernelResumeCtx::zeroed()) }; MAX_BOOT_CPUS];

/// Per-CPU trap-handler stack. Sized 128 KiB; the trap vector
/// `csrrw`-swaps onto its top via `sscratch` so trap-handler frames
/// don't trample the BSP/AP runtime kernel stack (which holds the
/// reactor + thread future frames at the moment a user trap fires).
///
/// Lives in `.data` (writable) rather than substrate-allocated
/// frames because the trap stack must be ready *before* substrate
/// is — the trap vector is installed early in boot, well before
/// the page allocator. Wrapped in `PerHartCell<UnsafeCell<...>>`
/// so the linker keeps it out of `.rodata` (which `pmap` maps
/// `KERNEL_RO`); plain `static [u8; N]` lands in `.rodata` and
/// the trap-vector's first store would fault.
///
/// The extra headroom keeps the proven synchronous RV64 syscall lanes on the
/// direct path without letting an unusually deep call chain reach the adjacent
/// resume-context area. The stacks live in `.bss`, so this does not increase
/// the submitted kernel image size.
///
/// Total static cost is `MAX_BOOT_CPUS × 128 KiB = 1 MiB`.
const RV64_TRAP_STACK_SIZE: usize = 128 * 1024;

#[repr(C, align(16))]
pub struct Rv64TrapStack(pub [u8; RV64_TRAP_STACK_SIZE]);

static RV64_TRAP_STACKS: [PerHartCell<Rv64TrapStack>; MAX_BOOT_CPUS] = [
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
    PerHartCell::new(Rv64TrapStack([0; RV64_TRAP_STACK_SIZE])),
];

/// Per-hart trap-stack top: the value the boot primer writes into
/// `sscratch`, and the value the reschedule longjmp restores
/// `sscratch` to before unwinding back to the kernel caller.
pub fn trap_stack_top_for_cpu(cpu: CpuId) -> usize {
    let stack = RV64_TRAP_STACKS[cpu.0].as_ptr();
    let base = stack as usize;
    base + RV64_TRAP_STACK_SIZE
}

/// Rebuild the kernel TLS value for a trap that landed on `trap_stack_top`.
///
/// From-user traps arrive with `tp` holding the user register x4, so the trap
/// vector cannot call Rust until it has restored kernel TLS. `sscratch` tells
/// the vector which per-hart trap stack it swapped onto; matching that stack
/// top back to a CPU gives the correct per-CPU area pointer for `tp`.
#[cfg(target_arch = "riscv64")]
#[no_mangle]
pub extern "C" fn tx_rv64_kernel_tls_from_trap_stack_top(trap_stack_top: usize) -> usize {
    let mut cpu = 0;
    while cpu < MAX_BOOT_CPUS {
        let cpu_id = CpuId(cpu);
        if trap_stack_top_for_cpu(cpu_id) == trap_stack_top {
            return percpu_tls_for_cpu(cpu_id).unwrap_or(cpu);
        }
        cpu += 1;
    }
    0
}

/// Pointer to one hart's [`KernelResumeCtx`].
///
/// Trap return code uses the hart identity recovered from the dedicated trap
/// stack instead of consulting `tp` again.  This keeps a transient TLS error
/// from selecting a second hart's suspended kernel context.
pub fn kernel_resume_ctx_ptr_for_cpu(cpu: CpuId) -> *mut KernelResumeCtx {
    assert!(cpu.0 < MAX_BOOT_CPUS, "invalid RV64 resume-context CPU");
    RV64_KERNEL_RESUME_CTX[cpu.0].as_ptr()
}

/// Pointer to the local hart's [`KernelResumeCtx`].
pub fn current_kernel_resume_ctx_ptr() -> *mut KernelResumeCtx {
    kernel_resume_ctx_ptr_for_cpu(current_cpu_id())
}

#[cfg(not(target_arch = "riscv64"))]
static HOST_KERNEL_TLS: AtomicUsize = AtomicUsize::new(0);

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::Riscv64;
    const BOARD: &'static str = "qemu-riscv64-virt";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = 56;
    const VIRT_ADDR_BITS: u8 = 39;
    const DIRECT_MAP_BASE: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::DIRECT_MAP_BASE);
    const DIRECT_MAP_SIZE: usize = pmap_topology::DIRECT_MAP_SIZE;
    const KERNEL_VIRT_BASE: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::KERNEL_VIRT_BASE);
    const KERNEL_PAGE_TABLE_ACTIVE_AT_SUBSTRATE_INIT: bool = true;
    const USER_TOP: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::SV39_USER_TOP);
    const USER_RESERVED_TOP_SIZE: usize = pmap_topology::USER_RESERVED_TOP_SIZE;
    const USER_ALLOC_TOP: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::SV39_USER_ALLOC_TOP);
    const KERNEL_STACK_SIZE: usize = 512 * 1024;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 3;
    const ASID_BITS: u8 = 16;
    const CACHE_LINE_SIZE: usize = 64;
    const DMA_COHERENT: bool = true;
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvSbi;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        let bag = BootStaticBag::<IdentityLive>::capture_once(firmware_arg);
        pmap::adopt_high_linked_bootstrap_pmap(bag);
        // QEMU with 8 GiB RAM may place the firmware DTB near the top of RAM
        // (the official lane passes 0x27fe00000), outside the trampoline's
        // initial 1 GiB direct-map leaf. Preseed the leaf containing the DTB
        // before the parser dereferences the firmware pointer through its
        // high direct-map alias.
        #[cfg(target_arch = "riscv64")]
        pmap::cover_boot_firmware_dtb_from_bag(bag, PhysAddr(firmware_arg))
            .expect("firmware DTB is outside the RV64 bootstrap direct map");

        BootStaticBag::<IdentityLive>::take_global()
            .publish_boot_info_before_identity_drop(firmware_arg)
            .complete_post_entry_pipeline()
            .install_global();

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
        BootStaticBag::<IdentityDropped>::global_ref().boot_info_ref() // 取内核初始化信息
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        BootStaticBag::<IdentityDropped>::global_ref().platform_info_ref() // 取硬件信息
    }

    fn devices() -> &'static [tx_hal::DeviceInfo] {
        BootStaticBag::<IdentityDropped>::global_ref().platform_devices_ref()
    }
}

impl AuxvIf for Platform {
    fn arch_auxv_facts() -> ArchAuxvFacts {
        ArchAuxvFacts::new(Self::PAGE_SIZE, tx_hal::RISCV_HWCAP_IMAFDC, 0, "riscv64")
    }
}
impl ConsoleIf for Platform {
    fn write_bytes(bytes: &[u8]) {
        #[cfg(target_arch = "riscv64")]
        {
            // The console TTY already expands `\n` to `\r\n` via ONLCR.
            // QEMU virt's SBI console path renders bare `\n` as a host newline,
            // so forwarding the cooked `\r\n` pair byte-for-byte produces
            // `\r\r\n` in captured logs. Collapse cooked CRLF back to LF before
            // handing bytes to SBI so the final host-visible stream is `\r\n`.
            for_each_console_byte_for_sbi(bytes, sbi_console_putchar);
        }

        #[cfg(not(target_arch = "riscv64"))]
        let _ = bytes;
    }

    fn read_bytes(buf: &mut [u8]) -> usize {
        read_sbi_console_bytes(buf)
    }
}
// PmapIf 的板级实现:除 activate_user_pmap(写 satp)外,几乎每个方法都是一行转发到
// pmap 模块的对应函数(内核映射→kernel_space,用户映射→address_space,节点→pt_node);
// shootdown 系列在本地 sfence 之外额外追加 SBI 远程 IPI(remote_sfence_vma*)。
impl PmapIf for Platform {
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
        pmap::bootstrap_pmap_info()
    }

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        pmap::alloc_pt_node()
    }

    fn free_pt_node(node: PtNode) {
        pmap::free_pt_node(node);
    }

    fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError> {
        pmap::install_pt_node_allocator(allocator)
    }

    fn reserve_kernel_direct_map_1g(phys: PhysAddr) -> Result<Option<PmapReservation>, PmapError> {
        pmap::reserve_kernel_direct_map_1g(phys)
    }

    fn commit_kernel_direct_map_1g(reservation: PmapReservation) {
        pmap::commit_kernel_direct_map_1g(reservation);
    }

    fn extend_direct_map(phys_end: PhysAddr) -> Result<(), PmapError> {
        pmap::extend_direct_map(phys_end)
    }

    fn reserve_kernel_mapping(
        virt: tx_hal::VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        pmap::reserve_kernel_mapping(virt, phys, kind)
    }

    fn rollback_kernel_mapping(reservation: PmapReservation) {
        pmap::rollback_kernel_mapping(reservation);
    }

    fn commit_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
        pmap::commit_kernel_mapping(reservation, permissions);
    }

    fn commit_new_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
        pmap::commit_new_kernel_mapping(reservation, permissions);
    }

    fn unmap_kernel_mapping(
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        pmap::unmap_kernel_mapping(virt, kind)
    }

    fn protect_kernel_mapping(
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        pmap::protect_kernel_mapping(virt, kind, permissions)
    }

    fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
        pmap::shootdown_kernel_mapping(invalidation);
        remote_sfence_vma(invalidation);
    }

    fn shootdown_kernel_mappings(invalidations: &[PmapInvalidation]) {
        let Some(first) = invalidations.first().copied() else {
            return;
        };
        let mut start = first.virt().0;
        let mut end = start.saturating_add(first.size());
        for invalidation in &invalidations[1..] {
            start = start.min(invalidation.virt().0);
            end = end.max(invalidation.virt().0.saturating_add(invalidation.size()));
        }

        // Kernel mappings are shared by every address space. The current RV64
        // backend already upgrades a single invalidation to one local full
        // sfence, so do that once for the whole batch and issue one remote SBI
        // range request instead of one request per 4-KiB page.
        let merged = PmapInvalidation::new(VirtAddr(start), end.saturating_sub(start));
        pmap::shootdown_kernel_mapping(merged);
        remote_sfence_vma(merged);
    }

    fn create_pmap_root() -> Result<PmapRoot, PmapError> {
        pmap::create_pmap_root()
    }

    fn destroy_pmap_root(root: PmapRoot) {
        pmap::destroy_pmap_root(root);
    }

    fn reserve_mapping(
        root: &PmapRoot,
        virt: tx_hal::VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        pmap::reserve_mapping(root, virt, phys, kind)
    }

    fn rollback_mapping(root: &PmapRoot, reservation: PmapReservation) {
        pmap::rollback_mapping(root, reservation);
    }

    fn commit_mapping(root: &PmapRoot, reservation: PmapReservation, permissions: PmapPermissions) {
        pmap::commit_mapping(root, reservation, permissions);
    }

    fn unmap_mapping(
        root: &PmapRoot,
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapUnmapResult>, PmapError> {
        pmap::unmap_mapping(root, virt, kind)
    }

    fn protect_mapping(
        root: &PmapRoot,
        virt: tx_hal::VirtAddr,
        kind: PmapReserveKind,
        permissions: PmapPermissions,
    ) -> Result<Option<PmapInvalidation>, PmapError> {
        pmap::protect_mapping(root, virt, kind, permissions)
    }

    fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
        pmap::shootdown_mapping(asid, invalidation);
        remote_sfence_vma_asid_batch(asid, &[invalidation]);
    }

    fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        pmap::shootdown_mappings(asid, invalidations);
    }

    fn synchronize_new_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
        pmap::synchronize_new_mappings(asid, invalidations);
    }

    /// 写 satp 指向 `root.phys()`(带 Sv39 模式位 + 该 root 的 ASID),再发一条本地 sfence.vma。
    ///
    /// 线程运行时在 `TrapIf::enter_userspace_with_context` 之前紧接着调它,好让用户态取指
    /// 看到本进程的页表。不调的话 satp 还指着引导 trampoline 留下的内核引导根(没有用户映射),
    /// 用户态每条取指都会永久缺页。
    fn activate_user_pmap(root: &PmapRoot) {
        let asid_usable = hw_asid_tagging_usable();
        #[cfg(target_arch = "riscv64")]
        unsafe {
            const SATP_MODE_SV39: usize = 0x8 << 60;
            let ppn = root.phys().0 >> 12;
            // 零/窄 ASID 硬件(VF2 U74 实现 0 位):硬件反正会截断标签;这里写 0,
            // 让下面的快路径比较仍有意义(否则读回被截断的字段永远不等于我们算的 satp,
            // 每次进用户态都被迫走慢路径)。
            let asid = if asid_usable {
                root.asid().0 as usize
            } else {
                0
            };
            let satp = SATP_MODE_SV39 | (asid << 44) | ppn;
            // 快路径:回到同一个地址空间(常见的 syscall 返回)。不写 CSR、不 fence——
            // 这个 root 的 TLB 项仍有效(ASID 硬件按 ASID 隔离;退化硬件靠下面的切换即刷
            // 保证 TLB 里只留当前空间的项)。先做这个判断，避免同地址
            // 空间的高频返回反复写 ASID_RESIDENCY / ASID_TLB_HARTS 共享缓存行。
            let current: usize;
            core::arch::asm!("csrr {satp}, satp", satp = out(reg) current, options(nomem, nostack));
            if current == satp {
                return;
            }

            // Keep the outgoing root resident until hardware has stopped
            // using it. During a real switch both ASIDs are conservatively
            // resident on this hart; an unnecessary shootdown is safe, an
            // early root free is not.
            let switch = begin_asid_switch_on_current_cpu(root.asid());
            // 换了 root:写 satp。ASID 硬件(QEMU:16 位)上无需在地址空间
            // 切换点额外 fence:
            //  - TLB 项带 ASID 标签,切 ASID 不用 fence。
            //  - PTE 的 invalid→valid 发布由 VmPmap 提交后的 ASID 范围
            //    shootdown 完成可见性，不依赖这里补刷。
            //  - ASID 复用在 root 销毁时 fence(invalidate_root_translations 切到引导根并在那 sfence.vma)。
            // 以前这里无条件 sfence.vma 会在每次进用户态时全刷 TLB——QEMU TCG 下相当于每次
            // syscall 返回都 full tlb_flush(约 ms 级),是 LTP shell 测试耗时的主项(net_stress 预算之战)。
            core::arch::asm!(
                "csrw satp, {satp}",
                satp = in(reg) satp,
                options(nostack)
            );
            // 退化(零 ASID)硬件:所有空间共享标签 0,上个空间的项对这个空间仍生效——
            // 所以切换即刷(board `ls` fork/COW 死循环的根因,2026-07-03)。QEMU 从不走这个分支。
            if !asid_usable {
                core::arch::asm!("sfence.vma", options(nostack));
            }
            finish_asid_switch_on_current_cpu(switch);
        }
        #[cfg(not(target_arch = "riscv64"))]
        {
            let _ = asid_usable;
            let switch = begin_asid_switch_on_current_cpu(root.asid());
            finish_asid_switch_on_current_cpu(switch);
        }
    }
}
impl IrqIf for Platform {
    const MAX_IRQ: u32 = PLIC_MAX_IRQ;

    /// QEMU `virt` machine's 16550 UART is wired at PLIC IRQ 10.
    /// Source: `qemu/hw/riscv/virt.c::UART0_IRQ`.
    const UART_IRQ: u32 = 10;
    fn uart_irq() -> u32 {
        uart_device_info()
            .and_then(|uart| uart.irq)
            .unwrap_or(Self::UART_IRQ)
    }

    /// QEMU `virt` machine's goldfish RTC is wired at PLIC IRQ 11.
    const RTC_IRQ: u32 = GOLDFISH_RTC_IRQ;
    /// `virtio1@0x1000_2000` is MMIO slot 1; QEMU wires slot N to
    /// `VIRTIO_IRQ + N`, so the boot network device uses PLIC IRQ 2.
    const NET_IRQ: u32 = 2;

    fn in_irq_context() -> bool {
        irq_context_depth() != 0
    }

    fn in_trap_context() -> bool {
        current_stack_is_trap_stack()
    }

    fn interrupts_enabled() -> bool {
        supervisor_interrupts_enabled()
    }

    fn exclude_local_execution() -> LocalExecutionGuard {
        exclude_supervisor_interrupts()
    }

    fn claim() -> u32 {
        plic_claim(current_plic_context())
    }

    fn complete(irq: u32) {
        if valid_plic_irq(irq) {
            plic_complete(current_plic_context(), irq);
        }
    }

    fn mask(irq: u32) {
        if valid_plic_irq(irq) {
            plic_set_enabled(current_plic_context(), irq, false);
        }
    }

    fn unmask(irq: u32) {
        if valid_plic_irq(irq) {
            plic_set_enabled(current_plic_context(), irq, true);
        }
    }

    fn set_priority(irq: u32, priority: u8) {
        if valid_plic_irq(irq) {
            plic_set_priority(irq, priority);
        }
    }

    fn install_dispatch_table(table: &'static IrqDispatchTable) {
        INSTALLED_IRQ_TABLE.store(
            table as *const IrqDispatchTable as *mut _,
            Ordering::Release,
        );
        // The PLIC routes UART IRQ 10 to us, but the 16550 UART
        // itself only raises that IRQ when its IER (offset 1) has
        // bit 0 set. SBI doesn't initialise the device-side IER
        // for us. We do it here, alongside the dispatch-table
        // install, so the same boot step that wires the kernel
        // handler also enables the device-side trigger.
        enable_uart_rx_irq();
    }

    fn dispatch_irq(irq: u32) -> IrqHandled {
        if !valid_plic_irq(irq) {
            return IrqHandled::Done;
        }

        if let Some(table) = installed_irq_table() {
            if let Some(handler) = table.entries[irq as usize] {
                return handler(irq);
            }
        }

        Self::mask(irq);
        IrqHandled::Done
    }
}
impl MonotonicCounterIf for Platform {
    fn read_ns() -> u64 {
        time::read_ns(<Self as MonotonicCounterIf>::frequency_hz())
    }

    fn frequency_hz() -> u64 {
        Self::platform_info().timebase_frequency_hz
    }

    fn vdso_counter_info() -> Option<VdsoCounterInfo> {
        Some(time::vdso_counter_info(
            <Self as MonotonicCounterIf>::frequency_hz(),
        ))
    }

    fn read_vdso_counter() -> u64 {
        time::read_time_ticks()
    }
}

impl DeadlineTimerIf for Platform {
    fn set_deadline_ns(deadline: u64) {
        time::set_deadline_ns(deadline, <Self as MonotonicCounterIf>::frequency_hz());
    }

    fn cancel_deadline() {
        time::cancel_deadline();
    }

    fn enable_timer_wakeups() {
        time::enable_timer_wakeups();
    }
}

impl PersistentClockIf for Platform {
    fn read_realtime_ns() -> Result<u64, PersistentClockError> {
        Ok(goldfish_rtc_read_time_ns())
    }

    fn set_realtime_ns(ns: u64) -> Result<(), PersistentClockError> {
        goldfish_rtc_write_time_ns(ns);
        Ok(())
    }

    fn set_wake_alarm_ns(ns: u64) -> Result<(), PersistentClockError> {
        goldfish_rtc_program_alarm_ns(ns);
        Self::set_priority(GOLDFISH_RTC_IRQ, 1);
        Self::unmask(GOLDFISH_RTC_IRQ);
        Ok(())
    }

    fn clear_wake_alarm() -> Result<(), PersistentClockError> {
        goldfish_rtc_disable_alarm();
        Self::mask(GOLDFISH_RTC_IRQ);
        Ok(())
    }

    fn acknowledge_wake_alarm_irq() -> Result<(), PersistentClockError> {
        goldfish_rtc_ack_alarm_irq();
        Ok(())
    }
}

impl PercpuIf for Platform {
    fn current_cpu_id() -> CpuId {
        current_cpu_id()
    }

    fn install_early_percpu(cpu_id: CpuId) {
        install_early_percpu(cpu_id);
    }

    fn read_kernel_tls() -> u64 {
        read_kernel_tls() as u64
    }

    fn write_kernel_tls(value: u64) {
        write_kernel_tls(value as usize);
    }

    fn pin_current_cpu() -> CpuPinGuard {
        let cpu = current_cpu_id();
        current_percpu_area()
            .expect("RV64 per-CPU area must exist before CPU pinning")
            .cpu_pin_depth
            .fetch_add(1, Ordering::Relaxed);
        CpuPinGuard::with_unpin(cpu, rv64_unpin_cpu)
    }

    fn cpu_pin_depth() -> usize {
        current_percpu_area()
            .map(|area| area.cpu_pin_depth.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    unsafe fn install_kernel_stack(top: VirtAddr) {
        unsafe { install_kernel_stack(top) };
    }
}

fn rv64_unpin_cpu(cpu: CpuId) {
    debug_assert_eq!(cpu, current_cpu_id());
    let previous = current_percpu_area()
        .expect("RV64 per-CPU area must exist while dropping CPU pin")
        .cpu_pin_depth
        .fetch_sub(1, Ordering::Release);
    assert!(previous != 0, "RV64 CPU pin nesting underflow");
}
impl CacheIf for Platform {
    fn fence_all() {
        rv64_fence_all();
    }

    fn fence_i_local() {
        rv64_fence_i();
    }

    fn fence_i_all() {
        rv64_remote_fence_i();
    }

    fn flush_icache_range(_start: VirtAddr, _len: usize) {
        rv64_fence_i();
    }

    fn dcache_clean_range(_start: PhysAddr, _len: usize) {}

    fn dcache_invalidate_range(_start: PhysAddr, _len: usize) {}

    fn dcache_clean_invalidate_range(_start: PhysAddr, _len: usize) {}
}

impl DmaIf for Platform {
    const DMA_COHERENT: bool = true;

    fn phys_to_dma(paddr: PhysAddr) -> DmaAddr {
        DmaAddr(paddr.0 as u64)
    }

    fn dma_to_phys(daddr: DmaAddr) -> PhysAddr {
        PhysAddr(daddr.0 as usize)
    }

    fn sync_for_device(_paddr: PhysAddr, _len: usize, _dir: DmaDirection) {}

    fn sync_for_cpu(_paddr: PhysAddr, _len: usize, _dir: DmaDirection) {}
}
impl SmpIf for Platform {
    fn current_cpu_id() -> CpuId {
        current_cpu_id()
    }

    fn possible_cpus() -> CpuMask {
        // Use every startable hart published by the firmware/QEMU topology by
        // default. `tx.maxcpus=N` is an explicit upper bound, including
        // `tx.maxcpus=1` for a diagnostic single-core boot. The boot hart is
        // always retained even when firmware omits it from the startable mask.
        let dtb_mask = boot_static::startable_harts();
        let discovered = if dtb_mask != 0 {
            CpuMask::from_bits(dtb_mask & CpuMask::first(MAX_BOOT_CPUS).bits())
        } else {
            CpuMask::first(Self::platform_info().possible_cpu_count.min(MAX_BOOT_CPUS))
        };
        let current = current_cpu_id();
        let base = CpuMask::from_bits(discovered.bits() | CpuMask::single(current).bits());
        let requested = max_cpus_from_cmdline()
            .unwrap_or_else(|| base.count())
            .clamp(1, MAX_BOOT_CPUS);
        limit_cpus(base, requested, current)
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(ONLINE_CPUS.load(Ordering::Acquire))
    }

    fn mark_cpu_online(cpu: CpuId) {
        if cpu.0 < u64::BITS as usize {
            ONLINE_CPUS.fetch_or(1u64 << cpu.0, Ordering::AcqRel);
        }
    }

    fn boot_secondary_cpus(entry: SecondaryEntry) -> usize {
        let possible = Self::possible_cpus();
        let current = current_cpu_id();
        let mut wait_mask = CpuMask::EMPTY;

        for acked in &IPI_ACKED_CPUS {
            acked.store(0, Ordering::Release);
        }
        pmap::install_secondary_identity_bridge();
        for cpu in 0..MAX_BOOT_CPUS {
            let cpu = CpuId(cpu);
            if cpu == current || !possible.contains(cpu) {
                continue;
            }
            let error = start_secondary_hart(cpu, entry);
            if error == SBI_SUCCESS || error == SBI_ERR_ALREADY_AVAILABLE {
                wait_mask = CpuMask::from_bits(wait_mask.bits() | CpuMask::single(cpu).bits());
            }
        }
        let online = wait_for_online_secondaries(wait_mask);
        if online == wait_mask.count() {
            pmap::remove_secondary_identity_bridge();
        }

        online
    }

    fn enable_ipi_wakeups() {
        enable_supervisor_software_wakeups();
    }

    fn wait_for_interrupt_once() {
        #[cfg(target_arch = "riscv64")]
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack));
        }

        #[cfg(not(target_arch = "riscv64"))]
        core::hint::spin_loop();
    }

    fn prepare_interrupt_wait() -> tx_hal::InterruptWaitState {
        #[cfg(target_arch = "riscv64")]
        {
            let previous_sstatus: usize;
            unsafe {
                // Keep sie.SSIE/STIE/SEIE enabled, but defer trap delivery by
                // clearing the global SIE bit before the caller's final work
                // check. The RISC-V privileged specification requires WFI to
                // resume for a locally-enabled pending interrupt regardless
                // of the global interrupt-enable bit. An IPI that races the
                // check therefore remains pending instead of being handled
                // and cleared immediately before WFI.
                core::arch::asm!(
                    "csrrci {previous}, sstatus, 2",
                    previous = out(reg) previous_sstatus,
                    options(nostack)
                );
            }
            return tx_hal::InterruptWaitState::from_raw(previous_sstatus);
        }

        #[cfg(not(target_arch = "riscv64"))]
        tx_hal::InterruptWaitState::from_raw(0)
    }

    fn cancel_interrupt_wait(state: tx_hal::InterruptWaitState) {
        #[cfg(target_arch = "riscv64")]
        if state.raw() & 0x2 != 0 {
            unsafe {
                core::arch::asm!("csrsi sstatus, 2", options(nostack));
            }
        }

        #[cfg(not(target_arch = "riscv64"))]
        let _ = state;
    }

    fn wait_for_interrupt_prepared(state: tx_hal::InterruptWaitState) {
        #[cfg(target_arch = "riscv64")]
        unsafe {
            // Interrupt delivery is still globally masked here, so a wake
            // arriving after the final queue check remains pending until WFI
            // observes it. Restore the caller's SIE state only after WFI has
            // returned.
            core::arch::asm!("wfi", options(nostack));
            if state.raw() & 0x2 != 0 {
                core::arch::asm!("csrsi sstatus, 2", options(nostack));
            }
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            let _ = state;
            core::hint::spin_loop();
        }
    }

    fn pending_ipi(kind: IpiKind) -> bool {
        let cpu = current_cpu_id();
        cpu.0 < IPI_PENDING.len()
            && IPI_PENDING[cpu.0].load(Ordering::Acquire) & ipi_kind_bit(kind) != 0
    }

    fn park_this_cpu() -> ! {
        enable_supervisor_software_interrupts();
        loop {
            Self::wait_for_interrupt_once();
        }
    }

    fn quiesce_this_cpu() -> ! {
        time::cancel_deadline();
        #[cfg(target_arch = "riscv64")]
        unsafe {
            // No handler may re-enter the reactor/zone runtime after the AP
            // published its shutdown acknowledgement.
            core::arch::asm!(
                "csrci sstatus, 2",
                "csrw sie, zero",
                options(nomem, nostack)
            );
        }
        loop {
            #[cfg(target_arch = "riscv64")]
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack));
            }
            #[cfg(not(target_arch = "riscv64"))]
            core::hint::spin_loop();
        }
    }

    fn send_ipi(target: CpuId, kind: IpiKind) {
        if target == current_cpu_id() {
            return;
        }
        if target.0 >= IPI_PENDING.len() {
            return;
        }
        IPI_PENDING[target.0].fetch_or(ipi_kind_bit(kind), Ordering::AcqRel);
        send_sbi_ipi(CpuMask::single(target));
    }

    fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
        let targets = remote_ipi_targets_from(mask, current_cpu_id());
        let mut bits = targets.bits();
        while bits != 0 {
            let cpu = bits.trailing_zeros() as usize;
            if cpu < IPI_PENDING.len() {
                IPI_PENDING[cpu].fetch_or(ipi_kind_bit(kind), Ordering::AcqRel);
            }
            bits &= bits - 1;
        }
        send_sbi_ipi(targets);
    }

    fn ack_ipi(kind: IpiKind) {
        let cpu = current_cpu_id();
        if cpu.0 >= IPI_PENDING.len() {
            return;
        }
        IPI_PENDING[cpu.0].fetch_and(!ipi_kind_bit(kind), Ordering::AcqRel);
        mark_ipi_ack(cpu, kind);
        if IPI_PENDING[cpu.0].load(Ordering::Acquire) == 0 {
            clear_supervisor_software_interrupt();
            // A sender can publish a different kind between the zero check
            // and the hardware clear. Re-observe after the clear and recreate
            // the edge locally if that happened. This closes the lost-SSIP
            // race without taking a lock in interrupt context.
            if IPI_PENDING[cpu.0].load(Ordering::Acquire) != 0 {
                send_sbi_ipi(CpuMask::single(cpu));
            }
        }
    }

    fn clear_ipi_ack_cpus(kind: IpiKind, mask: CpuMask) {
        IPI_ACKED_CPUS[ipi_kind_index(kind)].fetch_and(!mask.bits(), Ordering::AcqRel);
    }

    fn ipi_ack_cpus(kind: IpiKind) -> CpuMask {
        CpuMask::from_bits(IPI_ACKED_CPUS[ipi_kind_index(kind)].load(Ordering::Acquire))
    }

    fn wait_for_ipi_ack_cpus(mask: CpuMask, kind: IpiKind) -> usize {
        let target = mask.bits();
        if target == 0 {
            return 0;
        }

        // QEMU TCG does not give every vCPU an equal host timeslice. A fixed
        // spin count can expire before the last otherwise-healthy AP runs,
        // which made the 8-hart boot smoke intermittently report 6/7 acks.
        // Use the same architectural timebase discipline as secondary boot.
        let start_ns = time::read_ns(<Platform as TimeIf>::frequency_hz());
        let deadline_ns = start_ns.saturating_add(RV64_IPI_ACK_TIMEOUT_NS);
        loop {
            let acked = Self::ipi_ack_cpus(kind).bits() & target;
            if acked == target {
                return mask.count();
            }
            if time::read_ns(<Platform as TimeIf>::frequency_hz()) >= deadline_ns {
                return acked.count_ones() as usize;
            }
            core::hint::spin_loop();
        }
    }
}

impl PowerIf for Platform {
    fn system_off() -> ! {
        #[cfg(target_arch = "riscv64")]
        sbi_shutdown();

        loop {
            core::hint::spin_loop();
        }
    }
}

impl EntropyIf for Platform {
    /// RV64 QEMU virt entropy: mix the unprivileged `rdtime` CSR
    /// (always available, sub-µs resolution on the QEMU virt
    /// timebase) into the trait-default xorshift counter. This is
    /// not a CSPRNG, but for txKernel's current trust model — no
    /// ASLR, no untrusted input, musl SSP only — it is materially
    /// stronger than the static `[0; 16]` it replaces.
    ///
    /// The Zkr `seed` CSR (CSR 0x015) was considered but skipped:
    /// `riscv64gc` does not include Zkr, and a runtime probe would
    /// hook the trap shell. `rdtime` ships cleanly today; a Zkr
    /// upgrade can land later behind a board-config flag.
    fn fill_random(out: &mut [u8]) {
        // Mix rdtime ticks (per-exec varying) with a per-call
        // xorshift counter so back-to-back execs at the same tick
        // still diverge.
        use core::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0xA5A5_5A5A_DEAD_BEEF);

        let ticks: u64 = read_rdtime_ticks();
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut s = ticks ^ counter.rotate_left(13);
        // Avoid the all-zero xorshift fixed point.
        if s == 0 {
            s = 0xDEAD_BEEF_CAFE_F00D;
        }
        for byte in out.iter_mut() {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *byte = (s & 0xff) as u8;
        }
    }
}

// ── Observation ring backing ──────────────────────────────────────────────────
//
// rv64-qemu reserves a static aligned buffer in .bss to back hart 0's
// observation ring. The buffer's lifetime is the kernel's lifetime, so the
// `'static` requirement in `RingDescriptor` is satisfied.
//
// Sizing: 2 MiB per hart → 208-byte ring header + 26k × 80-byte slots.
// The ring is circular; once full, oldest slots are overwritten. Full
// libcbench mm/io/pthread diagnostic runs emit much more than the older
// basic-musl smoke trace, so keep enough backing for an end-of-window
// bracketed dump instead of relying on early threshold shutdown.
//
// Sizing budget: rv64-qemu OSComp boots with 1 GiB of guest RAM (`-m 1G`);
// the 4 x 2 MiB ring set keeps the high-kernel bootstrap alias within its
// current 16 MiB boot mapping while avoiding trace wrap for diagnostic
// captures of the current libcbench subset.
const OBS_RING_BYTES: usize = 2 * 1024 * 1024;
const OBS_RING_HARTS: usize = 4;

/// Aligned static backing for the observation ring. `#[repr(C, align(64))]`
/// ensures cache-line alignment for the SPSC head/tail atomics that live in
/// the `TxTraceHartRing` header.
#[repr(C, align(64))]
struct ObsRingBuf([u8; OBS_RING_BYTES]);

#[no_mangle]
#[cfg_attr(target_arch = "riscv64", link_section = ".bss.observe_rings")]
static mut TX_OBSERVE_RINGS: [ObsRingBuf; OBS_RING_HARTS] =
    [const { ObsRingBuf([0u8; OBS_RING_BYTES]) }; OBS_RING_HARTS];

impl ObserverIf for Platform {
    fn observation_ring(hart: CpuId) -> Option<tx_hal::RingDescriptor> {
        let idx = hart.0;
        if idx >= OBS_RING_HARTS {
            return None;
        }
        // SAFETY: `TX_OBSERVE_RINGS[idx]` is a static buffer with the kernel's
        // lifetime. The SPSC discipline in `tx-observe` guarantees that
        // only the owning hart writes to its slot; readers (the daemon
        // or the serial-dump path) read after the producer has stopped.
        // `OBS_RING_BYTES` is a power of two (64 KiB = 2^16), satisfying
        // the `size` invariant on `RingDescriptor`.
        let ptr = unsafe { TX_OBSERVE_RINGS[idx].0.as_mut_ptr() };
        Some(tx_hal::RingDescriptor {
            base: core::ptr::NonNull::new(ptr).expect("OBS_RINGS slice has non-null base"),
            size: OBS_RING_BYTES,
            doorbell: None,
        })
    }

    fn clock_shared() -> bool {
        // QEMU virt `time` CSR is a single counter exposed identically
        // across harts — cross-hart ordering is trustworthy without
        // calibration.
        true
    }
}

/// Read the raw bytes of hart `hart`'s observation ring backing region.
///
/// Used by the serial-dump path (`tx_observe::dump_console_hex`) at
/// shutdown to emit the full ring contents as a hex-framed blob over
/// the console, where `cargo xtask observe extract` can recover it.
///
/// Returns `None` if the hart index is out of range.
#[allow(static_mut_refs)]
pub fn obs_ring_bytes(hart: CpuId) -> Option<&'static [u8]> {
    let idx = hart.0;
    if idx >= OBS_RING_HARTS {
        return None;
    }
    // SAFETY: the kernel calls this only after the trace-producing
    // workload has reached its observation-quiescence point for the selected
    // hart. Live host drain reads the same backing while the guest runs.
    unsafe { Some(&TX_OBSERVE_RINGS[idx].0[..]) }
}

#[cfg(target_arch = "riscv64")]
fn read_rdtime_ticks() -> u64 {
    let ticks: u64;
    unsafe {
        core::arch::asm!(
            "rdtime {ticks}",
            ticks = out(reg) ticks,
            options(nomem, nostack)
        );
    }
    ticks
}

#[cfg(not(target_arch = "riscv64"))]
fn read_rdtime_ticks() -> u64 {
    // Host-test fallback: return 0 so the trait default counter
    // alone provides variance. The host test
    // `entropy_fill_random_distinct_calls_diverge` exercises this
    // path.
    0
}

fn current_cpu_id() -> CpuId {
    let kernel_tls = read_kernel_tls();
    cpu_id_from_kernel_tls(kernel_tls).unwrap_or({
        if kernel_tls < MAX_BOOT_CPUS {
            CpuId(kernel_tls)
        } else {
            CpuId(0)
        }
    })
}

fn install_early_percpu(cpu_id: CpuId) {
    let kernel_tls = percpu_tls_for_cpu(cpu_id).unwrap_or(cpu_id.0);
    write_kernel_tls(kernel_tls);

    // Slice 2 boot-time sscratch primer (one-shot per hart). The
    // trap-vector prologue does `csrrw sp, sscratch, sp` to swap
    // onto the per-CPU trap-handler stack; sscratch must therefore
    // be primed before any trap can fire on this hart. We are
    // called from `tx_hal::entry()` on every hart's boot path,
    // immediately after the trap vector is installed, which is
    // the earliest moment we have a valid `cpu_id` and a populated
    // trap-stack array. Subsequent traps re-prime sscratch through
    // the CSR-swap discipline (trap-vector epilogue +
    // userspace-entry shim + reschedule-longjmp helper); this is
    // genuinely one-shot.
    //
    // Note: this also runs on the host build target via the trait
    // impl, but the asm is gated on `target_arch = "riscv64"`.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        let trap_stack_top = trap_stack_top_for_cpu(cpu_id);
        RV64_PERCPU_AREAS[cpu_id.0]
            .trap_stack_top
            .store(trap_stack_top, Ordering::Release);
        core::arch::asm!("csrw sscratch, {top}", top = in(reg) trap_stack_top);
    }

    // Enable user-mode access to the `time` CSR via `scounteren`.
    // Required for the high-resolution vDSO clock: user-space needs
    // to be able to `rdtime` without trapping into the kernel.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        const SCOUNTEN_TM: usize = 1 << 1; // TM = Time enable
        core::arch::asm!("csrw scounteren, {val}", val = in(reg) SCOUNTEN_TM);
    }
}

fn read_kernel_tls() -> usize {
    #[cfg(target_arch = "riscv64")]
    {
        let kernel_tls: usize;
        unsafe {
            core::arch::asm!(
                "mv {kernel_tls}, tp",
                kernel_tls = out(reg) kernel_tls,
                options(nomem, nostack)
            );
        }
        kernel_tls
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        HOST_KERNEL_TLS.load(Ordering::Acquire)
    }
}

fn write_kernel_tls(value: usize) {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("mv tp, {value}", value = in(reg) value, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        HOST_KERNEL_TLS.store(value, Ordering::Release);
    }
}

unsafe fn install_kernel_stack(top: VirtAddr) {
    record_kernel_stack_top(top);

    #[cfg(target_arch = "riscv64")]
    unsafe {
        tx_rv64_qemu_install_kernel_stack(top.0);
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = top;
}

#[cfg(target_arch = "riscv64")]
extern "C" {
    fn tx_rv64_qemu_install_kernel_stack(top: usize);
}

fn record_kernel_stack_top(top: VirtAddr) {
    if let Some(cpu) = cpu_id_from_kernel_tls(read_kernel_tls()) {
        RV64_PERCPU_AREAS[cpu.0]
            .kernel_stack_top
            .store(top.0, Ordering::Release);
    }
}

fn percpu_tls_for_cpu(cpu_id: CpuId) -> Option<usize> {
    RV64_PERCPU_AREAS
        .get(cpu_id.0)
        .map(|area| area as *const Rv64PerCpuArea as usize)
}

fn cpu_id_from_kernel_tls(kernel_tls: usize) -> Option<CpuId> {
    let base = RV64_PERCPU_AREAS.as_ptr() as usize;
    let stride = core::mem::size_of::<Rv64PerCpuArea>();
    let end = base.checked_add(stride.checked_mul(RV64_PERCPU_AREAS.len())?)?;

    if kernel_tls < base || kernel_tls >= end {
        return None;
    }

    // `install_early_percpu` only ever writes `&RV64_PERCPU_AREAS[i]` into
    // tp, so an in-range `kernel_tls` points exactly at one per-CPU area and
    // its `cpu_id` is the first field. Read it directly rather than recovering
    // the index with a divide+modulo by the non-power-of-two stride: this leaf
    // is on every `current_cpu_id()` call (~3.6% of fork-path PC samples, and
    // inlined into many hot callers). The range check above is the only
    // validation needed — it is load-bearing for the early-boot window where
    // tp still holds the raw cpu_id (handled by `current_cpu_id`'s fallback).
    let area = unsafe { &*(kernel_tls as *const Rv64PerCpuArea) };
    Some(area.cpu_id())
}

fn installed_irq_table() -> Option<&'static IrqDispatchTable> {
    let ptr = INSTALLED_IRQ_TABLE.load(Ordering::Acquire);
    NonNull::new(ptr).map(|ptr| unsafe { ptr.as_ref() })
}

fn valid_plic_irq(irq: u32) -> bool {
    irq != 0 && irq < PLIC_MAX_IRQ
}

/// Parse `tx.maxcpus=N` from the boot cmdline. Boards without a cmdline
/// (host tests, missing chosen node) get `None`, so the firmware topology is
/// used without an additional cap.
///
/// The VF2 U-Boot control FDT MISDESCRIBES hart0 (claims
/// u74-mc + mmu-type sv39 + status okay for what is physically an
/// MMU-less S7 monitor core — verified with `fdt print /cpus/cpu@0`
/// on the board, 2026-07-02). Real-board boot commands should therefore keep
/// passing an explicit `tx.maxcpus` policy; QEMU's generated FDT is the
/// authoritative default for the final SMP lane.
fn max_cpus_from_cmdline() -> Option<usize> {
    let cmdline = BootStaticBag::<IdentityDropped>::global_ref()
        .boot_info_ref()
        .cmdline?;
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("tx.maxcpus=") {
            return value.parse().ok();
        }
    }
    None
}

/// Keep at most `limit` cpus from `base`, always retaining `keep`
/// (the boot hart), then lowest hart ids first.
fn limit_cpus(base: CpuMask, limit: usize, keep: CpuId) -> CpuMask {
    let mut bits = 0u64;
    let mut taken = 0usize;
    if base.contains(keep) {
        bits |= CpuMask::single(keep).bits();
        taken = 1;
    }
    for cpu in 0..u64::BITS as usize {
        if taken >= limit {
            break;
        }
        let cpu = CpuId(cpu);
        if cpu == keep || !base.contains(cpu) {
            continue;
        }
        bits |= CpuMask::single(cpu).bits();
        taken += 1;
    }
    CpuMask::from_bits(bits)
}

/// Hardware-implemented `satp.ASID` width in bits, probed once via
/// Linux's boot trick: write all-ones into the WARL ASID field, read
/// back, count surviving bits (`usize::MAX` = not probed yet).
///
/// Board reality (2026-07-03, `ls`-loop root cause): the VF2's
/// JH7110 U74 implements **zero** ASID bits — hardware truncates
/// every satp ASID write, so ALL address spaces share hardware tag 0
/// and the "ASID-tagged TLB entries need no fence on address-space
/// switch" fast path is physically void there: a parent shell's
/// stale read-only TLB entry stays live for the forked child at the
/// same VA, and the child's COW store faults forever (asid-qualified
/// sfences can't name the truncated tag either). QEMU implements the
/// full 16 bits, which hid all of this. When the implemented width
/// cannot represent `pmap::ASID_CAPACITY`, we degrade: satp always
/// carries ASID 0, every address-space switch issues a full local
/// `sfence.vma`, and per-VA shootdowns flush across all ASIDs — the
/// scheme Chronix/Del0n1x use unconditionally on this board.
static HW_ASID_BITS: AtomicUsize = AtomicUsize::new(usize::MAX);

fn hw_asid_bits() -> usize {
    let cached = HW_ASID_BITS.load(Ordering::Relaxed);
    if cached != usize::MAX {
        return cached;
    }
    let mut probed = probe_hw_asid_bits();
    // Debug knob: `tx.pmap.asid-bits=N` caps the detected width so the
    // zero-ASID degrade path (VF2 U74 reality) can be exercised and
    // debugged under QEMU, which implements the full 16 bits.
    if let Some(forced) = asid_bits_cap_from_cmdline() {
        probed = probed.min(forced);
    }
    HW_ASID_BITS.store(probed, Ordering::Relaxed);
    #[cfg(target_arch = "riscv64")]
    {
        trap::console_write_literal(b"txkernel:pmap:asid-bits=0x");
        trap::console_write_hex(probed);
        trap::console_write_literal(b"\n");
    }
    probed
}

#[cfg(target_arch = "riscv64")]
fn probe_hw_asid_bits() -> usize {
    unsafe {
        let orig: usize;
        core::arch::asm!("csrr {0}, satp", out(reg) orig, options(nomem, nostack));
        let probe = orig | (0xFFFFusize << 44);
        let read: usize;
        core::arch::asm!("csrw satp, {0}", in(reg) probe, options(nostack));
        core::arch::asm!("csrr {0}, satp", out(reg) read, options(nomem, nostack));
        core::arch::asm!("csrw satp, {0}", in(reg) orig, options(nostack));
        core::arch::asm!("sfence.vma", options(nostack));
        ((read >> 44) & 0xFFFF).count_ones() as usize
    }
}

#[cfg(not(target_arch = "riscv64"))]
fn probe_hw_asid_bits() -> usize {
    // Host builds have no satp; report the full RISC-V field width so
    // host tests exercise the (QEMU-equivalent) tagged fast path.
    16
}

/// True when the hardware ASID width can uniquely tag our whole
/// software ASID space; false = degrade to flush-on-switch.
pub(crate) fn hw_asid_tagging_usable() -> bool {
    hw_asid_bits() >= pmap::ASID_CAPACITY.trailing_zeros() as usize
}

fn asid_bits_cap_from_cmdline() -> Option<usize> {
    let cmdline = BootStaticBag::<IdentityDropped>::global_ref()
        .boot_info_ref()
        .cmdline?;
    for token in cmdline.split_whitespace() {
        if let Some(value) = token.strip_prefix("tx.pmap.asid-bits=") {
            return value.parse().ok();
        }
    }
    None
}

fn current_plic_context() -> usize {
    plic_context_for_cpu(current_cpu_id())
}

fn plic_context_for_cpu(cpu: CpuId) -> usize {
    if let Some(context) = boot_static::plic_scontext_for_hart(cpu.0) {
        return context as usize;
    }
    cpu.0.saturating_mul(2).saturating_add(1)
}

fn plic_priority_offset(irq: u32) -> usize {
    PLIC_PRIORITY_BASE + irq as usize * core::mem::size_of::<u32>()
}

fn plic_enable_word_offset(context: usize, word: usize) -> usize {
    PLIC_ENABLE_BASE + context * PLIC_ENABLE_CONTEXT_STRIDE + word * core::mem::size_of::<u32>()
}

fn plic_claim_complete_offset(context: usize) -> usize {
    PLIC_CONTEXT_BASE + context * PLIC_CONTEXT_STRIDE + PLIC_CLAIM_COMPLETE
}

fn plic_set_priority(irq: u32, priority: u8) {
    plic_write_u32(plic_priority_offset(irq), u32::from(priority));
}

fn plic_claim(context: usize) -> u32 {
    plic_read_u32(plic_claim_complete_offset(context))
}

fn plic_complete(context: usize, irq: u32) {
    plic_write_u32(plic_claim_complete_offset(context), irq);
}

fn plic_set_enabled(context: usize, irq: u32, enabled: bool) {
    let word = irq as usize / u32::BITS as usize;
    let bit = irq as usize % u32::BITS as usize;
    let offset = plic_enable_word_offset(context, word);
    let mask = 1u32 << bit;
    let current = plic_read_u32(offset);
    let next = if enabled {
        current | mask
    } else {
        current & !mask
    };
    plic_write_u32(offset, next);
}

fn split_u64(value: u64) -> (u32, u32) {
    (value as u32, (value >> 32) as u32)
}

fn join_u64(low: u32, high: u32) -> u64 {
    u64::from(low) | (u64::from(high) << 32)
}

fn goldfish_rtc_read_time_ns() -> u64 {
    let low = goldfish_rtc_read_u32(GOLDFISH_RTC_TIME_LOW);
    let high = goldfish_rtc_read_u32(GOLDFISH_RTC_TIME_HIGH);
    join_u64(low, high)
}

fn goldfish_rtc_write_time_ns(ns: u64) {
    let (low, high) = split_u64(ns);
    goldfish_rtc_write_u32(GOLDFISH_RTC_TIME_HIGH, high);
    goldfish_rtc_write_u32(GOLDFISH_RTC_TIME_LOW, low);
}

fn goldfish_rtc_program_alarm_ns(ns: u64) {
    let (low, high) = split_u64(ns);
    goldfish_rtc_write_u32(GOLDFISH_RTC_ALARM_HIGH, high);
    goldfish_rtc_write_u32(GOLDFISH_RTC_ALARM_LOW, low);
    goldfish_rtc_write_u32(GOLDFISH_RTC_IRQ_ENABLED, 1);
}

fn goldfish_rtc_disable_alarm() {
    goldfish_rtc_write_u32(GOLDFISH_RTC_IRQ_ENABLED, 0);
    if goldfish_rtc_read_u32(GOLDFISH_RTC_ALARM_STATUS) != 0 {
        goldfish_rtc_write_u32(GOLDFISH_RTC_CLEAR_ALARM, 1);
    }
    goldfish_rtc_write_u32(GOLDFISH_RTC_CLEAR_INTERRUPT, 1);
}

fn goldfish_rtc_ack_alarm_irq() {
    goldfish_rtc_write_u32(GOLDFISH_RTC_CLEAR_INTERRUPT, 1);
}

#[cfg(target_arch = "riscv64")]
fn plic_virt_base() -> usize {
    pmap_topology::DIRECT_MAP_BASE + boot_static::plic_phys_base()
}

#[cfg(target_arch = "riscv64")]
fn plic_read_u32(offset: usize) -> u32 {
    unsafe { ((plic_virt_base() + offset) as *const u32).read_volatile() }
}

#[cfg(target_arch = "riscv64")]
fn plic_write_u32(offset: usize, value: u32) {
    unsafe { ((plic_virt_base() + offset) as *mut u32).write_volatile(value) };
}

/// Enable the 16550 UART's "received-data-available" interrupt
/// (IER bit 0) so QEMU's UART raises PLIC IRQ 10 when stdin
/// delivers a byte; AND set `sie.SEIE` (bit 9 = supervisor
/// external interrupt enable) + `sstatus.SIE` (global) so the
/// PLIC IRQ actually reaches our trap vector. PLIC unmask alone
/// is insufficient — without SEIE the IRQ pends in mip but never
/// fires the trap.
///
/// `enable_timer_wakeups` (in `time.rs`) sets STIE separately for
/// timer interrupts. We don't share a helper because the order
/// of timer vs external IRQ enable matters for boot smoke
/// sentinels — timers come up earlier.
///
/// Idempotent: writing IER and `csrs` instructions just set the
/// same bits.
fn uart_device_info() -> Option<tx_hal::DeviceInfo> {
    BootStaticBag::<IdentityDropped>::global_ref()
        .platform_devices_ref()
        .iter()
        .copied()
        .find(|device| device.kind == tx_hal::DeviceKind::Uart)
}

#[cfg(target_arch = "riscv64")]
fn enable_uart_rx_irq() {
    const UART_IER_INDEX: usize = 1;
    const UART_IER_ERBFI: u8 = 0x01;
    let (uart_base, reg_shift, reg_io_width) = match uart_device_info() {
        Some(uart) => (
            pmap_topology::DIRECT_MAP_BASE + uart.mmio.start.0,
            uart.reg_shift as usize,
            uart.reg_io_width,
        ),
        None => (UART_BASE, 0, 1),
    };
    unsafe {
        let ier_addr = uart_base + (UART_IER_INDEX << reg_shift);
        if reg_io_width == 4 {
            (ier_addr as *mut u32).write_volatile(u32::from(UART_IER_ERBFI));
        } else {
            (ier_addr as *mut u8).write_volatile(UART_IER_ERBFI);
        }

        // sie |= SEIE (bit 9) and sstatus |= SIE (bit 1).
        let seie = 1usize << 9;
        core::arch::asm!(
            "csrs sie, {seie}",
            "csrsi sstatus, 2",
            seie = in(reg) seie,
            options(nomem, nostack)
        );
    }

    // Lower the PLIC threshold for the current hart's S-mode
    // context to 0 so any priority >= 1 IRQ is delivered. QEMU's
    // virt machine resets this register to 0, but explicit beats
    // implicit — the same value works on hardware and avoids a
    // surprise on platforms that don't zero it.
    let context = plic_context_for_cpu(current_cpu_id());
    let threshold_offset = PLIC_CONTEXT_BASE + context * PLIC_CONTEXT_STRIDE;
    plic_write_u32(threshold_offset, 0);
}

#[cfg(not(target_arch = "riscv64"))]
fn enable_uart_rx_irq() {
    // Host build: no UART hardware. Stubbed; the IrqIf impl on the
    // host platform never reaches this path under test (no real
    // dispatch-table install on host).
}

#[cfg(all(not(target_arch = "riscv64"), not(test)))]
fn plic_read_u32(_offset: usize) -> u32 {
    0
}

#[cfg(all(not(target_arch = "riscv64"), not(test)))]
fn plic_write_u32(_offset: usize, _value: u32) {}

#[cfg(all(not(target_arch = "riscv64"), test))]
fn plic_read_u32(offset: usize) -> u32 {
    HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .read_u32(offset)
}

#[cfg(all(not(target_arch = "riscv64"), test))]
fn plic_write_u32(offset: usize, value: u32) {
    HOST_PLIC_STATE
        .lock()
        .expect("host plic state")
        .write_u32(offset, value);
}

#[cfg(target_arch = "riscv64")]
fn goldfish_rtc_read_u32(offset: usize) -> u32 {
    unsafe { ((GOLDFISH_RTC_BASE + offset) as *const u32).read_volatile() }
}

#[cfg(target_arch = "riscv64")]
fn goldfish_rtc_write_u32(offset: usize, value: u32) {
    unsafe { ((GOLDFISH_RTC_BASE + offset) as *mut u32).write_volatile(value) };
}

#[cfg(all(not(target_arch = "riscv64"), not(test)))]
fn goldfish_rtc_read_u32(_offset: usize) -> u32 {
    0
}

#[cfg(all(not(target_arch = "riscv64"), not(test)))]
fn goldfish_rtc_write_u32(_offset: usize, _value: u32) {}

#[cfg(all(not(target_arch = "riscv64"), test))]
fn goldfish_rtc_read_u32(offset: usize) -> u32 {
    HOST_GOLDFISH_RTC_STATE
        .lock()
        .expect("host goldfish rtc state")
        .read_u32(offset)
}

#[cfg(all(not(target_arch = "riscv64"), test))]
fn goldfish_rtc_write_u32(offset: usize, value: u32) {
    HOST_GOLDFISH_RTC_STATE
        .lock()
        .expect("host goldfish rtc state")
        .write_u32(offset, value);
}

#[cfg(all(not(target_arch = "riscv64"), test))]
struct HostPlicState {
    priorities: [u32; PLIC_IRQ_SOURCES],
    enables: [[u32; PLIC_ENABLE_WORDS]; MAX_BOOT_CPUS * 2],
    claim_complete: [u32; MAX_BOOT_CPUS * 2],
}

#[cfg(all(not(target_arch = "riscv64"), test))]
impl HostPlicState {
    const fn new() -> Self {
        Self {
            priorities: [0; PLIC_IRQ_SOURCES],
            enables: [[0; PLIC_ENABLE_WORDS]; MAX_BOOT_CPUS * 2],
            claim_complete: [0; MAX_BOOT_CPUS * 2],
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn read_u32(&self, offset: usize) -> u32 {
        if offset < PLIC_ENABLE_BASE {
            let irq = (offset - PLIC_PRIORITY_BASE) / core::mem::size_of::<u32>();
            return self.priorities.get(irq).copied().unwrap_or(0);
        }

        if (PLIC_ENABLE_BASE..PLIC_CONTEXT_BASE).contains(&offset) {
            let rel = offset - PLIC_ENABLE_BASE;
            let context = rel / PLIC_ENABLE_CONTEXT_STRIDE;
            let word = (rel % PLIC_ENABLE_CONTEXT_STRIDE) / core::mem::size_of::<u32>();
            return self
                .enables
                .get(context)
                .and_then(|words| words.get(word))
                .copied()
                .unwrap_or(0);
        }

        if offset >= PLIC_CONTEXT_BASE {
            let rel = offset - PLIC_CONTEXT_BASE;
            let context = rel / PLIC_CONTEXT_STRIDE;
            let context_offset = rel % PLIC_CONTEXT_STRIDE;
            if context_offset == PLIC_CLAIM_COMPLETE {
                return self.claim_complete.get(context).copied().unwrap_or(0);
            }
        }

        0
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        if offset < PLIC_ENABLE_BASE {
            let irq = (offset - PLIC_PRIORITY_BASE) / core::mem::size_of::<u32>();
            if let Some(priority) = self.priorities.get_mut(irq) {
                *priority = value;
            }
            return;
        }

        if (PLIC_ENABLE_BASE..PLIC_CONTEXT_BASE).contains(&offset) {
            let rel = offset - PLIC_ENABLE_BASE;
            let context = rel / PLIC_ENABLE_CONTEXT_STRIDE;
            let word = (rel % PLIC_ENABLE_CONTEXT_STRIDE) / core::mem::size_of::<u32>();
            if let Some(enable_word) = self
                .enables
                .get_mut(context)
                .and_then(|words| words.get_mut(word))
            {
                *enable_word = value;
            }
            return;
        }

        if offset >= PLIC_CONTEXT_BASE {
            let rel = offset - PLIC_CONTEXT_BASE;
            let context = rel / PLIC_CONTEXT_STRIDE;
            let context_offset = rel % PLIC_CONTEXT_STRIDE;
            if context_offset == PLIC_CLAIM_COMPLETE {
                if let Some(claim_complete) = self.claim_complete.get_mut(context) {
                    *claim_complete = value;
                }
            }
        }
    }
}

#[cfg(all(not(target_arch = "riscv64"), test))]
static HOST_PLIC_STATE: std::sync::Mutex<HostPlicState> =
    std::sync::Mutex::new(HostPlicState::new());

#[cfg(all(not(target_arch = "riscv64"), test))]
struct HostGoldfishRtcState {
    registers: [u32; 8],
    read_offsets: [usize; 16],
    read_len: usize,
    write_offsets: [usize; 16],
    write_values: [u32; 16],
    write_len: usize,
}

#[cfg(all(not(target_arch = "riscv64"), test))]
impl HostGoldfishRtcState {
    const fn new() -> Self {
        Self {
            registers: [0; 8],
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

    fn set_time_ns(&mut self, ns: u64) {
        let (low, high) = split_u64(ns);
        self.registers[GOLDFISH_RTC_TIME_LOW / core::mem::size_of::<u32>()] = low;
        self.registers[GOLDFISH_RTC_TIME_HIGH / core::mem::size_of::<u32>()] = high;
    }

    fn set_alarm_status(&mut self, status: bool) {
        self.registers[GOLDFISH_RTC_ALARM_STATUS / core::mem::size_of::<u32>()] = u32::from(status);
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

#[cfg(all(not(target_arch = "riscv64"), test))]
static HOST_GOLDFISH_RTC_STATE: std::sync::Mutex<HostGoldfishRtcState> =
    std::sync::Mutex::new(HostGoldfishRtcState::new());

pub(crate) struct IrqContextGuard {
    depth: &'static AtomicUsize,
}

impl Drop for IrqContextGuard {
    fn drop(&mut self) {
        let previous = self
            .depth
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |depth| {
                Some(depth.saturating_sub(1))
            })
            .unwrap_or(0);
        debug_assert!(previous > 0);
    }
}

pub(crate) fn enter_irq_context() -> IrqContextGuard {
    let depth = current_irq_depth_cell();
    depth.fetch_add(1, Ordering::AcqRel);
    IrqContextGuard { depth }
}

fn irq_context_depth() -> usize {
    current_irq_depth_cell().load(Ordering::Acquire)
}

#[inline]
fn current_stack_is_trap_stack() -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        let sp: usize;
        unsafe {
            core::arch::asm!("mv {sp}, sp", sp = out(reg) sp, options(nomem, nostack));
        }
        let cpu = <Platform as SmpIf>::current_cpu_id();
        let top = trap_stack_top_for_cpu(cpu);
        return (top.saturating_sub(RV64_TRAP_STACK_SIZE)..top).contains(&sp);
    }

    #[cfg(not(target_arch = "riscv64"))]
    false
}

fn current_irq_depth_cell() -> &'static AtomicUsize {
    current_percpu_area()
        .map(|area| &area.irq_depth)
        .unwrap_or(&FALLBACK_IRQ_DEPTH)
}

fn current_percpu_area() -> Option<&'static Rv64PerCpuArea> {
    let cpu = cpu_id_from_kernel_tls(read_kernel_tls())?;
    RV64_PERCPU_AREAS.get(cpu.0)
}

fn supervisor_interrupts_enabled() -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        const RV64_SSTATUS_SIE: usize = 1 << 1;
        let sstatus: usize;
        unsafe {
            core::arch::asm!("csrr {sstatus}, sstatus", sstatus = out(reg) sstatus, options(nomem, nostack));
        }
        sstatus & RV64_SSTATUS_SIE != 0
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        true
    }
}

fn exclude_supervisor_interrupts() -> LocalExecutionGuard {
    #[cfg(target_arch = "riscv64")]
    {
        let saved: usize;
        unsafe {
            core::arch::asm!(
                "csrrci {saved}, sstatus, 2",
                saved = out(reg) saved,
                options(nostack)
            );
            LocalExecutionGuard::new(saved & (1 << 1), restore_supervisor_interrupts)
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    unsafe {
        LocalExecutionGuard::new(0, restore_supervisor_interrupts)
    }
}

unsafe fn restore_supervisor_interrupts(saved: usize) {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        if saved & (1 << 1) != 0 {
            core::arch::asm!("csrsi sstatus, 2", options(nostack));
        } else {
            core::arch::asm!("csrci sstatus, 2", options(nostack));
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = saved;
}

fn enable_supervisor_software_interrupts() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrsi sie, 2", "csrsi sstatus, 2", options(nomem, nostack));
    }
}

fn enable_supervisor_software_wakeups() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrsi sie, 2", "csrci sstatus, 2", options(nomem, nostack));
    }
}

const fn ipi_kind_index(kind: IpiKind) -> usize {
    match kind {
        IpiKind::Reschedule => 0,
        IpiKind::TlbShootdown => 1,
        IpiKind::Membarrier => 2,
        IpiKind::Maintenance => 3,
        IpiKind::Stop => 4,
    }
}

const fn ipi_kind_bit(kind: IpiKind) -> u8 {
    1u8 << ipi_kind_index(kind)
}

fn mark_ipi_ack(cpu_id: CpuId, kind: IpiKind) {
    if cpu_id.0 < u64::BITS as usize {
        IPI_ACKED_CPUS[ipi_kind_index(kind)].fetch_or(1u64 << cpu_id.0, Ordering::AcqRel);
    }
}

#[cfg(test)]
fn reset_ipi_software_state() {
    for pending in &IPI_PENDING {
        pending.store(0, Ordering::Release);
    }
    for acked in &IPI_ACKED_CPUS {
        acked.store(0, Ordering::Release);
    }
}

const SBI_SUCCESS: isize = 0;
const SBI_ERR_ALREADY_AVAILABLE: isize = -6;
const RV64_SECONDARY_BOOT_TIMEOUT_NS: u64 = 2_000_000_000;
const RV64_IPI_ACK_TIMEOUT_NS: u64 = 2_000_000_000;

fn start_secondary_hart(cpu: CpuId, entry: SecondaryEntry) -> isize {
    #[cfg(target_arch = "riscv64")]
    {
        let start_addr = boot_static::secondary_start_entry();
        sbi_hart_start(cpu.0, start_addr, entry as usize)
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        let _ = (cpu, entry);
        -2
    }
}

fn wait_for_online_secondaries(target: CpuMask) -> usize {
    let target = target.bits();
    if target == 0 {
        return 0;
    }

    // AP initialization installs per-hart substrate, observation and reactor
    // state before publishing ONLINE_CPUS. A fixed spin count expires at
    // different wall times on different QEMU hosts and used to let the BSP
    // continue with only a subset of `-smp 8` online. Use the architectural
    // monotonic timer so the wait is independent of emulation speed.
    let start_ns = time::read_ns(<Platform as TimeIf>::frequency_hz());
    let deadline_ns = start_ns.saturating_add(RV64_SECONDARY_BOOT_TIMEOUT_NS);
    loop {
        let online = ONLINE_CPUS.load(Ordering::Acquire) & target;
        if online == target {
            return online.count_ones() as usize;
        }
        if time::read_ns(<Platform as TimeIf>::frequency_hz()) >= deadline_ns {
            break;
        }
        core::hint::spin_loop();
    }
    (ONLINE_CPUS.load(Ordering::Acquire) & target).count_ones() as usize
}

fn remote_sfence_vma(invalidation: PmapInvalidation) {
    let targets = remote_sfence_targets();
    if targets.is_empty() {
        return;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let error = sbi_remote_sfence_vma(
            targets.bits(),
            0,
            invalidation.virt().0,
            invalidation.size(),
        );
        assert_eq!(error, 0, "SBI remote sfence.vma failed");
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = invalidation;
}

pub(crate) fn remote_sfence_vma_asid_batch(asid: Asid, invalidations: &[PmapInvalidation]) {
    if invalidations.is_empty() {
        return;
    }

    let targets = remote_sfence_targets_for_asid(asid);
    if targets.is_empty() {
        return;
    }

    #[cfg(target_arch = "riscv64")]
    {
        let asid_usable = hw_asid_tagging_usable();
        if !asid_usable {
            // Zero-ASID hardware cannot isolate address spaces, and the U74
            // CIP-1200 workaround requires an unqualified full fence on every
            // target hart. A range-limited SBI request would merely move the
            // local stale-translation bug to a remote hart.
            let error = sbi_remote_sfence_vma(targets.bits(), 0, 0, 0);
            assert_eq!(error, 0, "SBI remote full sfence.vma failed");
            return;
        }
        if pmap::should_flush_entire_asid(invalidations) {
            let error = sbi_remote_sfence_vma_asid(targets.bits(), 0, 0, 0, asid.0 as usize);
            assert_eq!(error, 0, "SBI remote full-ASID sfence.vma failed");
            return;
        }
        for invalidation in invalidations {
            let error = sbi_remote_sfence_vma_asid(
                targets.bits(),
                0,
                invalidation.virt().0,
                invalidation.size(),
                asid.0 as usize,
            );
            assert_eq!(error, 0, "SBI remote sfence.vma failed");
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = (asid, invalidations);
}

fn remote_sfence_targets() -> CpuMask {
    remote_sfence_targets_from(<Platform as SmpIf>::online_cpus(), current_cpu_id())
}

fn remote_sfence_targets_from(online: CpuMask, current: CpuId) -> CpuMask {
    CpuMask::from_bits(online.bits() & !CpuMask::single(current).bits())
}

fn remote_sfence_targets_for_asid(asid: Asid) -> CpuMask {
    remote_sfence_targets_for_asid_from(asid, <Platform as SmpIf>::online_cpus(), current_cpu_id())
}

fn remote_sfence_targets_for_asid_from(asid: Asid, online: CpuMask, current: CpuId) -> CpuMask {
    let cached = asid_tlb_hart_mask(asid);
    CpuMask::from_bits(cached.bits() & online.bits() & !CpuMask::single(current).bits())
}

#[derive(Clone, Copy, Debug)]
struct AsidSwitch {
    cpu: CpuId,
    previous: usize,
    next: usize,
    tracked: bool,
}

/// Publish the incoming ASID without removing the outgoing one.
///
/// The returned token records the old software state.  The caller must install
/// the new hardware root before passing it to
/// [`finish_asid_switch_on_current_cpu`].
fn begin_asid_switch_on_current_cpu(asid: Asid) -> AsidSwitch {
    let cpu = current_cpu_id();
    if (asid.0 as usize) >= ASID_RESIDENCY.len() || cpu.0 >= u64::BITS as usize {
        return AsidSwitch {
            cpu,
            previous: 0,
            next: asid.0 as usize,
            tracked: false,
        };
    }
    let next = asid.0 as usize;
    let previous = RV64_PERCPU_AREAS[cpu.0]
        .active_user_asid
        .load(Ordering::Acquire);
    ASID_RESIDENCY[next].fetch_or(1u64 << cpu.0, Ordering::AcqRel);
    // Publish possible TLB ownership before installing `satp`. A concurrent
    // unmap can now conservatively include this hart even while the hardware
    // switch is in progress.
    ASID_TLB_HARTS[next].fetch_or(1u64 << cpu.0, Ordering::AcqRel);
    AsidSwitch {
        cpu,
        previous,
        next,
        tracked: true,
    }
}

/// Publish completion after `satp` no longer names the outgoing root.
fn finish_asid_switch_on_current_cpu(switch: AsidSwitch) {
    if !switch.tracked {
        return;
    }
    RV64_PERCPU_AREAS[switch.cpu.0]
        .active_user_asid
        .store(switch.next, Ordering::Release);
    if switch.previous != 0
        && switch.previous != switch.next
        && switch.previous < ASID_RESIDENCY.len()
    {
        ASID_RESIDENCY[switch.previous].fetch_and(!(1u64 << switch.cpu.0), Ordering::AcqRel);
    }
}

/// Complete a software-only transition for host tests and boot helpers.
///
/// Real pmap activation uses begin/switch/finish explicitly.
#[cfg(test)]
fn mark_asid_resident_on_current_cpu(asid: Asid) {
    let switch = begin_asid_switch_on_current_cpu(asid);
    finish_asid_switch_on_current_cpu(switch);
}

/// Leave the current user root in the only safe order:
///
/// 1. install the permanent bootstrap kernel root,
/// 2. invalidate local translations,
/// 3. publish that this hart is no longer resident in the software ASID.
///
/// Clearing residency before changing `satp` lets a concurrent root destroy
/// free page-table pages that this hart can still walk.
pub(crate) fn deactivate_current_user_pmap() {
    let cpu = current_cpu_id();
    if cpu.0 >= RV64_PERCPU_AREAS.len() || cpu.0 >= u64::BITS as usize {
        return;
    }
    let asid = RV64_PERCPU_AREAS[cpu.0]
        .active_user_asid
        .load(Ordering::Acquire);
    if asid == 0 || asid >= ASID_RESIDENCY.len() {
        return;
    }

    #[cfg(target_arch = "riscv64")]
    unsafe {
        const SATP_MODE_SV39: usize = 0x8 << 60;
        let bootstrap_root = BootStaticBag::<IdentityDropped>::global_ref().bootstrap_root_phys();
        let bootstrap_satp = SATP_MODE_SV39 | (bootstrap_root.0 >> 12);
        core::arch::asm!("csrw satp, {satp}", satp = in(reg) bootstrap_satp, options(nostack));
        // With usable ASID tags, changing from the process ASID to bootstrap
        // ASID 0 does not require discarding either address space's cached
        // translations. ASID_TLB_HARTS retains the old ownership bit so a
        // later unmap/root teardown still targets this hart. Zero-ASID U74
        // hardware has no isolation and must retain the conservative full
        // fence on every root switch.
        if !hw_asid_tagging_usable() {
            core::arch::asm!("sfence.vma", options(nostack));
        }
    }

    RV64_PERCPU_AREAS[cpu.0]
        .active_user_asid
        .store(0, Ordering::Release);
    ASID_RESIDENCY[asid].fetch_and(!(1u64 << cpu.0), Ordering::AcqRel);
}

fn asid_residency_mask(asid: Asid) -> CpuMask {
    if (asid.0 as usize) >= ASID_RESIDENCY.len() {
        return CpuMask::EMPTY;
    }
    CpuMask::from_bits(ASID_RESIDENCY[asid.0 as usize].load(Ordering::Acquire))
}

fn asid_tlb_hart_mask(asid: Asid) -> CpuMask {
    if (asid.0 as usize) >= ASID_TLB_HARTS.len() {
        return CpuMask::EMPTY;
    }
    CpuMask::from_bits(ASID_TLB_HARTS[asid.0 as usize].load(Ordering::Acquire))
}

#[cfg(test)]
pub(crate) fn clear_asid_residency(asid: Asid) {
    if (asid.0 as usize) < ASID_RESIDENCY.len() {
        ASID_RESIDENCY[asid.0 as usize].store(0, Ordering::Release);
        ASID_TLB_HARTS[asid.0 as usize].store(0, Ordering::Release);
    }
}

/// Invalidate one ASID on every online hart before its numeric value is
/// returned to the allocator.
///
/// Current residency is intentionally not used as a filter: a hart that
/// switched away can retain tagged TLB entries until an explicit fence.
pub(crate) fn invalidate_asid_on_all_harts(asid: Asid) {
    #[cfg(target_arch = "riscv64")]
    {
        let asid_usable = hw_asid_tagging_usable();
        unsafe {
            if asid_usable {
                core::arch::asm!(
                    "sfence.vma x0, {asid}",
                    asid = in(reg) asid.0 as usize,
                    options(nostack)
                );
            } else {
                core::arch::asm!("sfence.vma", options(nostack));
            }
        }

        let targets = remote_sfence_targets();
        if !targets.is_empty() {
            let error = if asid_usable {
                // OpenSBI defines start=0,size=0 as a full ASID-scoped
                // invalidation on every target hart.
                sbi_remote_sfence_vma_asid(targets.bits(), 0, 0, 0, asid.0 as usize)
            } else {
                sbi_remote_sfence_vma(targets.bits(), 0, 0, 0)
            };
            assert_eq!(error, 0, "SBI global ASID invalidation failed");
        }

        // This function is called only after residency reached zero. Once the
        // local and remote full-ASID fences complete, no hart can retain a
        // translation under this numeric tag, so reuse starts with an empty
        // history mask.
        ASID_TLB_HARTS[asid.0 as usize].store(0, Ordering::Release);
    }

    #[cfg(not(target_arch = "riscv64"))]
    if (asid.0 as usize) < ASID_TLB_HARTS.len() {
        ASID_TLB_HARTS[asid.0 as usize].store(0, Ordering::Release);
    }
}

/// Wait until no hart can page-walk through this address-space root.
///
/// Callers must first stop all threads that can re-enter the address space.
/// The acquire loop pairs with `deactivate_current_user_pmap`'s release-side
/// publication and deliberately has no unsafe timeout: freeing a still-live
/// root is worse than exposing a lifecycle bug as a hang.
pub(crate) fn wait_for_asid_quiescence(asid: Asid) {
    while !asid_residency_mask(asid).is_empty() {
        core::hint::spin_loop();
    }
}

#[cfg(any(target_arch = "riscv64", test))]
fn current_satp_asid() -> Asid {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        let satp: usize;
        core::arch::asm!("csrr {satp}, satp", satp = out(reg) satp, options(nostack));
        return Asid(((satp >> 44) & 0xffff) as u16);
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        Asid(0)
    }
}

fn send_sbi_ipi(mask: CpuMask) {
    let mask = mask.bits();
    if mask == 0 {
        return;
    }

    #[cfg(test)]
    TEST_IPI_TRANSPORT_MASK.fetch_or(mask, Ordering::AcqRel);

    #[cfg(target_arch = "riscv64")]
    {
        let _ = sbi_send_ipi(mask, 0);
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = mask;
}

fn remote_ipi_targets_from(mask: CpuMask, current: CpuId) -> CpuMask {
    mask.without(current)
}

fn rv64_fence_all() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence iorw, iorw", "fence.i", options(nostack));
    }
}

fn rv64_fence_i() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence.i", options(nomem, nostack));
    }
}

fn rv64_remote_fence_i() {
    rv64_fence_i();

    #[cfg(target_arch = "riscv64")]
    {
        let targets = remote_sfence_targets();
        if !targets.is_empty() {
            let error = sbi_remote_fence_i(targets.bits(), 0);
            assert_eq!(error, 0, "SBI remote fence.i failed");
        }
    }
}

fn clear_supervisor_software_interrupt() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrci sip, 2", options(nomem, nostack));
    }
}

impl BootStaticBag<IdentityLive> {
    fn publish_boot_info_before_identity_drop(mut self, firmware_arg: usize) -> Self {
        debug_assert_eq!(self.firmware_dtb().addr(), firmware_arg);
        unsafe {
            self.publish_boot_info_from_fdt();
        }
        self
    }

    unsafe fn publish_boot_info_from_fdt(&mut self) {
        let dtb_addr = self.firmware_dtb().parse_addr();
        let memory_regions = unsafe { self.memory_regions_mut() };
        let cmdline = unsafe { self.cmdline_mut() };
        memory_regions.fill(reserved_region());
        cmdline.fill(0);

        let parsed = parse_boot_info_from_fdt(dtb_addr, memory_regions, cmdline);

        let devices = unsafe { self.platform_devices_mut() };
        let device_count = unsafe { dtb::parse_devices_from_fdt(dtb_addr, &mut devices[..]) };
        self.publish_platform_device_count(device_count);

        if let Some(plic) = devices[..device_count]
            .iter()
            .find(|device| device.kind == tx_hal::DeviceKind::IntController)
        {
            self.publish_plic_phys_base(plic.mmio.start.0);
        }
        let scontexts = unsafe { self.plic_scontexts_mut() };
        unsafe { dtb::parse_plic_scontexts_from_fdt(dtb_addr, scontexts) };
        let (memory_region_count, initrd, cmdline_len, timebase_frequency_hz, possible_cpu_count) =
            if let Some(parsed) = parsed {
                (
                    parsed.memory_region_count,
                    parsed.initrd,
                    parsed.cmdline_len.min(CMDLINE_CAPACITY),
                    parsed
                        .timebase_frequency_hz
                        .unwrap_or(time::QEMU_VIRT_FALLBACK_TIMEBASE_HZ),
                    parsed.possible_cpu_count,
                )
            } else {
                memory_regions[0] = MemoryRegion {
                    base: PhysAddr(QEMU_VIRT_RAM_BASE),
                    size: QEMU_VIRT_FALLBACK_RAM_SIZE,
                    kind: MemoryRegionKind::Usable,
                };
                (1, None, 0, time::QEMU_VIRT_FALLBACK_TIMEBASE_HZ, 1)
            };
        self.publish_timebase_frequency_hz(timebase_frequency_hz);
        self.publish_possible_cpu_count(possible_cpu_count);
        if let Some(parsed) = parsed {
            self.publish_startable_harts(parsed.startable_harts);
        }

        let reserved_count = unsafe {
            dtb::parse_reserved_regions_from_fdt(
                dtb_addr,
                &mut memory_regions[memory_region_count..],
            )
        };
        let memory_region_count = memory_region_count + reserved_count;

        if let Some(lowest) = memory_regions[..memory_region_count]
            .iter()
            .map(|region| region.base.0)
            .min()
        {
            let _ = pmap::cover_direct_map_low_from_bag(self, PhysAddr(lowest));
        }

        let memory_region_count =
            reserve_firmware_loader_region(memory_regions, memory_region_count);

        let cmdline = if cmdline_len > 0 {
            Some(core::str::from_utf8_unchecked(&cmdline[..cmdline_len]))
        } else {
            None
        };
        let memory_regions =
            core::slice::from_raw_parts(memory_regions.as_ptr(), memory_region_count);

        *unsafe { self.boot_info_mut() } = BootInfo {
            memory_regions,
            kernel_image: self.kernel_image_phys(),
            initrd,
            cmdline,
        };
    }
}

fn reserve_firmware_loader_region(
    memory_regions: &mut [MemoryRegion],
    memory_region_count: usize,
) -> usize {
    let Some(size) = pmap_topology::QEMU_KERNEL_PHYS_BASE.checked_sub(QEMU_VIRT_RAM_BASE) else {
        return memory_region_count;
    };
    if size == 0 || memory_region_count >= memory_regions.len() {
        return memory_region_count;
    }

    memory_regions[memory_region_count] = MemoryRegion {
        base: PhysAddr(QEMU_VIRT_RAM_BASE),
        size,
        kind: MemoryRegionKind::Reserved,
    };
    memory_region_count + 1
}

#[cfg(test)]
mod tests;
