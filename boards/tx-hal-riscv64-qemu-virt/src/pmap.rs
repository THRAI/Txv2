use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use tx_hal::{AllocError, BootstrapPmapInfo, PhysAddr, PhysRange, PtNode, VirtAddr};

pub(crate) const SV39_MODE: usize = 8;
pub(crate) const PTE_V: u64 = 1 << 0;
pub(crate) const PTE_R: u64 = 1 << 1;
pub(crate) const PTE_W: u64 = 1 << 2;
pub(crate) const PTE_X: u64 = 1 << 3;
pub(crate) const PTE_A: u64 = 1 << 6;
pub(crate) const PTE_D: u64 = 1 << 7;

const QEMU_RAM_BASE: usize = 0x8000_0000;
const QEMU_BOOTSTRAP_MAP_SIZE: usize = 1024 * 1024 * 1024;
const PT_NODE_POOL_ENTRIES: usize = 8;
const PAGE_SIZE: usize = 4096;

#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct PageTable([u64; 512]);

struct PageTableCell(UnsafeCell<PageTable>);
struct PtNodePoolCell(UnsafeCell<[PageTable; PT_NODE_POOL_ENTRIES]>);
struct BootstrapPmapInfoCell(UnsafeCell<Option<BootstrapPmapInfo>>);

unsafe impl Sync for PageTableCell {}
unsafe impl Sync for PtNodePoolCell {}
unsafe impl Sync for BootstrapPmapInfoCell {}

static BOOTSTRAP_ROOT: PageTableCell = PageTableCell(UnsafeCell::new(PageTable([0; 512])));
static PT_NODE_POOL: PtNodePoolCell =
    PtNodePoolCell(UnsafeCell::new([PageTable([0; 512]); PT_NODE_POOL_ENTRIES]));
static PT_NODE_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static BOOTSTRAP_PMAP_INFO: BootstrapPmapInfoCell = BootstrapPmapInfoCell(UnsafeCell::new(None));

pub(crate) fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
    unsafe { (&*BOOTSTRAP_PMAP_INFO.0.get()).as_ref() }
}

pub(crate) fn alloc_pt_node() -> Result<PtNode, AllocError> {
    loop {
        let allocated = PT_NODE_ALLOCATED.load(Ordering::Acquire);
        if allocated == pool_full_mask() {
            return Err(AllocError::Exhausted);
        }

        for index in 0..PT_NODE_POOL_ENTRIES {
            let bit = 1usize << index;
            if allocated & bit != 0 {
                continue;
            }
            if PT_NODE_ALLOCATED
                .compare_exchange(
                    allocated,
                    allocated | bit,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                let phys = pool_node_phys(index);
                unsafe {
                    zero_page(phys.0 as *mut u8);
                }
                return Ok(PtNode { phys });
            }
            break;
        }
    }
}

pub(crate) fn free_pt_node(node: PtNode) {
    if let Some(index) = pool_index(node.phys) {
        PT_NODE_ALLOCATED.fetch_and(!(1usize << index), Ordering::AcqRel);
    }
}

#[no_mangle]
pub extern "C" fn tx_rv64_qemu_bootstrap_satp() -> usize {
    init_bootstrap_pmap();
    let root = bootstrap_root_phys();
    publish_bootstrap_pmap_info(root);
    bootstrap_satp_value(root)
}

fn init_bootstrap_pmap() {
    unsafe {
        let root = &mut *BOOTSTRAP_ROOT.0.get();
        root.0.fill(0);
        root.0[rv64_1g_leaf_index(QEMU_RAM_BASE)] =
            encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_X);
    }
}

fn publish_bootstrap_pmap_info(root: PhysAddr) {
    let pool_base = pool_node_phys(0);
    unsafe {
        *BOOTSTRAP_PMAP_INFO.0.get() = Some(BootstrapPmapInfo {
            root,
            mapped: PhysRange {
                start: PhysAddr(QEMU_RAM_BASE),
                size: QEMU_BOOTSTRAP_MAP_SIZE,
            },
            direct_map_base: VirtAddr(QEMU_RAM_BASE),
            pt_node_pool: PhysRange {
                start: pool_base,
                size: PT_NODE_POOL_ENTRIES * PAGE_SIZE,
            },
        });
    }
}

