//! 进程页表根（process-root pmap）的编排。
//!
//! 本模块负责 VM 物化所使用的、板级专属的 `PmapRoot` 生命周期。
//!
//! 这里维护的核心数据结构/状态：
//! - `PmapRoot`：带类型的根 PT-node 加上它的 ASID。
//! - `ALLOCATED_ASIDS`：固定大小的 v1 ASID 位图，ASID 0 保留。
//! - `RootEnsuredTable`：相对根表的投机式建表状态。
//! - `pt_node` 中已提交 PT-node 的登记表：在递归拆除时用来从分支 PTE 的物理
//!   地址恢复其所有权。
//!
//! 主要的状态修改函数：
//! - `create_pmap_root()` / `destroy_pmap_root()` 分配和释放进程页表根、
//!   拷贝共享的内核半区、并递归清空用户半区的表。
//! - `reserve_mapping()`、`rollback_mapping()`、`commit_mapping()` 实现用户
//!   映射的“预留—发布”两阶段事务。
//! - `unmap_mapping()` 和 `protect_mapping()` 修改已存在的用户叶子项，并返回
//!   失效/解除映射的凭证供后续 shootdown/记账使用。
//! - `shootdown_mapping()` 是本地 v1 的、以 ASID 为形态的失效钩子。
//!
//! 辅助函数组：
//! - ASID 辅助：在固定位图上分配/释放；
//! - 根表辅助：分配、回滚、查找、修剪 L1/L0 表；
//! - 拆除辅助：遍历已提交的分支子树，通过仅 pmap 的释放路径归还 PT-node 所有权。
//!
//! 参见 `docs/progress/decisions/2026-04-29-process-root-asid-shootdown-anchors.md`
//! 和 `docs/progress/decisions/2026-04-29-rv64-pmap-module-extraction.md`。

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tx_hal::{
    Asid, PhysAddr, PmapError, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReservationIntermediates, PmapReserveKind, PmapRoot, PmapUnmapResult, VirtAddr,
};

use crate::boot_static::{BootStaticBag, IdentityDropped, PageTable};

use super::kernel_space::{protect_leaf_slot, unmap_leaf_slot};
use super::pt_node::{
    alloc_pt_node_from_bag, free_pt_node_from_bag, register_committed_intermediates,
    release_committed_pt_node_from_bag,
};
use super::{
    encode_branch_pte, encode_leaf_pte_with_permissions, ensure_l0_table_for_reservation,
    l0_table_mut, page_table_mut_from_phys, pte_is_branch, pte_phys, rv64_1g_leaf_index,
    rv64_2m_leaf_index, rv64_4k_leaf_index, sfence_vma_all, sfence_vma_range_asid,
    validate_aligned_mapping, validate_aligned_virt, validate_rv64_leaf_permissions,
    validate_user_mapping_virt,
};

// ASID 位图字数：16 个 u64
const ASID_BITMAP_WORDS: usize = 16;
// ASID 总容量 = 位数
pub(crate) const ASID_CAPACITY: usize = ASID_BITMAP_WORDS * u64::BITS as usize;

// ASID 分配位图（无锁），每一位对应一个 ASID；ASID 0 保留不分配。
static ALLOCATED_ASIDS: [AtomicU64; ASID_BITMAP_WORDS] =
    [const { AtomicU64::new(0) }; ASID_BITMAP_WORDS];

/// 相对根表“确保存在”的表，加上支撑它的新建 PT-node。
///
/// `node == None` 表示该表本就已存在。`Some(node)` 表示调用方必须提交此预留，
/// 或在失败时把该 node 回滚掉。
struct RootEnsuredTable {
    table: &'static mut PageTable,
    node: Option<tx_hal::PtNode>,
}

// 见证：被销毁的根已完成 TLB 失效（仅作为类型标记，防止漏刷）。
struct RootInvalidated;

// 根的生命周期：新进程根会拿到一个新建 PT-node、一个小 ASID，以及一份上半区
// 内核模板的拷贝。销毁时只遍历用户半区，通过 PT-node 登记表释放已提交的中间
// 表，最后归还根 node。
// 创建进程页表根：分配 ASID + 分配根表 + 清零 + 拷贝内核半区。
pub(crate) fn create_pmap_root() -> Result<PmapRoot, PmapError> {
    create_pmap_root_from_bag(BootStaticBag::<IdentityDropped>::global_ref())
}

