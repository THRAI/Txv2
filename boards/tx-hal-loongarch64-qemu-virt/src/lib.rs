#![no_std]

#[cfg(test)]
extern crate std;

use tx_hal::{
    AllocError, Arch, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf, BootPlatformIf,
    BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, DmaIf, InitIf, IrqIf, MemoryRegion,
    MemoryRegionKind, MmioFlags, MmioRegion, PercpuIf, PhysAddr, PhysRange, PlatformConfig,
    PlatformInfo, PlatformInfoIf, PmapError, PmapIf, PmapReservation, PmapReserveKind, PowerIf,
    PtNode, PtNodeAllocator, SignalFrameIf, SmpIf, TimeIf, TrapIf, UserAccessIf, VirtAddr,
    VirtRange,
};

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text.boot, "ax"
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
    move    $a0, $s0
    move    $a1, $s1
    la.local $t0, rust_entry
    jirl    $zero, $t0, 0

3:
    idle    0
    b       3b
"#
);

pub struct Platform;

const QEMU_LA64_RAM_BASE: usize = 0;
const QEMU_LA64_RAM_SIZE: usize = 0x1000_0000;
const QEMU_LA64_RAM_END: usize = QEMU_LA64_RAM_BASE + QEMU_LA64_RAM_SIZE;
const QEMU_LA64_KERNEL_LOAD_BASE: usize = 0x0020_0000;

static BOOT_FACTS_STATE: AtomicU8 = AtomicU8::new(0);
static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);

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
const UART_THR: usize = 0x00;
const UART_LSR: usize = 0x05;
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
        start: VirtAddr(QEMU_LA64_UART0_BASE),
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
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(QEMU_LA64_RAM_BASE);
    const DIRECT_MAP_SIZE: usize = QEMU_LA64_RAM_SIZE;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(QEMU_LA64_KERNEL_LOAD_BASE);
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

impl AuxvIf for Platform {}
impl ConsoleIf for Platform {
    fn write_bytes(bytes: &[u8]) {
        for &byte in bytes {
            uart_put_byte(byte);
        }
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

    fn reserve_kernel_mapping(
        virt: VirtAddr,
        phys: PhysAddr,
        kind: PmapReserveKind,
    ) -> Result<Option<PmapReservation>, PmapError> {
        if identity_mmio_page_is_precovered(virt, phys, kind) {
            return Ok(None);
        }

        Err(PmapError::Unsupported)
    }
}
impl TrapIf for Platform {}
impl UserAccessIf for Platform {}
impl SignalFrameIf for Platform {}
impl IrqIf for Platform {}
impl TimeIf for Platform {
    fn read_ns() -> u64 {
        0
    }

    fn set_deadline_ns(_deadline: u64) {}

    fn cancel_deadline() {}

    fn frequency_hz() -> u64 {
        PLATFORM_INFO.timebase_frequency_hz
    }
}
impl PercpuIf for Platform {}
impl CacheIf for Platform {}
impl DmaIf for Platform {}
impl SmpIf for Platform {}

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

fn uart_put_byte(byte: u8) {
    let base = QEMU_LA64_UART0_BASE as *mut u8;

    unsafe {
        while core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile(base.add(UART_THR), byte);
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
    let identity = VirtRange {
        start: VirtAddr(QEMU_LA64_RAM_BASE),
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
                // LA64 is still running identity/direct with no board-owned
                // hardware page-table root. Substrate smoke can consume these
                // facts, but mapping mutation stays unsupported until real
                // pmap work lands.
                root: PhysAddr(0),
                mapped: PhysRange {
                    start: PhysAddr(QEMU_LA64_RAM_BASE),
                    size: QEMU_LA64_RAM_SIZE,
                },
                direct_map_base: VirtAddr(QEMU_LA64_RAM_BASE),
                direct_map: identity,
                kernel_image: VirtRange {
                    start: VirtAddr(kernel_image.start.0),
                    size: kernel_image.size,
                },
                identity: Some(identity),
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

fn identity_mmio_page_is_precovered(virt: VirtAddr, phys: PhysAddr, kind: PmapReserveKind) -> bool {
    kind == PmapReserveKind::Page4K
        && virt == VirtAddr(QEMU_LA64_UART0_PAGE_BASE)
        && phys == PhysAddr(QEMU_LA64_UART0_PAGE_BASE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;
    use tx_hal::{AllocError, MemoryRegionKind, PmapError, PmapIf, PtNode, PtNodeSourceKind};

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
    fn bootstrap_pmap_info_describes_identity_direct_ram() {
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
        assert_eq!(pmap.direct_map_base, VirtAddr(0));
        assert_eq!(
            pmap.direct_map,
            VirtRange {
                start: VirtAddr(0),
                size: 0x1000_0000,
            }
        );
        assert_eq!(pmap.identity, Some(pmap.direct_map));
        assert_eq!(pmap.kernel_image.start, VirtAddr(info.kernel_image.start.0));
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
    }

    #[test]
    fn identity_uart_mmio_page_is_reported_as_precovered() {
        assert_eq!(
            Platform::reserve_kernel_mapping(
                VirtAddr(QEMU_LA64_UART0_PAGE_BASE),
                PhysAddr(QEMU_LA64_UART0_PAGE_BASE),
                PmapReserveKind::Page4K,
            ),
            Ok(None)
        );
        assert_eq!(
            Platform::reserve_kernel_mapping(
                VirtAddr(QEMU_LA64_UART0_PAGE_BASE + 0x1000),
                PhysAddr(QEMU_LA64_UART0_PAGE_BASE),
                PmapReserveKind::Page4K,
            ),
            Err(PmapError::Unsupported)
        );
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

    fn reset_pt_node_allocator_for_test() {
        INSTALLED_PT_NODE_ALLOCATOR.store(0, Ordering::Release);
    }
}
