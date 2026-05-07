#![no_std]

#[cfg(test)]
extern crate std;

use tx_hal::{
    AllocError, Arch, ArchAuxvFacts, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf,
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask, DmaIf,
    EntropyIf, InitIf, IpiKind, IrqIf, MemoryRegion, MemoryRegionKind, MmioFlags, MmioRegion,
    PercpuIf, PhysAddr, PhysRange, PlatformConfig, PlatformInfo, PlatformInfoIf, PmapError, PmapIf,
    PmapInvalidation, PmapReservation, PmapReserveKind, PowerIf, PtNode, PtNodeAllocator,
    SecondaryEntry, SignalFrameIf, SmpIf, TimeIf, TrapClass, TrapFrameSnapshot, TrapIf,
    UserAccessIf, VirtAddr, VirtRange,
};

use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text.boot, "ax"
    .equ TX_LA64_DMW_CACHED,   0x9000000000000011
    .equ TX_LA64_DMW_UNCACHED, 0x8000000000000001
    .equ TX_LA64_CSR_DMW0, 0x180
    .equ TX_LA64_CSR_DMW1, 0x181
    .equ TX_LA64_CSR_DMW2, 0x182
    .equ TX_LA64_CSR_DMW3, 0x183
    .globl _start
_start:
    move    $s0, $a0
    move    $s1, $a1
    la.local $sp, __tx_boot_stack_top

    la.local $t0, _bss_start
    la.local $t1, _bss_end
1:
    bgeu    $t0, $t1, 2f
    st.d    $zero, $t0, 0
    addi.d  $t0, $t0, 8
    b       1b

2:
    li.d    $t0, TX_LA64_DMW_CACHED
    csrwr   $t0, TX_LA64_CSR_DMW0
    li.d    $t0, TX_LA64_DMW_UNCACHED
    csrwr   $t0, TX_LA64_CSR_DMW1
    move    $t0, $zero
    csrwr   $t0, TX_LA64_CSR_DMW2
    csrwr   $t0, TX_LA64_CSR_DMW3
    invtlb  0x0, $zero, $zero

    move    $a0, $s0
    move    $a1, $s1
    la.local $t0, rust_entry
    jirl    $zero, $t0, 0

3:
    idle    0
    b       3b

    .section .text.trap, "ax"
    .align 12
    .globl tx_la64_qemu_exception_vector
    .type tx_la64_qemu_exception_vector, @function
tx_la64_qemu_exception_vector:
    bl      tx_la64_qemu_unhandled_exception
1:
    idle    0
    b       1b
    .size tx_la64_qemu_exception_vector, . - tx_la64_qemu_exception_vector

    .align 12
    .globl tx_la64_qemu_tlb_refill_vector
    .type tx_la64_qemu_tlb_refill_vector, @function
tx_la64_qemu_tlb_refill_vector:
    bl      tx_la64_qemu_unhandled_exception
2:
    idle    0
    b       2b
    .size tx_la64_qemu_tlb_refill_vector, . - tx_la64_qemu_tlb_refill_vector
"#
);

pub struct Platform;

const QEMU_LA64_RAM_BASE: usize = 0;
const QEMU_LA64_RAM_SIZE: usize = 0x1000_0000;
const QEMU_LA64_RAM_END: usize = QEMU_LA64_RAM_BASE + QEMU_LA64_RAM_SIZE;
const QEMU_LA64_KERNEL_LOAD_BASE: usize = 0x0020_0000;
const LA64_MAX_BOOT_CPUS: usize = 1;
const LA64_DMW_CACHED_BASE: usize = 0x9000_0000_0000_0000;
const LA64_DMW_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
const LA64_PHYS_ADDR_MASK: usize = (1usize << 48) - 1;
const LA64_CSR_EENTRY: usize = 0x0c;
const LA64_CSR_CRMD: usize = 0x00;
const LA64_CSR_ECFG: usize = 0x04;
const LA64_CSR_TLBRENTRY: usize = 0x88;
const LA64_CSR_MERRENTRY: usize = 0x93;
const LA64_CSR_TCFG: usize = 0x41;
const LA64_CSR_TICLR: usize = 0x44;
const LA64_CRMD_IE: usize = 1 << 2;
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

static BOOT_FACTS_STATE: AtomicU8 = AtomicU8::new(0);
static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
static LA64_TIMEBASE_HZ: AtomicU64 = AtomicU64::new(0);
static LA64_ONLINE_CPUS: AtomicU64 = AtomicU64::new(1);
static LA64_IPI_ACKED_CPUS: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_KERNEL_TLS: AtomicUsize = AtomicUsize::new(0);

static mut BOOT_MEMORY_REGIONS: [MemoryRegion; 2] = [
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Usable,
    },
];

static mut BOOT_INFO: BootInfo = BootInfo::empty();

static mut BOOTSTRAP_PMAP_INFO: BootstrapPmapInfo = BootstrapPmapInfo {
    root: PhysAddr(0),
    mapped: PhysRange::empty(),
    direct_map_base: VirtAddr(0),
    direct_map: VirtRange::empty(),
    kernel_image: VirtRange::empty(),
    identity: None,
    pt_node_pool: PhysRange::empty(),
    reserved_page_tables: &[],
};