pub(super) fn create_pmap_root_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<PmapRoot, PmapError> {
    let asid = alloc_asid()?;
    let node = match alloc_pt_node_from_bag(bag) {
        Ok(node) => node,
        Err(_) => {
            free_asid(asid);
            return Err(PmapError::Exhausted);
        }
    };

    let root = unsafe { page_table_mut_from_phys(node.phys) };
    root.0.fill(0); // 整张根表清零
    let kernel_root = unsafe { bag.bootstrap_root_mut() };
    root.0[256..].copy_from_slice(&kernel_root.0[256..]); // 拷贝内核半区（高 256 项）

    Ok(PmapRoot::new(node, asid))
}

// 销毁进程页表根：只递归释放用户半区 [..256]，内核半区不动。
pub(crate) fn destroy_pmap_root(root: PmapRoot) {
    destroy_pmap_root_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), root);
}

pub(super) fn destroy_pmap_root_from_bag<State>(bag: &BootStaticBag<State>, root: PmapRoot) {
    let root_phys = root.phys();
    // `satp` may still reference this root.  Leave it and invalidate all
    // translations before returning any child PT-node to the frame allocator;
    // otherwise a hardware page walk can continue through a page-table page
    // that has already been reused for unrelated kernel data.
    let invalidated = invalidate_destroyed_root(bag, root_phys);

    let table = unsafe { page_table_mut_from_phys(root_phys) };
    for slot in &mut table.0[..256] {
        // 只处理用户半区（低 256 项），内核半区保持不动
        if pte_is_branch(*slot) {
            release_page_table_tree_from_bag(bag, pte_phys(*slot)); // 递归释放子树
        }
        *slot = 0;
    }
    free_asid_after_invalidation(root.asid(), invalidated); // 清残留后再回收 ASID
    free_pt_node_from_bag(bag, root.into_node());
}

// 用户根的映射生命周期。预留（reserve）阶段可能分配中间表，并把这些 node 放进
// 预留令牌中携带；提交（commit）阶段发布最终叶子项并登记已提交的中间表；回滚
// （rollback）阶段释放任何尚未提交的表。protect 只更新同粒度且已存在（present）
// 的叶子项，把不安全的重新物化场景留给 VM/缺页处理。
// 预留用户映射（两阶段事务的第一步）：校验后确保中间表存在，返回携带中间表的令牌。
pub(crate) fn reserve_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    reserve_mapping_from_root(
        BootStaticBag::<IdentityDropped>::global_ref(),
        root.phys(),
        virt,
        phys,
        kind,
    )
}

