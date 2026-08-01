//! 页表节点(中间层页表页)的所有权来源。
//!
//! 这里维护的核心数据结构/状态：
//! - `PT_NODE_ALLOCATED`：链接器预留的早期引导 PT-node 池的分配位图。
//! - `INSTALLED_PT_NODE_ALLOCATOR`：substrate 一次性移交进来的、面向类型化
//!   页表帧分配的分配器。
//! - `CommittedPtNodeRegistry`：从已提交分支表物理地址反查其所属 `PtNode`
//!   的定长登记表。
//! - `RegistryGuard`：已提交节点登记表自旋锁的 RAII 守卫。
//!
//! 主要的状态修改函数：
//! - `alloc_pt_node()` 与 `free_pt_node()`：优先用类型化帧，耗尽时回退到
//!   引导池。
//! - `install_pt_node_allocator()`：把稳态分配切换到 substrate 提供的来源。
//! - `register_committed_intermediates()`：在提交时登记 PT-node 所有权。
//! - `release_committed_pt_node_from_bag()`：后续修剪后释放所有权。
//!
//! 辅助函数分组：
//! - 引导池辅助：扫描/更新分配位图并清零页；
//! - 已安装分配器辅助：编码/解码函数指针；
//! - 登记表辅助：加锁、插入、取出、清空已提交节点条目。
//!
//! 早期引导阶段帧分配器尚未就绪，故使用固定的板卡自持 PT-node 池。substrate
//! 安装类型化分配器后，新 PT 节点优先使用类型化页表帧，仅在耗尽时才回退到
//! 引导池。参见
//! `docs/progress/decisions/2026-04-29-pmap-typed-intermediate-source.md` 与
//! `docs/progress/decisions/2026-04-29-pmap-committed-pt-node-teardown.md`。

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tx_hal::{
    AllocError, PhysAddr, PmapError, PmapReservationIntermediates, PtNode, PtNodeAllocator,
};

use crate::boot_static::{BootStaticBag, IdentityDropped};

use super::{pool_index, pt_node_zero_ptr, zero_page, PT_NODE_POOL_ENTRIES};

/// 已提交 PT-node 登记表的槽位数量（哈希表容量，须为 2 的幂）。
const COMMITTED_PT_NODE_REGISTRY_ENTRIES: usize = 8192;

/// 引导池分配位图：每一位对应一个引导池页表页，置 1 表示已分配。
static PT_NODE_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
/// 已安装的类型化分配器函数指针（以 usize 存储，0 表示尚未安装）。
static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
/// 已提交节点登记表的自旋锁（false=未持有）。
static COMMITTED_PT_NODE_REGISTRY_LOCK: AtomicBool = AtomicBool::new(false);
/// 物理地址 -> PtNode 的反查登记表，初始全为空槽。
static COMMITTED_PT_NODES: CommittedPtNodeRegistry = CommittedPtNodeRegistry(UnsafeCell::new(
    [CommittedPtNodeEntry::Empty; COMMITTED_PT_NODE_REGISTRY_ENTRIES],
));

/// 从已提交分支表物理页反查到 `PtNode` 的定长登记表。
///
/// 分支 PTE 只保存物理地址。此登记表保留了类型化的所有权令牌，供后续 unmap
/// 修剪该分支时用来释放对应的页表页。
struct CommittedPtNodeRegistry(
    UnsafeCell<[CommittedPtNodeEntry; COMMITTED_PT_NODE_REGISTRY_ENTRIES]>,
);

// 登记表以自旋锁串行访问，故手动标记为 Sync。
unsafe impl Sync for CommittedPtNodeRegistry {}

/// 登记表槽位状态：空、墓碑（曾占用现已释放）、被某 PtNode 占用。
#[derive(Clone, Copy)]
enum CommittedPtNodeEntry {
    Empty,
    Tombstone,
    Occupied(PtNode),
}

// 分配优先使用已安装的类型化分配器（若存在），否则回退到静态引导池。引导池是
// `BootStaticBag` 中链接器预留页表页上的位图；类型化帧通过 `PtNode` 自带释放钩子。

/// 分配一个 PT-node（使用全局 BootStaticBag）。
pub(crate) fn alloc_pt_node() -> Result<PtNode, AllocError> {
    alloc_pt_node_from_bag(BootStaticBag::<IdentityDropped>::global_ref())
}

