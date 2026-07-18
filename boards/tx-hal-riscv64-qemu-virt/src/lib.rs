#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

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
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask, DmaAddr,
    DmaDirection, DmaIf, EntropyIf, InitIf, IpiKind, IrqDispatchTable, IrqHandled, IrqIf,
    MemoryRegion, MemoryRegionKind, ObserverIf, PercpuIf, PhysAddr, PlatformConfig, PlatformInfo,
    PlatformInfoIf, PmapError, PmapIf, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReserveKind, PmapRoot, PmapUnmapResult, PowerIf, PtNode, PtNodeAllocator, SecondaryEntry,
    SmpIf, TimeIf, VirtAddr,
};

pub struct Platform;

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
const MAX_BOOT_CPUS: usize = 4;
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
static ONLINE_CPUS: AtomicU64 = AtomicU64::new(0);
static IPI_ACKED_CPUS: AtomicU64 = AtomicU64::new(0);
static ASID_RESIDENCY: [AtomicU64; pmap::ASID_CAPACITY] =
    [const { AtomicU64::new(0) }; pmap::ASID_CAPACITY];
static FALLBACK_IRQ_DEPTH: AtomicUsize = AtomicUsize::new(0);
static INSTALLED_IRQ_TABLE: AtomicPtr<IrqDispatchTable> = AtomicPtr::new(core::ptr::null_mut());

#[repr(C, align(64))]
pub struct Rv64PerCpuArea {
    cpu_id: usize,
    kernel_stack_top: AtomicUsize,
    irq_depth: AtomicUsize,
}

impl Rv64PerCpuArea {
    pub const fn new(cpu_id: usize) -> Self {
        Self {
            cpu_id,
            kernel_stack_top: AtomicUsize::new(0),
            irq_depth: AtomicUsize::new(0),
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

static RV64_PERCPU_AREAS: [Rv64PerCpuArea; MAX_BOOT_CPUS] = [
    Rv64PerCpuArea::new(0),
    Rv64PerCpuArea::new(1),
    Rv64PerCpuArea::new(2),
    Rv64PerCpuArea::new(3),
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
}

// These offsets are referenced by literal byte offset in the trap-vector
// asm (`TX_RV64_RCTX_SP`, `TX_RV64_RCTX_RA`, `TX_RV64_RCTX_S0`). The
// Rust constants below pin the layout from the Rust side so a struct
// reorder triggers a compile-time mismatch with the static_assert.
const KERNEL_RESUME_CTX_SP_OFFSET: usize = 0;
const KERNEL_RESUME_CTX_RA_OFFSET: usize = 8;
const KERNEL_RESUME_CTX_S0_OFFSET: usize = 16;
const _: () = assert!(core::mem::size_of::<KernelResumeCtx>() == 14 * 8);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, sp) == KERNEL_RESUME_CTX_SP_OFFSET);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, ra) == KERNEL_RESUME_CTX_RA_OFFSET);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, s) == KERNEL_RESUME_CTX_S0_OFFSET);

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

static RV64_KERNEL_RESUME_CTX: [PerHartCell<KernelResumeCtx>; MAX_BOOT_CPUS] = [
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        s: [0; 12],
    }),
];

/// Per-CPU trap-handler stack. Sized 64 KiB; the trap vector
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
/// Total static cost is `MAX_BOOT_CPUS × 64 KiB = 256 KiB`.
const RV64_TRAP_STACK_SIZE: usize = 64 * 1024;

#[repr(C, align(16))]
pub struct Rv64TrapStack(pub [u8; RV64_TRAP_STACK_SIZE]);