pub(super) fn reserve_mapping_from_root<State>(
    bag: &BootStaticBag<State>,
    root: PhysAddr,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    validate_user_mapping_virt(virt, kind)?; // 校验虚地址落在用户区且符合粒度
    validate_aligned_mapping(virt, phys, kind.size())?; // 校验虚/物理地址对齐

    let root = unsafe { page_table_mut_from_phys(root) };
    match kind {
        PmapReserveKind::Superpage1G => {
            // 1G 大页直接落在根表叶子项，无需中间表
            let slot = &root.0[rv64_1g_leaf_index(virt.0)];
            if *slot != 0 {
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new(virt, phys, kind)))
        }
        PmapReserveKind::Superpage2M => {
            // 2M 大页需要一层 L1 表
            let ensured_l1 = ensure_l1_table_for_root(bag, root, virt)?;
            let current = ensured_l1.table.0[rv64_2m_leaf_index(virt.0)];
            if current != 0 {
                // 该槽已占用：把刚建的中间表回滚掉再报错
                if let Some(node) = ensured_l1.node {
                    rollback_intermediates_in_root(
                        bag,
                        root,
                        virt,
                        PmapReservationIntermediates {
                            l2: None,
                            l1: Some(node),
                            l0: None,
                        },
                    );
                }
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new_with_intermediates(
                virt,
                phys,
                kind,
                PmapReservationIntermediates {
                    l2: None,
                    l1: ensured_l1.node,
                    l0: None,
                },
            )))
        }
        PmapReserveKind::Page4K => {
            // 4K 页需要 L1、L0 两层中间表
            let ensured_l1 = ensure_l1_table_for_root(bag, root, virt)?;
            let l1_node = ensured_l1.node;
            let ensured_l0 = match ensure_l0_table_for_reservation(bag, ensured_l1.table, virt) {
                Ok(table) => table,
                Err(err) => {
                    // 建 L0 失败：回滚本次新建的 L1
                    if let Some(node) = l1_node {
                        rollback_intermediates_in_root(
                            bag,
                            root,
                            virt,
                            PmapReservationIntermediates {
                                l2: None,
                                l1: Some(node),
                                l0: None,
                            },
                        );
                    }
                    return Err(err);
                }
            };
            let current = ensured_l0.table.0[rv64_4k_leaf_index(virt.0)];
            if current != 0 {
                // 叶子槽已占用：回滚本次新建的 L1/L0
                rollback_intermediates_in_root(
                    bag,
                    root,
                    virt,
                    PmapReservationIntermediates {
                        l2: None,
                        l1: l1_node,
                        l0: ensured_l0.node,
                    },
                );
                return Err(PmapError::AlreadyMapped);
            }
            Ok(Some(PmapReservation::new_with_intermediates(
                virt,
                phys,
                kind,
                PmapReservationIntermediates {
                    l2: None,
                    l1: l1_node,
                    l0: ensured_l0.node,
                },
            )))
        }
    }
}

// 回滚一次未提交的预留：释放其携带的中间表并全刷 TLB。
pub(crate) fn rollback_mapping(root: &PmapRoot, reservation: PmapReservation) {
    rollback_mapping_from_root(
        BootStaticBag::<IdentityDropped>::global_ref(),
        root.phys(),
        reservation,
    );
}

fn rollback_mapping_from_root<State>(
    bag: &BootStaticBag<State>,
    root: PhysAddr,
    reservation: PmapReservation,
) {
    let root = unsafe { page_table_mut_from_phys(root) };
    rollback_intermediates_in_root(bag, root, reservation.virt(), reservation.intermediates());
    sfence_vma_all();
}

// 提交预留（两阶段事务的第二步）：写入最终叶子 PTE 并登记已提交的中间表。
pub(crate) fn commit_mapping(
    root: &PmapRoot,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    commit_mapping_from_root(root.phys(), reservation, permissions);
}

pub(super) fn commit_mapping_from_root(
    root: PhysAddr,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    validate_rv64_leaf_permissions(permissions, true).expect("invalid process pmap permissions");
    register_committed_intermediates(reservation.intermediates()); // 把中间表交给已提交登记表托管
    let pte = encode_leaf_pte_with_permissions(reservation.phys(), permissions); // 编码叶子 PTE
    let root = unsafe { page_table_mut_from_phys(root) };
    match reservation.kind() {
        PmapReserveKind::Superpage1G => {
            root.0[rv64_1g_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Superpage2M => {
            let l1 = l1_table_mut_from_root(root, reservation.virt()).expect("reserved L1 table");
            l1.0[rv64_2m_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Page4K => {
            let l1 = l1_table_mut_from_root(root, reservation.virt()).expect("reserved L1 table");
            let l0 = l0_table_mut(l1, reservation.virt()).expect("reserved L0 table");
            l0.0[rv64_4k_leaf_index(reservation.virt().0)] = pte;
        }
    }
    // 带 ASID 标记的硬件（QEMU）：此处不需要 fence。提交都是 invalid->valid
    //（reserve 已拒绝 AlreadyMapped），而 invalid PTE 从不会被缓存，因此下一次
    // 硬件页表遍历自然会取到新项；unmap/protect 在失效时仍会 fence。
    //
    // 零 ASID 硬件（VF2 U74）：再次进入“同一”地址空间会走 `activate_user_pmap`
    // 的快路径（不写 satp、不 fence），因此 fence 必须在这里做。按 SiFive 勘误
    // CIP-1200，带地址限定的形式在该硅片上不可靠——改用整表 sfence.vma
    //（与 Linux 的绕过手法一致）。
    if !crate::hw_asid_tagging_usable() {
        sfence_vma_all();
    }
}

// 解除用户映射：只清空对应叶子项并返回失效凭证供 shootdown。
//
// 已提交的空 L0/L1 继续由该进程根持有，直到 destroy_pmap_root 在离开当前
// satp 并完成 TLB 失效后统一回收。普通 unmap 不能在 shootdown 之前归还
// 中间页表页，否则硬件页表遍历可能继续访问已经复用的物理页。
pub(crate) fn unmap_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    unmap_mapping_from_root(
        BootStaticBag::<IdentityDropped>::global_ref(),
        root.phys(),
        virt,
        kind,
    )
}

pub(super) fn unmap_mapping_from_root<State>(
    _bag: &BootStaticBag<State>,
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    let root = unsafe { page_table_mut_from_phys(root) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
            unmap_leaf_slot(slot, virt, kind)
        }
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
                return Ok(None);
            };
            {
                let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)
            }
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
                return Ok(None);
            };
            let l1_slot = l1.0[rv64_2m_leaf_index(virt.0)];
            if l1_slot == 0 {
                return Ok(None);
            }
            if !pte_is_branch(l1_slot) {
                return Err(PmapError::InvalidRequest); // 该处是大页叶子而非分支，请求非法
            }
            let l0 = unsafe { page_table_mut_from_phys(pte_phys(l1_slot)) };
            {
                let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)
            }
        }
    }
}