// QEMU loongson3-virt exposes the first serial port as an 8250-compatible
// UART at 0x1fe0_01e0; Linux examples use earlycon=uart,mmio,0x1fe001e0.
const QEMU_LA64_UART0_BASE: usize = 0x1fe0_01e0;
const QEMU_LA64_UART0_SIZE: usize = 0x100;
const QEMU_LA64_UART0_PAGE_BASE: usize = 0x1fe0_0000;
#[cfg(target_arch = "loongarch64")]
const UART_RBR: usize = 0x00;
const UART_THR: usize = 0x00;
const UART_LSR: usize = 0x05;
#[cfg(target_arch = "loongarch64")]
const UART_LSR_DR: u8 = 1 << 0;
const UART_LSR_THRE: u8 = 1 << 5;

// The early UART is reachable through QEMU's current direct/identity execution
// convention. Phase-3 substrate MMIO mapping treats this exact page as already
// covered; all non-identity requests remain unsupported until LA64 owns real
// DMW/page-table mutation.
static MMIO_REGIONS: &[MmioRegion] = &[MmioRegion {
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
}];

static PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: Platform::BOARD,
    spi_sd: None,
    mmio_regions: MMIO_REGIONS,
    timebase_frequency_hz: 0,
    possible_cpu_count: 1,
};

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::LoongArch64;
    const BOARD: &'static str = "qemu-loongarch64-virt";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = 48;
    const VIRT_ADDR_BITS: u8 = 48;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(LA64_DMW_CACHED_BASE);
    const DIRECT_MAP_SIZE: usize = QEMU_LA64_RAM_SIZE;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(la64_cached_virt(QEMU_LA64_KERNEL_LOAD_BASE));
    const KERNEL_STACK_SIZE: usize = 64 * 1024;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const CACHE_LINE_SIZE: usize = 64;
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
        ensure_static_boot_facts();

        unsafe { &*core::ptr::addr_of!(BOOT_INFO) }
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        &PLATFORM_INFO
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
impl PmapIf for Platform {
    fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
        ensure_static_boot_facts();

        unsafe { Some(&*core::ptr::addr_of!(BOOTSTRAP_PMAP_INFO)) }
    }

    fn alloc_pt_node() -> Result<PtNode, AllocError> {
        let Some(allocator) = installed_pt_node_allocator() else {
            return Err(AllocError::Exhausted);
        };

        allocator()
    }

    fn free_pt_node(node: PtNode) {
        unsafe {
            let _ = node.release_typed_frame();
        }
    }

    fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError> {
        let value = allocator as usize;
        INSTALLED_PT_NODE_ALLOCATOR
            .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| PmapError::AlreadyMapped)
    }

    fn reserve_kernel_direct_map_1g(phys: PhysAddr) -> Result<Option<PmapReservation>, PmapError> {
        if dmw_covers_phys_range(phys, PmapReserveKind::Superpage1G.size()) {
            Ok(None)
        } else {
            Err(PmapError::Unsupported)
        }
    }

    fn extend_direct_map(phys_end: PhysAddr) -> Result<(), PmapError> {
        if phys_end.0 <= QEMU_LA64_RAM_END {
            Ok(())
        } else {
            Err(PmapError::Unsupported)
        }
    }

    fn reserve_kernel_mapping(
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        if dmw_mmio_page_is_precovered(virt, phys, kind) {
            return Ok(None);
        }

        Err(PmapError::Unsupported)
    }

    fn shootdown_kernel_mapping(invalidation: PmapInvalidation) {
        la64_invtlb_global(invalidation.virt());
    }

    fn shootdown_mapping(asid: Asid, invalidation: PmapInvalidation) {
        la64_invtlb_asid(asid, invalidation.virt());
    }
}
impl TrapIf for Platform {
    fn install_minimal_trap_vector() {
        install_la64_trap_vectors();
    }

    fn install_kernel_trap_vector() {
        install_la64_trap_vectors();
    }

    fn install_user_trap_vector() {
        install_la64_trap_vectors();
    }

    fn classify_trap(snapshot: TrapFrameSnapshot) -> TrapClass {
        classify_la64_trap(snapshot.scause)
    }
}
impl UserAccessIf for Platform {}
impl SignalFrameIf for Platform {}
impl IrqIf for Platform {}
impl TimeIf for Platform {
    fn read_ns() -> u64 {
        tx_hal::time::ticks_to_ns(la64_read_stable_counter(), Self::frequency_hz())
    }

    fn set_deadline_ns(deadline: u64) {
        let frequency_hz = Self::frequency_hz();
        if frequency_hz == 0 {
            return;
        }

        let now = la64_read_stable_counter();
        let target = tx_hal::time::deadline_ns_to_ticks(deadline, frequency_hz);
        let delta = target.saturating_sub(now).max(1);
        let delta = round_up_to_tcfg_ticks(delta);

        write_la64_csr(LA64_CSR_TICLR, LA64_TICLR_CLEAR_TIMER);
        write_la64_csr(LA64_CSR_TCFG, delta as usize | LA64_TCFG_ENABLE);
    }