static RV64_TRAP_STACKS: [PerHartCell<Rv64TrapStack>; MAX_BOOT_CPUS] = [
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

/// Pointer to the local hart's [`KernelResumeCtx`]. Asm helpers
/// load/store at the documented field offsets; Rust callers in
/// `apply_trap_action` use the pointer directly.
pub fn current_kernel_resume_ctx_ptr() -> *mut KernelResumeCtx {
    let cpu = current_cpu_id();
    RV64_KERNEL_RESUME_CTX[cpu.0].as_ptr()
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
    const USER_TOP: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::SV39_USER_TOP);
    const USER_RESERVED_TOP_SIZE: usize = pmap_topology::USER_RESERVED_TOP_SIZE;
    const USER_ALLOC_TOP: tx_hal::VirtAddr = tx_hal::VirtAddr(pmap_topology::SV39_USER_ALLOC_TOP);
    const KERNEL_STACK_SIZE: usize = 128 * 1024;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 3;
    const ASID_BITS: u8 = 16;
    const CACHE_LINE_SIZE: usize = 64;
    const DMA_COHERENT: bool = true;
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::RiscvSbi;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        // firmware_arg 是 SBI 交来的 DTB(设备树)物理地址——只是个指针,
        // 真正的内存/设备信息还锁在它指向的 blob 里,必须现在就地解析。
        // 解析 DTB,把内存布局/设备等榨进静态包(capture_once 保证全局只解析一次)
        let bag = BootStaticBag::<IdentityLive>::capture_once(firmware_arg);
        // 建立引导页表:Sv39 无硬件直映射窗口,内核要在高地址跑必须软件建表
        pmap::adopt_high_linked_bootstrap_pmap(bag);

        // 把解析出的事实发布成全局 BootInfo/PlatformInfo(供上层 boot_info() 等读),
        // 并在拆除低地址恒等映射前完成收尾(IdentityLive -> IdentityDropped 类型状态机)
        BootStaticBag::<IdentityLive>::take_global()
            .publish_boot_info_before_identity_drop(firmware_arg)
            .complete_post_entry_pipeline()
            .install_global();

        // 最后才做契约默认版做的那件事:把裸值套壳成交接单返回
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

// 三个方法都一样:从那张已定案的启动登记表(BootStaticBag)里把对应字段取出来。
// 解析设备树、发布这些脏活在 boot_handoff 里干完了,这边只管读。
impl BootInfoIf for Platform {
    fn boot_info() -> &'static BootInfo {
        BootStaticBag::<IdentityDropped>::global_ref().boot_info_ref()   // 取内核初始化信息
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        BootStaticBag::<IdentityDropped>::global_ref().platform_info_ref()   // 取硬件信息
    }

    fn devices() -> &'static [tx_hal::DeviceInfo] {
        BootStaticBag::<IdentityDropped>::global_ref().platform_devices_ref()   // 取设备列表
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

    /// Write `satp` to point at `root.phys()` with Sv39 mode bits and
    /// the root's ASID, then issue a local `sfence.vma`.
    ///
    /// Called from the thread runtime right before
    /// `TrapIf::enter_userspace_with_context` so user-mode fetches see
    /// the per-process pmap. Without this satp would still point at the
    /// kernel bootstrap root from the boot trampoline (which has no
    /// user mappings), and every user-mode instruction fetch would
    /// fault forever.
    fn activate_user_pmap(root: &PmapRoot) {
        let asid_usable = hw_asid_tagging_usable();
        mark_asid_resident_on_current_cpu(root.asid());
        #[cfg(target_arch = "riscv64")]
        unsafe {
            const SATP_MODE_SV39: usize = 0x8 << 60;
            let ppn = root.phys().0 >> 12;
            // Zero-/narrow-ASID hardware (VF2 U74 implements 0 bits):
            // hardware would truncate the tag anyway; write 0 so the
            // fast-path compare below stays meaningful (a readback of
            // a truncated field would otherwise never equal our
            // computed satp and force the slow path every entry).
            let asid = if asid_usable {
                root.asid().0 as usize
            } else {
                0
            };
            let satp = SATP_MODE_SV39 | (asid << 44) | ppn;
            // Fast path: returning to the same address space (the common
            // syscall return). No CSR write, no fence — TLB entries for
            // this root are still valid (per-ASID on tagged hardware; on
            // degraded hardware the flush-on-switch below guarantees the
            // TLB only ever holds the current space's entries).
            let current: usize;
            core::arch::asm!("csrr {satp}, satp", satp = out(reg) current, options(nomem, nostack));
            if current == satp {
                return;
            }
            // Different root: write satp. On ASID-tagged hardware (QEMU:
            // 16 bits) no fence is needed per the privileged spec:
            //  - TLB entries are ASID-tagged; switching ASIDs needs no fence.
            //  - Invalid (V=0) PTEs are never cached, so invalid→valid map
            //    commits are picked up by the next hardware walk unfenced
            //    (`fill_mapping` fences only when overwriting a valid PTE).
            //  - ASID reuse is fenced at root teardown
            //    (`invalidate_root_translations` switches to the bootstrap
            //    root and issues sfence.vma there).
            // The previous unconditional `sfence.vma` here flushed the whole
            // TLB on EVERY userspace entry — under QEMU TCG that meant a
            // full tlb_flush per syscall return (~ms each), the dominant
            // term of LTP shell-test runtime (net_stress budget battle).
            core::arch::asm!(
                "csrw satp, {satp}",
                satp = in(reg) satp,
                options(nostack)
            );
            // Degraded (zero-ASID) hardware: every space shares tag 0,
            // so the previous space's entries are live for this one —
            // flush on switch (board `ls` fork/COW loop root cause,
            // 2026-07-03). QEMU never takes this branch.
            if !asid_usable {
                core::arch::asm!("sfence.vma", options(nostack));
            }
        }
        #[cfg(not(target_arch = "riscv64"))]
        let _ = (root, asid_usable);
    }
}
impl IrqIf for Platform {
    const MAX_IRQ: u32 = PLIC_MAX_IRQ;