// 修改已存在用户叶子项的权限：只改同粒度、已存在的叶子，返回失效凭证。
pub(crate) fn protect_mapping(
    root: &PmapRoot,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    protect_mapping_from_root(root.phys(), virt, kind, permissions)
}

pub(super) fn protect_mapping_from_root(
    root: PhysAddr,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    validate_rv64_leaf_permissions(permissions, true)?;
    validate_user_mapping_virt(virt, kind)?;
    validate_aligned_virt(virt, kind.size())?;
    let root = unsafe { page_table_mut_from_phys(root) };
    match kind {
        PmapReserveKind::Superpage1G => {
            let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
                return Ok(None);
            };
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut_from_root(root, virt) else {
                return Ok(None);
            };
            let l1_slot = l1.0[rv64_2m_leaf_index(virt.0)];
            if l1_slot == 0 {
                return Ok(None);
            }
            if !pte_is_branch(l1_slot) {
                return Err(PmapError::InvalidRequest); // 该处是大页叶子而非分支，请求非法
            }
            let l0 = unsafe { page_table_mut_from_phys(pte_phys(l1_slot)) };
            let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
    }
}

// 本 v1 实现里 ASID 刻意做得很小且局限于本地。API 采用 ASID 形态，以便后续的
// 远程 shootdown 和复用纪律能在同一套 HAL 接口下扩展。
// 单条失效：v1 直接全刷 TLB。
pub(crate) fn shootdown_mapping(_asid: Asid, _invalidation: PmapInvalidation) {
    sfence_vma_all();
}

// 批量失效：合并连续区间；能用 ASID 就按 ASID 精刷，否则退化整表刷，最后发远程 IPI。
pub(crate) fn shootdown_mappings(asid: Asid, invalidations: &[PmapInvalidation]) {
    if invalidations.is_empty() {
        return;
    }

    let coalesced = coalesce_invalidation_ranges(invalidations); // 合并相邻区间减少 fence 次数
    let asid_usable = crate::hw_asid_tagging_usable();
    if asid_usable {
        // 带 ASID 硬件：逐区间做地址+ASID 限定的精刷
        for invalidation in &coalesced {
            sfence_vma_range_asid(invalidation.virt(), invalidation.size(), asid);
        }
    } else {
        // SiFive U74 勘误 CIP-1200（JH7110/VF2）：带地址限定的 `sfence.vma`
        // 无法失效全部翻译缓存条目——Linux 的绕过手法是在该硅片上把每一次这种
        // fence 升级为整表 `sfence.vma`，我们照做。2026-07-03 已在真板上证实：
        // 某次 store 在一个 VA 上死循环，而其内存中的页表遍历
        //（按硬件顺序 root->l2e->l1e->l0e 读回）是一条完美的 V|R|W|X|U|A|D 链，
        // 尽管每轮迭代都发了按 VA 限定的 fence。我们以“零 ASID”探测为判据，
        // 它当前能唯一地识别出这颗核。
        sfence_vma_all();
    }
    crate::remote_sfence_vma_asid_batch(asid, &coalesced); // 通知其他 hart 做远程失效
}