    fn cancel_deadline() {
        write_la64_csr(LA64_CSR_TCFG, 0);
        write_la64_csr(LA64_CSR_TICLR, LA64_TICLR_CLEAR_TIMER);
    }

    fn enable_timer_wakeups() {
        let ecfg = read_la64_csr(LA64_CSR_ECFG) | LA64_ESTAT_IS_TIMER;
        write_la64_csr(LA64_CSR_ECFG, ecfg);

        let crmd = read_la64_csr(LA64_CSR_CRMD) | LA64_CRMD_IE;
        write_la64_csr(LA64_CSR_CRMD, crmd);
    }

    fn frequency_hz() -> u64 {
        la64_timebase_frequency_hz()
    }
}
impl PercpuIf for Platform {
    fn current_cpu_id() -> CpuId {
        la64_current_cpu_id()
    }

    fn install_early_percpu(cpu_id: CpuId) {
        la64_write_kernel_tls(cpu_id.0);
    }

    fn read_kernel_tls() -> u64 {
        la64_read_kernel_tls() as u64
    }

    fn write_kernel_tls(value: u64) {
        la64_write_kernel_tls(value as usize);
    }

    unsafe fn install_kernel_stack(top: VirtAddr) {
        unsafe { la64_install_kernel_stack(top) };
    }
}
impl CacheIf for Platform {}
impl DmaIf for Platform {}
impl SmpIf for Platform {
    fn current_cpu_id() -> CpuId {
        la64_current_cpu_id()
    }

    fn possible_cpus() -> CpuMask {
        CpuMask::first(PLATFORM_INFO.possible_cpu_count.min(LA64_MAX_BOOT_CPUS))
    }

    fn online_cpus() -> CpuMask {
        CpuMask::from_bits(LA64_ONLINE_CPUS.load(Ordering::Acquire) & Self::possible_cpus().bits())
    }

    fn mark_cpu_online(cpu: CpuId) {
        if Self::possible_cpus().contains(cpu) {
            LA64_ONLINE_CPUS.fetch_or(CpuMask::single(cpu).bits(), Ordering::AcqRel);
        }
    }

    fn boot_secondary_cpus(_entry: SecondaryEntry) -> usize {
        0
    }

    fn wait_for_interrupt_once() {
        la64_wait_for_interrupt_once();
    }

    fn pending_ipi(_kind: IpiKind) -> bool {
        false
    }

    fn send_ipi(target: CpuId, _kind: IpiKind) {
        assert_eq!(target, la64_current_cpu_id());
    }

    fn broadcast_ipi(mask: CpuMask, kind: IpiKind) {
        let current = la64_current_cpu_id();
        if mask.contains(current) {
            Self::send_ipi(current, kind);
        }
    }

    fn ack_ipi(_kind: IpiKind) {
        LA64_IPI_ACKED_CPUS.fetch_or(
            CpuMask::single(la64_current_cpu_id()).bits(),
            Ordering::AcqRel,
        );
    }

    fn clear_ipi_ack_cpus(_kind: IpiKind, mask: CpuMask) {
        LA64_IPI_ACKED_CPUS.fetch_and(!mask.bits(), Ordering::AcqRel);
    }

    fn ipi_ack_cpus(_kind: IpiKind) -> CpuMask {
        CpuMask::from_bits(LA64_IPI_ACKED_CPUS.load(Ordering::Acquire))
    }
}

impl PowerIf for Platform {
    fn system_off() -> ! {
        loop {
            #[cfg(target_arch = "loongarch64")]
            unsafe {
                core::arch::asm!("idle 0", options(nomem, nostack));
            }
            core::hint::spin_loop();
        }
    }
}

/// LoongArch64 entropy: relies on the trait default
/// (deterministic xorshift-counter seed). LoongArch lacks a
/// portable unprivileged hardware RNG CSR equivalent to RV64's
/// Zkr; a future virtio-rng (or platform-specific RNG MMIO) impl
/// can override this without touching callers.
impl EntropyIf for Platform {}

fn uart_put_byte(byte: u8) {
    let base = QEMU_LA64_UART0_BASE as *mut u8;

    unsafe {
        while core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(base.add(UART_THR), byte);
    }
}

fn uart_try_get_byte() -> Option<u8> {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let base = QEMU_LA64_UART0_BASE as *const u8;
        if core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_DR == 0 {
            return None;
        }

        Some(core::ptr::read_volatile(base.add(UART_RBR)))
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        None
    }
}

const fn la64_cached_virt(phys: usize) -> usize {
    LA64_DMW_CACHED_BASE | phys
}

const fn la64_uncached_virt(phys: usize) -> usize {
    LA64_DMW_UNCACHED_BASE | phys
}

fn la64_kernel_addr_to_phys(addr: usize) -> usize {
    addr & LA64_PHYS_ADDR_MASK
}

fn dmw_covers_phys_range(start: PhysAddr, len: usize) -> bool {
    start
        .0
        .checked_add(len)
        .is_some_and(|end| end <= (LA64_PHYS_ADDR_MASK + 1))
}

fn la64_invtlb_global(virt: VirtAddr) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "invtlb 0x6, $zero, {virt}",
            virt = in(reg) virt.0,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = virt;
}