/// 从指定 bag 分配 PT-node：先试类型化分配器，失败则退回引导池。
pub(super) fn alloc_pt_node_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<PtNode, AllocError> {
    if let Some(allocator) = installed_pt_node_allocator() {
        if let Ok(node) = allocator() {
            return Ok(node);
        }
    }

    alloc_boot_pool_pt_node_from_bag(bag)
}

/// 从引导池位图中分配一页：扫描空闲位并用 CAS 占位，成功后清零该页。
fn alloc_boot_pool_pt_node_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<PtNode, AllocError> {
    loop {
        let allocated = PT_NODE_ALLOCATED.load(Ordering::Acquire);
        if allocated == pool_full_mask() {
            // 位图全 1，引导池已耗尽。
            return Err(AllocError::Exhausted);
        }

        for index in 0..PT_NODE_POOL_ENTRIES {
            let bit = 1usize << index;
            if allocated & bit != 0 {
                continue; // 该位已占用，跳过。
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
                // CAS 成功占到该位，取物理地址并清零后返回。
                let phys = bag.pt_node_phys(index);
                zero_pt_node(bag, index);
                return Ok(PtNode::boot_pool(phys));
            }
            break; // CAS 失败（位图被并发改动），重读快照后重试整轮。
        }
    }
}

/// 释放一个 PT-node（使用全局 BootStaticBag）。
pub(crate) fn free_pt_node(node: PtNode) {
    free_pt_node_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), node);
}

/// 释放 PT-node：类型化帧走自带释放；否则在引导池位图中清掉对应位。
pub(super) fn free_pt_node_from_bag<State>(bag: &BootStaticBag<State>, node: PtNode) {
    if unsafe { node.release_typed_frame() } {
        return;
    }

    if let Some(index) = pool_index(bag, node.phys) {
        PT_NODE_ALLOCATED.fetch_and(!(1usize << index), Ordering::AcqRel);
    }
}

// 已安装分配器是帧分配器就绪后由 substrate 一次性移交的。为契合静态 HAL 表面，
// 它刻意保持为函数指针而非 dyn 对象。

/// 安装类型化 PT-node 分配器（一次性）；若已安装则返回 AlreadyMapped。
pub(crate) fn install_pt_node_allocator(allocator: PtNodeAllocator) -> Result<(), PmapError> {
    // 把函数指针编码为 usize，用 CAS 从 0 抢占（保证只安装一次）。
    let value = allocator as usize;
    INSTALLED_PT_NODE_ALLOCATOR
        .compare_exchange(0, value, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| PmapError::AlreadyMapped)
}

/// 读取已安装分配器：0 表示未安装，否则把 usize 解码回函数指针。
fn installed_pt_node_allocator() -> Option<PtNodeAllocator> {
    let value = INSTALLED_PT_NODE_ALLOCATOR.load(Ordering::Acquire);
    if value == 0 {
        return None;
    }

    Some(unsafe { core::mem::transmute::<usize, PtNodeAllocator>(value) })
}

/// 已提交 PT-node 登记表自旋锁的 RAII 守卫，Drop 时自动解锁。
struct RegistryGuard;

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        // 释放锁。
        COMMITTED_PT_NODE_REGISTRY_LOCK.store(false, Ordering::Release);
    }
}

/// 自旋获取登记表锁，返回 RAII 守卫。
fn lock_committed_pt_node_registry() -> RegistryGuard {
    while COMMITTED_PT_NODE_REGISTRY_LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    RegistryGuard
}

// 已提交节点的登记与查找是从"页表形态"回到"所有权"的桥梁：提交时登记，修剪时
// 取出，然后把释放分派到类型化帧拆除或引导池位图释放。

/// 把已提交的 PT-node 登记进反查表（开放寻址 + 线性探测，遇墓碑可复用槽位）。
pub(super) fn register_committed_pt_node(node: PtNode) {
    let _guard = lock_committed_pt_node_registry();
    let nodes = unsafe { &mut *COMMITTED_PT_NODES.0.get() };

    let mut first_tombstone = None; // 记录探测途中遇到的首个墓碑槽，优先复用。
    let start = committed_pt_node_slot_index(node.phys); // 哈希起始槽。
    for offset in 0..COMMITTED_PT_NODE_REGISTRY_ENTRIES {
        let index = (start + offset) % COMMITTED_PT_NODE_REGISTRY_ENTRIES;
        match nodes[index] {
            CommittedPtNodeEntry::Empty => {
                // 探测终止：优先落到之前遇到的墓碑槽，否则用此空槽。
                let index = first_tombstone.unwrap_or(index);
                nodes[index] = CommittedPtNodeEntry::Occupied(node);
                return;
            }
            CommittedPtNodeEntry::Tombstone => {
                first_tombstone.get_or_insert(index);
            }
            CommittedPtNodeEntry::Occupied(registered) if registered.phys == node.phys => {
                return; // 同一物理地址已登记，幂等返回。
            }
            CommittedPtNodeEntry::Occupied(_) => {}
        }
    }
    // 一整轮未遇空槽：若途中有墓碑仍可复用。
    if let Some(index) = first_tombstone {
        nodes[index] = CommittedPtNodeEntry::Occupied(node);
        return;
    }
    panic!(
        "committed PT-node registry exhausted: entries={}",
        COMMITTED_PT_NODE_REGISTRY_ENTRIES
    );
}