// 分配一个空闲 ASID：在无锁位图上扫描，ASID 0 保留。
fn alloc_asid() -> Result<Asid, PmapError> {
    for word_index in 0..ASID_BITMAP_WORDS {
        loop {
            let allocated = ALLOCATED_ASIDS[word_index].load(Ordering::Acquire);
            let reserved = if word_index == 0 { 1 } else { 0 }; // 第 0 字的 bit0 对应保留的 ASID 0
            if allocated | reserved == u64::MAX {
                break; // 本字已满，换下一字
            }
            for bit_index in 0..u64::BITS as usize {
                let asid = word_index * u64::BITS as usize + bit_index;
                if asid == 0 || asid >= ASID_CAPACITY {
                    continue; // 跳过保留的 0 和越界
                }
                let bit = 1u64 << bit_index;
                if allocated & bit != 0 {
                    continue; // 该位已占用
                }
                // CAS 抢占该位；失败说明有并发修改，重读本字重试
                if ALLOCATED_ASIDS[word_index]
                    .compare_exchange(
                        allocated,
                        allocated | bit,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return Ok(Asid(asid as u16));
                }
                break;
            }
        }
    }
    Err(PmapError::Exhausted)
}

// 释放一个 ASID：清位图对应位（ASID 0 与越界忽略）。
fn free_asid(asid: Asid) {
    let asid = asid.0 as usize;
    if asid == 0 || asid >= ASID_CAPACITY {
        return;
    }
    let word_index = asid / u64::BITS as usize;
    let bit_index = asid % u64::BITS as usize;
    ALLOCATED_ASIDS[word_index].fetch_and(!(1u64 << bit_index), Ordering::AcqRel);
}

/// 在回收根页表前使其翻译失效。
///
/// 如果当前 hart 的 `satp` 仍指向这个根，单独执行 `sfence.vma`
/// 并不能解除引用：根页被帧分配器复用后，硬件会把新数据当成
/// PTE 继续游走。因此必须先切换到永久存在的 bootstrap 内核根，
/// 再刷新 TLB，然后才能释放进程根页表。
///
/// 当前保证本 hart（BuildStorm `-smp 1` 路径）；多核下还需要在释放前
/// 确保其他 hart 也已经离开该根页表。
fn invalidate_destroyed_root<State>(
    bag: &BootStaticBag<State>,
    root_phys: PhysAddr,
) -> RootInvalidated {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        const SATP_PPN_MASK: usize = (1usize << 44) - 1;
        let current_satp: usize;
        core::arch::asm!(
            "csrr {satp}, satp",
            satp = out(reg) current_satp,
            options(nomem, nostack)
        );
        if (current_satp & SATP_PPN_MASK) == (root_phys.0 >> 12) {
            const SATP_MODE_SV39: usize = 0x8 << 60;
            let bootstrap_satp = SATP_MODE_SV39 | (bag.bootstrap_root_phys().0 >> 12);
            core::arch::asm!(
                "csrw satp, {satp}",
                "sfence.vma",
                satp = in(reg) bootstrap_satp,
                options(nostack)
            );
            return RootInvalidated;
        }
    }
    #[cfg(not(target_arch = "riscv64"))]
    let _ = (bag, root_phys);
    sfence_vma_all();
    RootInvalidated
}

// 在完成 TLB 失效后回收 ASID：先清除该 ASID 的驻留记录再释放位图。
fn free_asid_after_invalidation(asid: Asid, _invalidated: RootInvalidated) {
    crate::clear_asid_residency(asid);
    free_asid(asid);
}