    /// QEMU `virt` machine's 16550 UART is wired at PLIC IRQ 10.
    /// Source: `qemu/hw/riscv/virt.c::UART0_IRQ`. Fallback only —
    /// `uart_irq()` serves the device-tree value when one was probed.
    const UART_IRQ: u32 = 10;

    /// Device-tree probed UART IRQ (VisionFive 2 wires uart0 at 32,
    /// QEMU virt at 10); constant fallback when no table was parsed.
    fn uart_irq() -> u32 {
        uart_device_info()
            .and_then(|uart| uart.irq)
            .unwrap_or(Self::UART_IRQ)
    }

    fn in_irq_context() -> bool {
        irq_context_depth() != 0
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
impl TimeIf for Platform {
    fn read_ns() -> u64 {
        time::read_ns(Self::frequency_hz())
    }

    fn set_deadline_ns(deadline: u64) {
        time::set_deadline_ns(deadline, Self::frequency_hz());
    }

    fn cancel_deadline() {
        time::cancel_deadline();
    }

    fn enable_timer_wakeups() {
        time::enable_timer_wakeups();
    }

    fn frequency_hz() -> u64 {
        Self::platform_info().timebase_frequency_hz
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

    unsafe fn install_kernel_stack(top: VirtAddr) {
        unsafe { install_kernel_stack(top) };
    }
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
        // SINGLE-CORE BY DEFAULT: the userspace scheduling contract
        // is single-hart until cross-hart handoff lands, and the
        // judge lane has always run -smp 1. More cores are an
        // explicit opt-in via `tx.maxcpus=N` on the boot cmdline
        // (xtask qemu injects it to match --smp; boards simply omit
        // it). This also sidesteps firmware trees that misdescribe
        // cpu topology — the VF2 U-Boot FDT claims hart0 (physically
        // an MMU-less S7) is an S-mode-capable u74.
        let requested = max_cpus_from_cmdline().unwrap_or(1);
        if requested <= 1 {
            return CpuMask::single(current_cpu_id());
        }
        // Opt-in multi-core: DTB-derived S-mode-capable harts
        // (Linux's mmu-type + status rule), falling back to the
        // count-prefix mask, capped by the request and the boot-asm
        // stack budget. The boot hart always stays in the set.
        let dtb_mask = boot_static::startable_harts();
        let base = if dtb_mask != 0 {
            CpuMask::from_bits(dtb_mask & CpuMask::first(MAX_BOOT_CPUS).bits())
        } else {
            CpuMask::first(Self::platform_info().possible_cpu_count.min(MAX_BOOT_CPUS))
        };
        limit_cpus(base, requested, current_cpu_id())
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
        let mut started_mask = CpuMask::EMPTY;

        IPI_ACKED_CPUS.store(0, Ordering::Release);
        pmap::install_secondary_identity_bridge();
        for cpu in 0..MAX_BOOT_CPUS {
            let cpu = CpuId(cpu);
            if cpu == current || !possible.contains(cpu) {
                continue;
            }
            if start_secondary_hart(cpu, entry) {
                started_mask =
                    CpuMask::from_bits(started_mask.bits() | CpuMask::single(cpu).bits());
            }
        }
        let online = wait_for_online_secondaries(started_mask);
        pmap::remove_secondary_identity_bridge();

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

    fn pending_ipi(_kind: IpiKind) -> bool {
        supervisor_software_interrupt_pending()
    }

    fn park_this_cpu() -> ! {
        enable_supervisor_software_interrupts();
        loop {
            Self::wait_for_interrupt_once();
        }
    }

    fn send_ipi(target: CpuId, _kind: IpiKind) {
        if target == current_cpu_id() {
            return;
        }
        send_sbi_ipi(CpuMask::single(target));
    }

    fn broadcast_ipi(mask: CpuMask, _kind: IpiKind) {
        send_sbi_ipi(mask);
    }

    fn ack_ipi(_kind: IpiKind) {
        mark_ipi_ack(current_cpu_id());
        clear_supervisor_software_interrupt();
    }

    fn clear_ipi_ack_cpus(_kind: IpiKind, mask: CpuMask) {
        IPI_ACKED_CPUS.fetch_and(!mask.bits(), Ordering::AcqRel);
    }

    fn ipi_ack_cpus(_kind: IpiKind) -> CpuMask {
        CpuMask::from_bits(IPI_ACKED_CPUS.load(Ordering::Acquire))
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

        let ticks: u64 = time::read_time_ticks();
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
#[link_section = ".bss.observe_rings"]
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
    // called from `tx_kernel::kernel_main()` on every hart's boot path,
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

/// Parse `tx.maxcpus=N` from the boot cmdline. Boards without a
/// cmdline (host tests, missing chosen node) get `None` = no cap.
///
/// Rationale: the VF2 U-Boot control FDT MISDESCRIBES hart0 (claims
/// u74-mc + mmu-type sv39 + status okay for what is physically an
/// MMU-less S7 monitor core — verified with `fdt print /cpus/cpu@0`
/// on the board, 2026-07-02), so device-tree cpu filtering cannot be
/// trusted there. The cmdline knob sidesteps firmware-tree lies and
/// also enforces the single-core requirement while userspace
/// cross-hart handoff remains unfinished.
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
    // Device-tree derived S-mode context when boot resolved one (on
    // VF2 hart0 has no S context and the formula below is wrong for
    // every hart); QEMU virt formula as fallback.
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
/// First probed UART from the published device table (None on boards
/// without a table, e.g. before FDT parse or in host tests).
fn uart_device_info() -> Option<tx_hal::DeviceInfo> {
    BootStaticBag::<IdentityDropped>::global_ref()
        .platform_devices_ref()
        .iter()
        .copied()
        .find(|device| device.kind == tx_hal::DeviceKind::Uart)
}

#[cfg(target_arch = "riscv64")]
fn enable_uart_rx_irq() {
    // 16550 IER register index = 1; bit 0 = ERBFI (Enable Received
    // Data Available Interrupt). Real 16550 derivatives place it at
    // index << reg-shift with reg-io-width-sized registers: QEMU's
    // ns16550a is byte-adjacent (shift 0, width 1), VF2's dw-apb-uart
    // uses 32-bit registers at stride 4 (shift 2, width 4).
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

fn current_irq_depth_cell() -> &'static AtomicUsize {
    current_percpu_area()
        .map(|area| &area.irq_depth)
        .unwrap_or(&FALLBACK_IRQ_DEPTH)
}

fn current_percpu_area() -> Option<&'static Rv64PerCpuArea> {
    let cpu = cpu_id_from_kernel_tls(read_kernel_tls())?;
    RV64_PERCPU_AREAS.get(cpu.0)
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

fn supervisor_software_interrupt_pending() -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        let sip: usize;
        unsafe {
            core::arch::asm!("csrr {sip}, sip", sip = out(reg) sip, options(nomem, nostack));
        }
        sip & 0x2 != 0
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        false
    }
}

fn mark_ipi_ack(cpu_id: CpuId) {
    if cpu_id.0 < u64::BITS as usize {
        IPI_ACKED_CPUS.fetch_or(1u64 << cpu_id.0, Ordering::AcqRel);
    }
}

fn start_secondary_hart(cpu: CpuId, entry: SecondaryEntry) -> bool {
    #[cfg(target_arch = "riscv64")]
    {
        let start_addr = boot_static::secondary_start_entry();
        sbi_hart_start(cpu.0, start_addr, entry as usize) == 0
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        let _ = (cpu, entry);
        false
    }
}

fn wait_for_online_secondaries(target: CpuMask) -> usize {
    let target = target.bits();
    if target == 0 {
        return 0;
    }

    for _ in 0..100_000 {
        let online = ONLINE_CPUS.load(Ordering::Acquire) & target;
        if online == target {
            return online.count_ones() as usize;
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
        for invalidation in invalidations {
            let error = if asid_usable {
                sbi_remote_sfence_vma_asid(
                    targets.bits(),
                    0,
                    invalidation.virt().0,
                    invalidation.size(),
                    asid.0 as usize,
                )
            } else {
                // Zero-ASID hardware: remote harts can't match the
                // truncated tag either; use the unqualified form.
                sbi_remote_sfence_vma(
                    targets.bits(),
                    0,
                    invalidation.virt().0,
                    invalidation.size(),
                )
            };
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
    let resident = asid_residency_mask(asid);
    CpuMask::from_bits(resident.bits() & online.bits() & !CpuMask::single(current).bits())
}

fn mark_asid_resident_on_current_cpu(asid: Asid) {
    let cpu = current_cpu_id();
    if (asid.0 as usize) >= ASID_RESIDENCY.len() || cpu.0 >= u64::BITS as usize {
        return;
    }
    ASID_RESIDENCY[asid.0 as usize].fetch_or(1u64 << cpu.0, Ordering::AcqRel);
}

pub(crate) fn clear_current_asid_residency() {
    let asid = current_satp_asid();
    if asid.0 == 0 || (asid.0 as usize) >= ASID_RESIDENCY.len() {
        return;
    }
    let cpu = current_cpu_id();
    if cpu.0 >= u64::BITS as usize {
        return;
    }
    ASID_RESIDENCY[asid.0 as usize].fetch_and(!(1u64 << cpu.0), Ordering::AcqRel);
}

fn asid_residency_mask(asid: Asid) -> CpuMask {
    if (asid.0 as usize) >= ASID_RESIDENCY.len() {
        return CpuMask::EMPTY;
    }
    CpuMask::from_bits(ASID_RESIDENCY[asid.0 as usize].load(Ordering::Acquire))
}

pub(crate) fn clear_asid_residency(asid: Asid) {
    if (asid.0 as usize) < ASID_RESIDENCY.len() {
        ASID_RESIDENCY[asid.0 as usize].store(0, Ordering::Release);
    }
}

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

    #[cfg(target_arch = "riscv64")]
    {
        let _ = sbi_send_ipi(mask, 0);
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = mask;
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
        let dtb_addr = self.firmware_dtb().addr();
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

        // Firmware-reserved RAM (OpenSBI's PMP-protected home, exposed
        // via /reserved-memory + the header memreserve block) must be
        // excluded before the allocator plans metadata placement. On
        // VF2 the firmware sits at the very bottom of DDR.
        let reserved_count = unsafe {
            dtb::parse_reserved_regions_from_fdt(
                dtb_addr,
                &mut memory_regions[memory_region_count..],
            )
        };
        let memory_region_count = memory_region_count + reserved_count;

        // Boards like the VisionFive 2 report DDR from 0x4000_0000; the
        // asm boot tables only cover the QEMU gigabyte at 0x8000_0000.
        // Add the missing low direct-map leaves before anyone allocates
        // from those regions. No-op on QEMU.
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