pub(crate) fn bootstrap_satp_value(root: PhysAddr) -> usize {
    (SV39_MODE << 60) | (root.0 >> 12)
}

pub(crate) fn encode_leaf_pte(phys: PhysAddr, flags: u64) -> u64 {
    ((phys.0 as u64 >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D
}

pub(crate) fn rv64_1g_leaf_index(virt: usize) -> usize {
    (virt >> 30) & 0x1ff
}

fn bootstrap_root_phys() -> PhysAddr {
    PhysAddr(BOOTSTRAP_ROOT.0.get() as usize)
}

fn pool_node_phys(index: usize) -> PhysAddr {
    unsafe {
        let pool = &mut *PT_NODE_POOL.0.get();
        PhysAddr(core::ptr::addr_of_mut!(pool[index]) as usize)
    }
}

fn pool_index(phys: PhysAddr) -> Option<usize> {
    let base = pool_node_phys(0).0;
    let end = base + PT_NODE_POOL_ENTRIES * PAGE_SIZE;
    if phys.0 < base || phys.0 >= end || !(phys.0 - base).is_multiple_of(PAGE_SIZE) {
        return None;
    }
    Some((phys.0 - base) / PAGE_SIZE)
}

const fn pool_full_mask() -> usize {
    (1usize << PT_NODE_POOL_ENTRIES) - 1
}

unsafe fn zero_page(page: *mut u8) {
    core::ptr::write_bytes(page, 0, PAGE_SIZE);
}

#[cfg(test)]
pub(crate) fn reset_pt_node_pool_for_test() {
    PT_NODE_ALLOCATED.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use tx_hal::{PhysAddr, PmapIf};

    use super::{
        bootstrap_satp_value, encode_leaf_pte, rv64_1g_leaf_index, PTE_A, PTE_D, PTE_R, PTE_V,
        PTE_W, PTE_X, SV39_MODE,
    };
    use crate::Platform;

    #[test]
    fn encodes_sv39_1g_leaf_pte_for_qemu_ram() {
        let pte = encode_leaf_pte(PhysAddr(0x8000_0000), PTE_R | PTE_W | PTE_X);

        assert_eq!(pte & PTE_V, PTE_V);
        assert_eq!(pte & PTE_R, PTE_R);
        assert_eq!(pte & PTE_W, PTE_W);
        assert_eq!(pte & PTE_X, PTE_X);
        assert_eq!(pte & PTE_A, PTE_A);
        assert_eq!(pte & PTE_D, PTE_D);
        assert_eq!(pte >> 28, 0x2);
    }

    #[test]
    fn qemu_ram_uses_expected_sv39_root_slot() {
        assert_eq!(rv64_1g_leaf_index(0x8000_0000), 2);
        assert_eq!(rv64_1g_leaf_index(0x8020_0000), 2);
    }

    #[test]
    fn bootstrap_satp_uses_sv39_mode_and_root_ppn() {
        let satp = bootstrap_satp_value(PhysAddr(0x8020_0000));

        assert_eq!(satp >> 60, SV39_MODE);
        assert_eq!(satp & ((1usize << 44) - 1), 0x8020_0000 >> 12);
    }

    #[test]
    fn pt_node_pool_allocates_fixed_boot_nodes() {
        super::reset_pt_node_pool_for_test();

        let first = Platform::alloc_pt_node().expect("first node");
        let second = Platform::alloc_pt_node().expect("second node");

        assert_ne!(first.phys, second.phys);
        Platform::free_pt_node(first);
        let reused = Platform::alloc_pt_node().expect("reused node");
        assert_eq!(reused.phys, first.phys);
    }
}