// 合并相邻的失效区间：把首尾相接的区间拼成一个，减少后续 fence 数量。
pub(crate) fn coalesce_invalidation_ranges(
    invalidations: &[PmapInvalidation],
) -> Vec<PmapInvalidation> {
    let mut coalesced: Vec<PmapInvalidation> = Vec::with_capacity(invalidations.len());
    for invalidation in invalidations {
        if let Some(last) = coalesced.last_mut() {
            let last_end = last.virt().0 + last.size();
            if last_end == invalidation.virt().0 {
                // 与上一区间首尾相接，扩展上一区间
                *last = PmapInvalidation::new(last.virt(), last.size() + invalidation.size());
                continue;
            }
        }
        coalesced.push(*invalidation);
    }
    coalesced
}

// 相对根表的中间表管理，与内核 bootstrap 的辅助函数对称，但作用于任意进程根。
// unmap 之后会修剪空的 L0/L1 表，使已提交的 PT-node 所有权经由 pmap 路径归还，
// 而不是丢失在裸的分支 PTE 里。
// 确保 virt 对应的 L1 表存在：已存在则直接返回；否则新建并记下待回滚的 node。
fn ensure_l1_table_for_root<State>(
    bag: &BootStaticBag<State>,
    root: &mut PageTable,
    virt: VirtAddr,
) -> Result<RootEnsuredTable, PmapError> {
    if let Some(table) = l1_table_mut_from_root(root, virt) {
        return Ok(RootEnsuredTable { table, node: None }); // 表已存在，node 为 None
    }

    let index = rv64_1g_leaf_index(virt.0);
    if root.0[index] != 0 {
        return Err(PmapError::AlreadyMapped); // 该根槽已是 1G 大页叶子
    }

    let node = alloc_pt_node_from_bag(bag).map_err(|_| PmapError::Exhausted)?;
    root.0[index] = encode_branch_pte(node.phys); // 把新表挂成分支 PTE
    let Some(table) = l1_table_mut_from_root(root, virt) else {
        // 回读不到分支，回滚刚写入的槽和 node
        root.0[index] = 0;
        free_pt_node_from_bag(bag, node);
        return Err(PmapError::InvalidRequest);
    };
    Ok(RootEnsuredTable {
        table,
        node: Some(node),
    })
}

// 回滚预留携带的中间表：先摘 L0 再摘 L1（仅当 PTE 确实指向该 node 才清槽），并归还 node。
fn rollback_intermediates_in_root<State>(
    bag: &BootStaticBag<State>,
    root: &mut PageTable,
    virt: VirtAddr,
    intermediates: PmapReservationIntermediates,
) {
    if let Some(l0) = intermediates.l0 {
        if let Some(l1) = l1_table_mut_from_root(root, virt) {
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            if pte_is_branch(*slot) && pte_phys(*slot) == l0.phys {
                *slot = 0; // 确认指向本 L0 才清
            }
        }
        free_pt_node_from_bag(bag, l0);
    }

    if let Some(l1) = intermediates.l1 {
        let slot = &mut root.0[rv64_1g_leaf_index(virt.0)];
        if pte_is_branch(*slot) && pte_phys(*slot) == l1.phys {
            *slot = 0; // 确认指向本 L1 才清
        }
        free_pt_node_from_bag(bag, l1);
    }
}

// 从根表取出 virt 对应的 L1 表可变引用：槽不是分支则返回 None。
pub(super) fn l1_table_mut_from_root(
    root: &mut PageTable,
    virt: VirtAddr,
) -> Option<&'static mut PageTable> {
    let pte = root.0[rv64_1g_leaf_index(virt.0)];
    if !pte_is_branch(pte) {
        return None;
    }
    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

// 递归释放整棵页表子树：深度优先清空各级分支并归还每个 PT-node。
fn release_page_table_tree_from_bag<State>(bag: &BootStaticBag<State>, phys: PhysAddr) {
    let table = unsafe { page_table_mut_from_phys(phys) };
    for slot in table.0.iter_mut() {
        if pte_is_branch(*slot) {
            release_page_table_tree_from_bag(bag, pte_phys(*slot)); // 先递归子表
        }
        *slot = 0;
    }
    release_committed_pt_node_from_bag(bag, phys);
}

// 测试专用：清空 ASID 位图，隔离各用例状态。
#[cfg(test)]
pub(super) fn reset_asids_for_test() {
    for word in &ALLOCATED_ASIDS {
        word.store(0, Ordering::Release);
    }
}