fn la64_invtlb_asid(asid: Asid, virt: VirtAddr) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!(
            "invtlb 0x5, {asid}, {virt}",
            asid = in(reg) asid.0 as usize,
            virt = in(reg) virt.0,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = (asid, virt);
}

fn la64_read_stable_counter() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        let ticks: u64;
        unsafe {
            core::arch::asm!(
                "rdtime.d {ticks}, $zero",
                ticks = out(reg) ticks,
                options(nomem, nostack)
            );
        }
        ticks
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        0
    }
}

fn la64_current_cpu_id() -> CpuId {
    let kernel_tls = la64_read_kernel_tls();
    if kernel_tls < LA64_MAX_BOOT_CPUS {
        CpuId(kernel_tls)
    } else {
        CpuId(0)
    }
}

fn la64_read_kernel_tls() -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        let value: usize;
        unsafe {
            core::arch::asm!(
                "move {value}, $r21",
                value = out(reg) value,
                options(nomem, nostack)
            );
        }
        value
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        LA64_HOST_KERNEL_TLS.load(Ordering::Acquire)
    }
}

fn la64_write_kernel_tls(value: usize) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("move $r21, {value}", value = in(reg) value, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        LA64_HOST_KERNEL_TLS.store(value, Ordering::Release);
    }
}

unsafe fn la64_install_kernel_stack(top: VirtAddr) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("move $sp, {top}", top = in(reg) top.0, options(nomem, nostack));
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = top;
}

fn la64_wait_for_interrupt_once() {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        core::arch::asm!("idle 0", options(nomem, nostack));
    }

    #[cfg(not(target_arch = "loongarch64"))]
    core::hint::spin_loop();
}

fn la64_timebase_frequency_hz() -> u64 {
    let cached = LA64_TIMEBASE_HZ.load(Ordering::Acquire);
    if cached != 0 {
        return cached;
    }

    let detected = la64_detect_timebase_frequency_hz();
    if detected != 0 {
        LA64_TIMEBASE_HZ.store(detected, Ordering::Release);
    }

    detected
}

fn la64_detect_timebase_frequency_hz() -> u64 {
    la64_cpucfg_timebase_frequency(
        read_la64_cpucfg(LA64_CPUCFG2),
        read_la64_cpucfg(LA64_CPUCFG4),
        read_la64_cpucfg(LA64_CPUCFG5),
    )
}

const fn la64_cpucfg_timebase_frequency(cpucfg2: u32, cpucfg4: u32, cpucfg5: u32) -> u64 {
    if cpucfg2 & LA64_CPUCFG2_LLFTP == 0 {
        return 0;
    }

    let base_hz = cpucfg4 as u64;
    let multiplier = (cpucfg5 & 0xffff) as u64;
    let divisor = (cpucfg5 >> 16) as u64;

    if base_hz == 0 || multiplier == 0 || divisor == 0 {
        return 0;
    }

    base_hz.saturating_mul(multiplier) / divisor
}

const fn round_up_to_tcfg_ticks(ticks: u64) -> u64 {
    let mask = LA64_TCFG_TICK_MASK as u64;
    ticks.saturating_add(3) & mask
}

fn read_la64_cpucfg(index: usize) -> u32 {
    #[cfg(target_arch = "loongarch64")]
    {
        let value: usize;
        unsafe {
            core::arch::asm!(
                "cpucfg {value}, {index}",
                value = out(reg) value,
                index = in(reg) index,
                options(nomem, nostack)
            );
        }
        value as u32
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = index;
        0
    }
}

const fn classify_la64_trap(estat: usize) -> TrapClass {
    let ecode = (estat >> LA64_ESTAT_ECODE_SHIFT) & LA64_ESTAT_ECODE_MASK;

    match ecode {
        LA64_ECODE_INT => {
            if estat & LA64_ESTAT_IS_TIMER != 0 {
                TrapClass::TimerInterrupt
            } else if estat & LA64_ESTAT_IS_IPI != 0 {
                TrapClass::InterprocessorInterrupt
            } else if estat & LA64_ESTAT_IS_HWI_MASK != 0 {
                TrapClass::ExternalInterrupt
            } else {
                TrapClass::UnknownInterrupt
            }
        }
        LA64_ECODE_PIL | LA64_ECODE_PNR | LA64_ECODE_PPI => TrapClass::PageFault {
            write: false,
            instruction: false,
        },
        LA64_ECODE_PIS | LA64_ECODE_PME => TrapClass::PageFault {
            write: true,
            instruction: false,
        },
        LA64_ECODE_PIF | LA64_ECODE_PNX => TrapClass::PageFault {
            write: false,
            instruction: true,
        },
        LA64_ECODE_ALE => TrapClass::AlignmentFault {
            write: false,
            instruction: false,
        },
        LA64_ECODE_ADEF => TrapClass::AlignmentFault {
            write: false,
            instruction: true,
        },
        LA64_ECODE_ADEM => TrapClass::AlignmentFault {
            write: false,
            instruction: false,
        },
        LA64_ECODE_SYS => TrapClass::Syscall,
        LA64_ECODE_BRK => TrapClass::Breakpoint,
        LA64_ECODE_INE | LA64_ECODE_IPE => TrapClass::IllegalInstruction,
        _ => TrapClass::UnknownSync,
    }
}