/// 登记一次预约产生的中间层节点（L1、L0，若存在）。
pub(super) fn register_committed_intermediates(intermediates: PmapReservationIntermediates) {
    if let Some(l1) = intermediates.l1 {
        register_committed_pt_node(l1);
    }
    if let Some(l0) = intermediates.l0 {
        register_committed_pt_node(l0);
    }
}

/// 按物理地址从登记表取出 PtNode，命中后把该槽置为墓碑；未命中返回 None。
fn take_committed_pt_node(phys: PhysAddr) -> Option<PtNode> {
    let _guard = lock_committed_pt_node_registry();
    let nodes = unsafe { &mut *COMMITTED_PT_NODES.0.get() };
    let start = committed_pt_node_slot_index(phys);
    for offset in 0..COMMITTED_PT_NODE_REGISTRY_ENTRIES {
        let index = (start + offset) % COMMITTED_PT_NODE_REGISTRY_ENTRIES;
        match nodes[index] {
            CommittedPtNodeEntry::Empty => return None, // 遇空槽即确定不存在。
            CommittedPtNodeEntry::Tombstone => {}       // 墓碑：继续探测。
            CommittedPtNodeEntry::Occupied(registered) if registered.phys == phys => {
                nodes[index] = CommittedPtNodeEntry::Tombstone; // 取走并留墓碑。
                return Some(registered);
            }
            CommittedPtNodeEntry::Occupied(_) => {}
        }
    }
    None
}

/// 修剪分支后，按物理地址取出并释放对应的已提交 PT-node。
pub(super) fn release_committed_pt_node_from_bag<State>(
    bag: &BootStaticBag<State>,
    phys: PhysAddr,
) {
    if let Some(node) = take_committed_pt_node(phys) {
        free_pt_node_from_bag(bag, node);
    }
}

/// 清零引导池第 index 页（新分配的页表页必须全零）。
fn zero_pt_node<State>(bag: &BootStaticBag<State>, index: usize) {
    unsafe {
        zero_page(pt_node_zero_ptr(bag, index));
    }
}

/// 由物理页号经 Fibonacci 哈希算出登记表槽位起点（& 掩码取低位）。
fn committed_pt_node_slot_index(phys: PhysAddr) -> usize {
    let page = phys.0 >> 12;
    page.wrapping_mul(0x9e37_79b9_7f4a_7c15usize) & (COMMITTED_PT_NODE_REGISTRY_ENTRIES - 1)
}

/// 引导池"全部已分配"的位图掩码（低 PT_NODE_POOL_ENTRIES 位全 1）。
const fn pool_full_mask() -> usize {
    (1usize << PT_NODE_POOL_ENTRIES) - 1
}

/// 测试用：直接设置（或清除）已安装分配器。
#[cfg(test)]
pub(super) fn install_pt_node_allocator_for_test(allocator: Option<PtNodeAllocator>) {
    INSTALLED_PT_NODE_ALLOCATOR.store(
        allocator.map_or(0, |allocator| allocator as usize),
        Ordering::Release,
    );
}

/// 测试用：把登记表全部清空。
#[cfg(test)]
pub(super) fn reset_committed_pt_nodes_for_test() {
    let _guard = lock_committed_pt_node_registry();
    unsafe {
        (*COMMITTED_PT_NODES.0.get()).fill(CommittedPtNodeEntry::Empty);
    }
}

/// 测试用：清空引导池位图并重置登记表。
#[cfg(test)]
pub(super) fn reset_pt_node_pool_allocations_for_test() {
    PT_NODE_ALLOCATED.store(0, Ordering::Release);
    reset_committed_pt_nodes_for_test();
}

/// 测试用：读取当前引导池分配位图。
#[cfg(test)]
pub(super) fn pt_node_allocated_for_test() -> usize {
    PT_NODE_ALLOCATED.load(Ordering::Acquire)
}