fn install_la64_trap_vectors() {
    let exception = la64_kernel_addr_to_phys(la64_exception_vector_addr());
    let tlb_refill = la64_kernel_addr_to_phys(la64_tlb_refill_vector_addr());

    write_la64_csr(LA64_CSR_EENTRY, exception);
    write_la64_csr(LA64_CSR_TLBRENTRY, tlb_refill);
    write_la64_csr(LA64_CSR_MERRENTRY, exception);
}

#[cfg(target_arch = "loongarch64")]
fn la64_exception_vector_addr() -> usize {
    unsafe extern "C" {
        fn tx_la64_qemu_exception_vector();
    }

    tx_la64_qemu_exception_vector as *const () as usize
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_exception_vector_addr() -> usize {
    0
}

#[cfg(target_arch = "loongarch64")]
fn la64_tlb_refill_vector_addr() -> usize {
    unsafe extern "C" {
        fn tx_la64_qemu_tlb_refill_vector();
    }

    tx_la64_qemu_tlb_refill_vector as *const () as usize
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_tlb_refill_vector_addr() -> usize {
    0
}

fn write_la64_csr(csr: usize, value: usize) {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        match csr {
            LA64_CSR_CRMD => {
                core::arch::asm!("csrwr {value}, 0x00", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_EENTRY => {
                core::arch::asm!("csrwr {value}, 0x0c", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_ECFG => {
                core::arch::asm!("csrwr {value}, 0x04", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_TLBRENTRY => {
                core::arch::asm!("csrwr {value}, 0x88", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_MERRENTRY => {
                core::arch::asm!("csrwr {value}, 0x93", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_TCFG => {
                core::arch::asm!("csrwr {value}, 0x41", value = in(reg) value, options(nomem, nostack));
            }
            LA64_CSR_TICLR => {
                core::arch::asm!("csrwr {value}, 0x44", value = in(reg) value, options(nomem, nostack));
            }
            _ => {}
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    let _ = (csr, value);
}

fn read_la64_csr(csr: usize) -> usize {
    #[cfg(target_arch = "loongarch64")]
    unsafe {
        let value: usize;
        match csr {
            LA64_CSR_CRMD => {
                core::arch::asm!("csrrd {value}, 0x00", value = out(reg) value, options(nomem, nostack));
                value
            }
            LA64_CSR_ECFG => {
                core::arch::asm!("csrrd {value}, 0x04", value = out(reg) value, options(nomem, nostack));
                value
            }
            _ => 0,
        }
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = csr;
        0
    }
}

#[no_mangle]
#[cfg(target_arch = "loongarch64")]
extern "C" fn tx_la64_qemu_unhandled_exception() -> ! {
    Platform::write_bytes(b"txkernel:qemu-loongarch64-virt:trap\n");
    loop {
        unsafe {
            core::arch::asm!("idle 0", options(nomem, nostack));
        }
        core::hint::spin_loop();
    }
}

fn ensure_static_boot_facts() {
    loop {
        match BOOT_FACTS_STATE.load(Ordering::Acquire) {
            2 => return,
            0 => {
                if BOOT_FACTS_STATE
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    publish_static_boot_facts();
                    BOOT_FACTS_STATE.store(2, Ordering::Release);
                    return;
                }
            }
            _ => core::hint::spin_loop(),
        }
    }
}

fn publish_static_boot_facts() {
    let kernel_image = linked_kernel_image();
    let reserved_end = align_up(
        kernel_image.end().0,
        <Platform as PlatformConfig>::PAGE_SIZE,
    )
    .min(QEMU_LA64_RAM_END);
    let usable_size = QEMU_LA64_RAM_END.saturating_sub(reserved_end);
    let direct_map = VirtRange {
        start: VirtAddr(la64_cached_virt(QEMU_LA64_RAM_BASE)),
        size: QEMU_LA64_RAM_SIZE,
    };

    unsafe {
        let regions = core::ptr::addr_of_mut!(BOOT_MEMORY_REGIONS) as *mut MemoryRegion;
        core::ptr::write(
            regions,
            MemoryRegion {
                base: PhysAddr(QEMU_LA64_RAM_BASE),
                size: reserved_end - QEMU_LA64_RAM_BASE,
                kind: MemoryRegionKind::Reserved,
            },
        );
        core::ptr::write(
            regions.add(1),
            MemoryRegion {
                base: PhysAddr(reserved_end),
                size: usable_size,
                kind: MemoryRegionKind::Usable,
            },
        );

        core::ptr::write(
            core::ptr::addr_of_mut!(BOOT_INFO),
            BootInfo {
                memory_regions: core::slice::from_raw_parts(regions, 2),
                kernel_image,
                initrd: None,
                cmdline: None,
            },
        );

        core::ptr::write(
            core::ptr::addr_of_mut!(BOOTSTRAP_PMAP_INFO),
            BootstrapPmapInfo {
                // LA64 publishes DMW-backed direct-map facts before the
                // board-owned page-table root exists. Fine-grained mapping
                // mutation stays unsupported until real pmap work lands.
                root: PhysAddr(0),
                mapped: PhysRange {
                    start: PhysAddr(QEMU_LA64_RAM_BASE),
                    size: QEMU_LA64_RAM_SIZE,
                },
                direct_map_base: VirtAddr(LA64_DMW_CACHED_BASE),
                direct_map,
                kernel_image: VirtRange {
                    start: VirtAddr(la64_cached_virt(kernel_image.start.0)),
                    size: kernel_image.size,
                },
                identity: None,
                pt_node_pool: PhysRange::empty(),
                reserved_page_tables: &[],
            },
        );
    }
}

fn linked_kernel_image() -> PhysRange {
    let start = linked_kernel_start();
    let end = linked_kernel_end();

    PhysRange {
        start: PhysAddr(start),
        size: end.saturating_sub(start),
    }
}

#[cfg(target_arch = "loongarch64")]
fn linked_kernel_start() -> usize {
    core::ptr::addr_of!(__kernel_start) as usize
}

#[cfg(not(target_arch = "loongarch64"))]
fn linked_kernel_start() -> usize {
    QEMU_LA64_KERNEL_LOAD_BASE
}

#[cfg(target_arch = "loongarch64")]
fn linked_kernel_end() -> usize {
    core::ptr::addr_of!(__kernel_end) as usize
}

#[cfg(not(target_arch = "loongarch64"))]
fn linked_kernel_end() -> usize {
    QEMU_LA64_KERNEL_LOAD_BASE + 128 * 1024
}

fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

fn installed_pt_node_allocator() -> Option<PtNodeAllocator> {
    let value = INSTALLED_PT_NODE_ALLOCATOR.load(Ordering::Acquire);
    if value == 0 {
        return None;
    }

    Some(unsafe { core::mem::transmute::<usize, PtNodeAllocator>(value) })
}

fn dmw_mmio_page_is_precovered(virt: VirtAddr, phys: PhysAddr, kind: PmapReserveKind) -> bool {
    kind == PmapReserveKind::Page4K
        && virt == VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_PAGE_BASE))
        && phys == PhysAddr(QEMU_LA64_UART0_PAGE_BASE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;
    use tx_hal::{
        AllocError, AuxvIf, MemoryRegionKind, PmapError, PmapIf, PtNode, PtNodeSourceKind,
    };

    static TEST_PT_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    static TEST_PT_RELEASES: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn boot_info_publishes_qemu_ram_and_kernel_image() {
        let info = Platform::boot_info();

        assert_eq!(info.initrd, None);
        assert_eq!(info.cmdline, None);
        assert_eq!(info.kernel_image.start, PhysAddr(0x0020_0000));
        assert!(info.kernel_image.size > 0);

        assert_eq!(info.memory_regions.len(), 2);
        assert_eq!(info.memory_regions[0].base, PhysAddr(0));
        assert_eq!(info.memory_regions[0].kind, MemoryRegionKind::Reserved);
        assert!(info.memory_regions[0].size >= info.kernel_image.end().0);

        assert_eq!(info.memory_regions[1].kind, MemoryRegionKind::Usable);
        assert_eq!(
            info.memory_regions[1].base,
            PhysAddr(info.memory_regions[0].size)
        );
        assert_eq!(
            info.memory_regions[1].base.0 + info.memory_regions[1].size,
            0x1000_0000
        );
    }

    #[test]
    fn bootstrap_pmap_info_describes_dmw_direct_ram() {
        let info = Platform::boot_info();
        let pmap = Platform::bootstrap_pmap_info().expect("bootstrap pmap info");

        assert_eq!(pmap.root, PhysAddr(0));
        assert_eq!(
            pmap.mapped,
            PhysRange {
                start: PhysAddr(0),
                size: 0x1000_0000,
            }
        );
        assert_eq!(pmap.direct_map_base, VirtAddr(LA64_DMW_CACHED_BASE));
        assert_eq!(
            pmap.direct_map,
            VirtRange {
                start: VirtAddr(LA64_DMW_CACHED_BASE),
                size: 0x1000_0000,
            }
        );
        assert_eq!(pmap.identity, None);
        assert_eq!(
            pmap.kernel_image.start,
            VirtAddr(la64_cached_virt(info.kernel_image.start.0))
        );
        assert_eq!(pmap.kernel_image.size, info.kernel_image.size);
        assert_eq!(pmap.pt_node_pool, PhysRange::empty());
        assert!(pmap.reserved_page_tables.is_empty());
    }

    #[test]
    fn substrate_smoke_gate_is_enabled_and_uart_mmio_is_published() {
        let substrate_ready =
            core::hint::black_box(<Platform as PlatformConfig>::SUBSTRATE_BOOT_READY);
        assert!(substrate_ready);
        assert_eq!(Platform::platform_info().mmio_regions.len(), 1);
        assert_eq!(Platform::platform_info().mmio_regions[0].name, "uart0");
        assert_eq!(
            Platform::platform_info().mmio_regions[0].virt.start,
            VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_BASE))
        );
    }

    #[test]
    fn auxv_facts_publish_loongarch64_platform() {
        let facts = <Platform as AuxvIf>::arch_auxv_facts();

        assert_eq!(facts.page_size, <Platform as PlatformConfig>::PAGE_SIZE);
        assert_eq!(facts.hwcap, 0);
        assert_eq!(facts.hwcap2, 0);
        assert_eq!(facts.platform, "loongarch64");
    }

    #[test]
    #[cfg(not(target_arch = "loongarch64"))]
    fn console_read_bytes_is_nonblocking_when_host_has_no_uart_input() {
        let mut buf = [0xaa; 4];

        assert_eq!(<Platform as ConsoleIf>::read_bytes(&mut buf), 0);
        assert_eq!(tx_hal::console_read_bytes::<Platform>(&mut buf), 0);
        assert_eq!(buf, [0xaa; 4]);
    }

    #[test]
    fn trap_vector_addresses_are_written_as_physical_addresses() {
        assert_eq!(
            la64_kernel_addr_to_phys(la64_cached_virt(0x1234_5000)),
            0x1234_5000
        );
        assert_eq!(
            la64_kernel_addr_to_phys(la64_uncached_virt(0x1fe0_0000)),
            0x1fe0_0000
        );
    }

    #[test]
    #[cfg(not(target_arch = "loongarch64"))]
    fn trap_vector_install_is_host_noop() {
        <Platform as TrapIf>::install_minimal_trap_vector();
        <Platform as TrapIf>::install_kernel_trap_vector();
        <Platform as TrapIf>::install_user_trap_vector();
    }

    #[test]
    fn la64_trap_classification_decodes_interrupts_and_sync_faults() {
        assert_eq!(
            classify_la64_trap(LA64_ESTAT_IS_TIMER),
            TrapClass::TimerInterrupt
        );
        assert_eq!(
            classify_la64_trap(LA64_ESTAT_IS_IPI),
            TrapClass::InterprocessorInterrupt
        );
        assert_eq!(classify_la64_trap(1 << 2), TrapClass::ExternalInterrupt);
        assert_eq!(
            classify_la64_trap(LA64_ECODE_SYS << LA64_ESTAT_ECODE_SHIFT),
            TrapClass::Syscall
        );
        assert_eq!(
            classify_la64_trap(LA64_ECODE_BRK << LA64_ESTAT_ECODE_SHIFT),
            TrapClass::Breakpoint
        );
        assert_eq!(
            classify_la64_trap(LA64_ECODE_INE << LA64_ESTAT_ECODE_SHIFT),
            TrapClass::IllegalInstruction
        );
        assert_eq!(
            classify_la64_trap(LA64_ECODE_PIS << LA64_ESTAT_ECODE_SHIFT),
            TrapClass::PageFault {
                write: true,
                instruction: false,
            }
        );
        assert_eq!(
            classify_la64_trap(LA64_ECODE_PIF << LA64_ESTAT_ECODE_SHIFT),
            TrapClass::PageFault {
                write: false,
                instruction: true,
            }
        );
    }

    #[test]
    fn la64_platform_snapshot_projects_classified_fault() {
        let snapshot = TrapFrameSnapshot {
            scause: LA64_ECODE_PIL << LA64_ESTAT_ECODE_SHIFT,
            sepc: 0x2000,
            stval: 0x3000,
        };
        let portable = Platform::snapshot_trap(snapshot);

        assert_eq!(
            portable.class,
            TrapClass::PageFault {
                write: false,
                instruction: false,
            }
        );
        assert_eq!(portable.pc, VirtAddr(0x2000));
        assert_eq!(portable.fault_address, Some(VirtAddr(0x3000)));
    }

    #[test]
    fn dmw_uart_mmio_page_is_reported_as_precovered() {
        assert_eq!(
            Platform::reserve_kernel_mapping(
                VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_PAGE_BASE)),
                PhysAddr(QEMU_LA64_UART0_PAGE_BASE),
                PmapReserveKind::Page4K,
            ),
            Ok(None)
        );
        assert_eq!(
            Platform::reserve_kernel_mapping(
                VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_PAGE_BASE + 0x1000)),
                PhysAddr(QEMU_LA64_UART0_PAGE_BASE),
                PmapReserveKind::Page4K,
            ),
            Err(PmapError::Unsupported)
        );
    }

    #[test]
    fn dmw_direct_map_reservation_is_precovered_for_ram() {
        assert_eq!(
            Platform::reserve_kernel_direct_map_1g(PhysAddr(0)),
            Ok(None)
        );
        assert_eq!(
            Platform::extend_direct_map(PhysAddr(QEMU_LA64_RAM_END)),
            Ok(())
        );
        assert_eq!(
            Platform::extend_direct_map(PhysAddr(QEMU_LA64_RAM_END + 1)),
            Err(PmapError::Unsupported)
        );
    }

    #[test]
    #[cfg(not(target_arch = "loongarch64"))]
    fn pmap_shootdown_paths_are_host_noops() {
        let invalidation = PmapInvalidation::new(VirtAddr(LA64_DMW_CACHED_BASE), 4096);

        Platform::shootdown_kernel_mapping(invalidation);
        Platform::shootdown_mapping(Asid(1), invalidation);
    }

    #[test]
    fn la64_timebase_frequency_uses_cpucfg_ratio() {
        assert_eq!(
            la64_cpucfg_timebase_frequency(LA64_CPUCFG2_LLFTP, 100_000_000, 3 | (2 << 16)),
            150_000_000
        );
        assert_eq!(
            la64_cpucfg_timebase_frequency(0, 100_000_000, 1 | (1 << 16)),
            0
        );
        assert_eq!(
            la64_cpucfg_timebase_frequency(LA64_CPUCFG2_LLFTP, 100_000_000, 0),
            0
        );
    }

    #[test]
    fn la64_timer_deadline_rounds_up_to_tcfg_granule() {
        assert_eq!(round_up_to_tcfg_ticks(1), 4);
        assert_eq!(round_up_to_tcfg_ticks(4), 4);
        assert_eq!(round_up_to_tcfg_ticks(5), 8);
        assert_eq!(round_up_to_tcfg_ticks(u64::MAX), u64::MAX - 3);
    }

    #[test]
    #[cfg(not(target_arch = "loongarch64"))]
    fn timeif_paths_are_host_noops_without_cpu_counter() {
        assert_eq!(<Platform as TimeIf>::frequency_hz(), 0);
        assert_eq!(<Platform as TimeIf>::read_ns(), 0);
        <Platform as TimeIf>::set_deadline_ns(1_000_000);
        <Platform as TimeIf>::cancel_deadline();
        <Platform as TimeIf>::enable_timer_wakeups();
    }

    #[test]
    #[cfg(not(target_arch = "loongarch64"))]
    fn percpu_and_smp_publish_uniprocessor_state() {
        let saved_tls = <Platform as PercpuIf>::read_kernel_tls();

        <Platform as PercpuIf>::install_early_percpu(CpuId(0));
        assert_eq!(<Platform as PercpuIf>::current_cpu_id(), CpuId(0));
        assert_eq!(<Platform as SmpIf>::current_cpu_id(), CpuId(0));
        assert_eq!(
            <Platform as SmpIf>::possible_cpus(),
            CpuMask::single(CpuId(0))
        );

        <Platform as SmpIf>::mark_cpu_online(CpuId(0));
        assert_eq!(
            <Platform as SmpIf>::online_cpus(),
            CpuMask::single(CpuId(0))
        );
        assert_eq!(
            <Platform as SmpIf>::boot_secondary_cpus(test_secondary_entry),
            0
        );
        assert!(!<Platform as SmpIf>::pending_ipi(IpiKind::Reschedule));

        <Platform as SmpIf>::clear_ipi_ack_cpus(IpiKind::Reschedule, CpuMask::single(CpuId(0)));
        assert_eq!(
            <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule),
            CpuMask::EMPTY
        );
        <Platform as SmpIf>::ack_ipi(IpiKind::Reschedule);
        assert_eq!(
            <Platform as SmpIf>::ipi_ack_cpus(IpiKind::Reschedule),
            CpuMask::single(CpuId(0))
        );

        <Platform as PercpuIf>::write_kernel_tls(saved_tls);
    }

    #[test]
    fn pt_node_allocator_handoff_is_one_shot_and_releases_typed_frames() {
        reset_pt_node_allocator_for_test();
        TEST_PT_ALLOCATIONS.store(0, Ordering::Release);
        TEST_PT_RELEASES.store(0, Ordering::Release);

        assert_eq!(Platform::alloc_pt_node(), Err(AllocError::Exhausted));
        assert_eq!(
            Platform::install_pt_node_allocator(test_pt_allocator),
            Ok(())
        );
        assert_eq!(
            Platform::install_pt_node_allocator(test_pt_allocator),
            Err(PmapError::AlreadyMapped)
        );

        let node = Platform::alloc_pt_node().expect("typed frame PT node");
        assert_eq!(node.phys, PhysAddr(0x0040_0000));
        assert_eq!(node.source_kind(), PtNodeSourceKind::TypedFrame);
        assert_eq!(TEST_PT_ALLOCATIONS.load(Ordering::Acquire), 1);

        Platform::free_pt_node(node);
        assert_eq!(TEST_PT_RELEASES.load(Ordering::Acquire), 1);

        reset_pt_node_allocator_for_test();
    }

    fn test_pt_allocator() -> Result<PtNode, AllocError> {
        TEST_PT_ALLOCATIONS.fetch_add(1, Ordering::AcqRel);
        Ok(PtNode::typed_frame(PhysAddr(0x0040_0000), test_pt_release))
    }

    unsafe fn test_pt_release(phys: PhysAddr) {
        assert_eq!(phys, PhysAddr(0x0040_0000));
        TEST_PT_RELEASES.fetch_add(1, Ordering::AcqRel);
    }

    unsafe extern "C" fn test_secondary_entry(_cpu_id: usize) -> ! {
        panic!("LA64 qemu virt is configured as uniprocessor in this HAL crate")
    }

    fn reset_pt_node_allocator_for_test() {
        INSTALLED_PT_NODE_ALLOCATOR.store(0, Ordering::Release);
    }
}
