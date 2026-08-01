//! 内核半区（高地址）pmap 操作。
//!
//! 本模块负责：直接映射扩展、MMIO/内核映射的预约/提交、内核权限原地修改，
//! 以及内核解除映射行为。这里是内核半区页表的所有写操作，对应
//! `PmapIf` 的 `*_kernel_mapping` / `direct_map` 接口。
//!
//! 这里维护的核心数据结构/状态：
//! - 存放于 `BootStaticBag<IdentityDropped>` 的全局引导根页表；
//! - `BootstrapPmapInfo`，尤其是 substrate 要消费的直接映射范围；
//! - 尚未发布的内核映射所用的 `PmapReservation` 中间表；
//! - 已提交的分支表在 PT-node 登记表中的条目。
//!
//! 主要修改状态的函数：
//! - `reserve_kernel_direct_map_1g()`、`commit_kernel_direct_map_1g()`、
//!   `extend_direct_map()` 以 1 GiB 叶子扩展永久直接映射。
//! - `reserve_kernel_mapping()`、`rollback_kernel_mapping()`、
//!   `commit_kernel_mapping()` 通过全局根发布 MMIO/直接映射叶子。
//! - `unmap_kernel_mapping()` 和 `protect_kernel_mapping()` 清除或改写
//!   已存在的同粒度叶子，并产出失效凭据。
//! - `shootdown_kernel_mapping()` 是本地 v1 全局失效钩子。
//!
//! 辅助函数分组：
//! - 预约辅助函数判定某个槽位是已映射、空闲、还是冲突；
//! - 叶子辅助函数实现安全的同粒度解除映射/权限修改；
//! - 已提交的内核 L0/L1 表保持常驻，避免在 shootdown 前回收页表页。
//!
//! 模式约定：薄壳 + `_from_bag`——薄壳取全局根，`_from_bag` 真正干活，也便于
//! 测试注入。这里仍是板卡专属的 Sv39 代码；可移植的范围 API 在
//! `tx_hal::pmap`，带帧记账的 shootdown 在 substrate。参见
//! `docs/progress/decisions/2026-04-28-rv64-direct-map-extension.md`、
//! `docs/progress/decisions/2026-04-28-rv64-mmio-pmap-reserve-commit.md`、
//! `docs/progress/decisions/2026-04-29-pmap-kernel-protect-in-place.md`。

use tx_hal::{
    BootstrapPmapInfo, PhysAddr, PmapError, PmapInvalidation, PmapPermissions, PmapReservation,
    PmapReservationIntermediates, PmapReserveKind, PmapUnmapResult, VirtAddr,
};

use crate::boot_static::{BootStaticBag, IdentityDropped, PageTable};

use super::pt_node::register_committed_intermediates;
use super::{
    align_up, direct_map_virt, encode_kernel_mapping_leaf, encode_leaf_pte,
    encode_leaf_pte_with_permissions, ensure_l0_table_for_reservation,
    ensure_l1_table_for_reservation, l0_table_mut, l1_table_mut, page_table_mut_from_phys,
    pte_is_branch, pte_is_leaf, pte_phys, rollback_intermediates_from_bag, rv64_1g_leaf_index,
    rv64_2m_leaf_index, rv64_4k_leaf_index, sfence_vma_all, validate_aligned_mapping,
    validate_aligned_virt, validate_rv64_leaf_permissions, DIRECT_MAP_SIZE, PAGE_SIZE, PTE_G,
    PTE_R, PTE_W, QEMU_RAM_BASE, SUPERPAGE_1G_SIZE, SUPERPAGE_2M_SIZE,
};

// 引导 pmap 事实与直接映射扩展放在一起：substrate 先消费这些事实，再请板卡
// 扩展直接映射，然后才能把分配器元数据放进那些初始 1 GiB 引导叶子没覆盖到的
// 内存里。
// 返回引导阶段发布的 pmap 事实（直接映射范围等）。
pub(crate) fn bootstrap_pmap_info() -> Option<&'static BootstrapPmapInfo> {
    BootStaticBag::<IdentityDropped>::global_ref().bootstrap_pmap_info_ref()
}

// 薄壳：为直连区某个 1 GiB 物理页预约根叶子（取全局根后转交 `_from_bag`）。
pub(crate) fn reserve_kernel_direct_map_1g(
    phys: PhysAddr,
) -> Result<Option<PmapReservation>, PmapError> {
    reserve_direct_map_1g_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), phys)
}

// 校验对齐后检查根叶子槽位：已是目标返回 None，非空冲突报错，空则返回预约。
pub(super) fn reserve_direct_map_1g_from_bag<State>(
    bag: &BootStaticBag<State>,
    phys: PhysAddr,
) -> Result<Option<PmapReservation>, PmapError> {
    if phys.0 < QEMU_RAM_BASE || !phys.0.is_multiple_of(SUPERPAGE_1G_SIZE) {
        return Err(PmapError::InvalidRequest);
    }

    let virt = direct_map_virt(phys.0);
    let expected = encode_leaf_pte(phys, PTE_R | PTE_W | PTE_G);
    let current = unsafe { bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt)] };
    if current == expected {
        return Ok(None);
    }
    if current != 0 {
        return Err(PmapError::AlreadyMapped);
    }

    Ok(Some(PmapReservation::new(
        VirtAddr(virt),
        phys,
        PmapReserveKind::Superpage1G,
    )))
}

/// Preseed the direct-map root leaf that contains the firmware DTB.
///
/// The trampoline deliberately installs only the first 1 GiB RAM leaf. With
/// large QEMU guests, firmware may place the DTB near the top of RAM, before
/// its memory nodes have been parsed and before substrate can extend the full
/// direct map. This one early leaf is therefore installed without advancing
/// the published contiguous direct-map range; normal substrate extension will
/// later encounter the identical leaf and treat it idempotently.
pub(crate) fn cover_boot_firmware_dtb_from_bag<State>(
    bag: &BootStaticBag<State>,
    dtb_phys: PhysAddr,
) -> Result<(), PmapError> {
    if dtb_phys.0 == 0 {
        return Ok(());
    }
    if dtb_phys.0 < QEMU_RAM_BASE || dtb_phys.0 >= DIRECT_MAP_SIZE {
        return Err(PmapError::InvalidRequest);
    }

    let leaf_phys = dtb_phys.0 - (dtb_phys.0 % SUPERPAGE_1G_SIZE);
    let virt = direct_map_virt(leaf_phys);
    let expected = encode_leaf_pte(PhysAddr(leaf_phys), PTE_R | PTE_W | PTE_G);
    let root = unsafe { bag.bootstrap_root_mut() };
    let slot = &mut root.0[rv64_1g_leaf_index(virt)];
    if *slot == 0 {
        *slot = expected;
    } else if *slot != expected {
        return Err(PmapError::AlreadyMapped);
    }
    sfence_vma_all();
    Ok(())
}

// 薄壳：提交直连区 1 GiB 叶子预约（取全局根后转交 `_from_bag`）。
pub(crate) fn commit_kernel_direct_map_1g(reservation: PmapReservation) {
    commit_direct_map_1g_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), reservation);
}

// 写入根叶子 PTE 并把已发布的直接映射事实向后扩展，最后本地刷 TLB。
pub(super) fn commit_direct_map_1g_from_bag<State>(
    bag: &BootStaticBag<State>,
    reservation: PmapReservation,
) {
    assert_eq!(reservation.kind(), PmapReserveKind::Superpage1G);
    let phys = reservation.phys();
    assert_eq!(reservation.virt(), VirtAddr(direct_map_virt(phys.0)));
    assert!(phys.0.is_multiple_of(SUPERPAGE_1G_SIZE));

    let pte = encode_leaf_pte(phys, PTE_R | PTE_W | PTE_G);
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(reservation.virt().0)] = pte;
        extend_bootstrap_direct_map_info(bag, phys.0 + SUPERPAGE_1G_SIZE);
    }
    sfence_vma_all();
}

// 薄壳：substrate 建堆时把直接映射扩展到 `phys_end`（取全局根后转交）。
pub(crate) fn extend_direct_map(phys_end: PhysAddr) -> Result<(), PmapError> {
    extend_direct_map_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), phys_end)
}

// 从当前直接映射末端起，按 1 GiB 叶子逐个预约并提交，直到覆盖到 `phys_end`。
pub(super) fn extend_direct_map_from_bag<State>(
    bag: &BootStaticBag<State>,
    phys_end: PhysAddr,
) -> Result<(), PmapError> {
    let current_end = direct_map_phys_end_from_bag(bag)?;
    if phys_end.0 <= current_end {
        return Ok(());
    }

    let mut phys = align_up(current_end, SUPERPAGE_1G_SIZE).ok_or(PmapError::InvalidRequest)?;
    let target = align_up(phys_end.0, SUPERPAGE_1G_SIZE).ok_or(PmapError::InvalidRequest)?;
    while phys < target {
        let next_phys = phys
            .checked_add(SUPERPAGE_1G_SIZE)
            .ok_or(PmapError::InvalidRequest)?;
        if let Some(reservation) = reserve_direct_map_1g_from_bag(bag, PhysAddr(phys))? {
            commit_direct_map_1g_from_bag(bag, reservation);
        } else {
            // A bootstrap-only leaf (notably the high firmware-DTB leaf)
            // becomes part of the contiguous direct map once this walk
            // reaches it. The PTE needs no rewrite, but the published range
            // must still advance across the already-identical leaf.
            unsafe {
                extend_bootstrap_direct_map_info(bag, next_phys);
            }
        }
        phys = next_phys;
    }

    Ok(())
}

// 内核映射预约与进程根预约类似，但目标是引导/全局根。新建的中间表在提交前
// 一直挂在预约上，这样若后续某步失败，回滚就能干净地归还这些 PT 节点。
// 薄壳：预约内核映射（投机建中间表，不写叶子；取全局根后转交 `_from_bag`）。
pub(crate) fn reserve_kernel_mapping(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    reserve_kernel_mapping_from_bag(
        BootStaticBag::<IdentityDropped>::global_ref(),
        virt,
        phys,
        kind,
    )
}

// 按粒度（1G/2M/4K）按需建好中间表，检查目标叶子槽位后返回预约；建表失败或
// 槽位冲突时回滚本次新建的中间表。
pub(super) fn reserve_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapReservation>, PmapError> {
    let expected = encode_kernel_mapping_leaf(phys);
    match kind {
        PmapReserveKind::Superpage1G => {
            validate_aligned_mapping(virt, phys, SUPERPAGE_1G_SIZE)?;
            let current = unsafe { bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] };
            reservation_for_slot(
                virt,
                phys,
                kind,
                current,
                expected,
                PmapReservationIntermediates::empty(),
            )
        }
        PmapReserveKind::Superpage2M => {
            validate_aligned_mapping(virt, phys, SUPERPAGE_2M_SIZE)?;
            let l1 = ensure_l1_table_for_reservation(bag, virt)?;
            let intermediates = PmapReservationIntermediates {
                l2: None,
                l1: l1.node,
                l0: None,
            };
            let current = l1.table.0[rv64_2m_leaf_index(virt.0)];
            match reservation_for_slot(virt, phys, kind, current, expected, intermediates) {
                Ok(reservation) => Ok(reservation),
                Err(error) => {
                    rollback_intermediates_from_bag(bag, virt, intermediates);
                    Err(error)
                }
            }
        }
        PmapReserveKind::Page4K => {
            validate_aligned_mapping(virt, phys, PAGE_SIZE)?;
            let l1 = ensure_l1_table_for_reservation(bag, virt)?;
            let l1_node = l1.node;
            let l0 = match ensure_l0_table_for_reservation(bag, l1.table, virt) {
                Ok(l0) => l0,
                Err(error) => {
                    rollback_intermediates_from_bag(
                        bag,
                        virt,
                        PmapReservationIntermediates {
                            l2: None,
                            l1: l1_node,
                            l0: None,
                        },
                    );
                    return Err(error);
                }
            };
            let intermediates = PmapReservationIntermediates {
                l2: None,
                l1: l1_node,
                l0: l0.node,
            };
            let current = l0.table.0[rv64_4k_leaf_index(virt.0)];
            match reservation_for_slot(virt, phys, kind, current, expected, intermediates) {
                Ok(reservation) => Ok(reservation),
                Err(error) => {
                    rollback_intermediates_from_bag(bag, virt, intermediates);
                    Err(error)
                }
            }
        }
    }
}

// 预约之后，commit/rollback 负责发布或放弃内核映射。已提交的中间表会被登记，
// 这样后续 unmap/prune 才能从一个裸分支 PTE 恢复出对应的 `PtNode` 所有权。
// 薄壳：回滚一次内核映射预约（取全局根后转交 `_from_bag`）。
pub(crate) fn rollback_kernel_mapping(reservation: PmapReservation) {
    rollback_kernel_mapping_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), reservation);
}

// 归还预约投机建出的中间表，然后本地刷 TLB。
pub(super) fn rollback_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    reservation: PmapReservation,
) {
    rollback_intermediates_from_bag(bag, reservation.virt(), reservation.intermediates());
    sfence_vma_all();
}

// 薄壳：提交内核映射（写叶子并登记中间表；取全局根后转交 `_from_bag`）。
pub(crate) fn commit_kernel_mapping(reservation: PmapReservation, permissions: PmapPermissions) {
    commit_kernel_mapping_from_bag(
        BootStaticBag::<IdentityDropped>::global_ref(),
        reservation,
        permissions,
    );
}

/// Publish a reservation for a leaf slot that was proven empty without an
/// unnecessary `sfence.vma`. This is the vmap population path: there cannot be
/// a stale valid translation for a previously unmapped address.
pub(crate) fn commit_new_kernel_mapping(
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    publish_kernel_mapping_from_bag(
        BootStaticBag::<IdentityDropped>::global_ref(),
        reservation,
        permissions,
    );
}

// 登记中间表所有权，按粒度写入叶子 PTE，最后本地刷 TLB。
pub(super) fn commit_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    publish_kernel_mapping_from_bag(bag, reservation, permissions);
    sfence_vma_all();
}

fn publish_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    reservation: PmapReservation,
    permissions: PmapPermissions,
) {
    validate_rv64_leaf_permissions(permissions, false).expect("valid kernel leaf permissions");
    register_committed_intermediates(reservation.intermediates());
    let pte = encode_leaf_pte_with_permissions(reservation.phys(), permissions);
    match reservation.kind() {
        PmapReserveKind::Superpage1G => unsafe {
            bag.bootstrap_root_mut().0[rv64_1g_leaf_index(reservation.virt().0)] = pte;
        },
        PmapReserveKind::Superpage2M => {
            let l1 = l1_table_mut(bag, reservation.virt()).expect("reserved L1 table must exist");
            l1.0[rv64_2m_leaf_index(reservation.virt().0)] = pte;
        }
        PmapReserveKind::Page4K => {
            let l1 = l1_table_mut(bag, reservation.virt()).expect("reserved L1 table must exist");
            let l0 = l0_table_mut(l1, reservation.virt()).expect("reserved L0 table must exist");
            l0.0[rv64_4k_leaf_index(reservation.virt().0)] = pte;
        }
    }
}

// 内核 unmap/protect 只作用于已存在的同粒度叶子。空槽位是 no-op；拆分大页这类
// 不安全变换会被拒绝，留给上层 VM 策略之后靠缺页重新物化。
// 薄壳：解除内核映射（取全局根后转交 `_from_bag`）。
pub(crate) fn unmap_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    unmap_kernel_mapping_from_bag(BootStaticBag::<IdentityDropped>::global_ref(), virt, kind)
}

// 定位到叶子槽位并清零（只动同粒度已存在叶子）。已提交的内核中间表保持
// 常驻；vmalloc 窗口有界，这避免了在上层 shootdown 之前回收页表页。
pub(super) fn unmap_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    validate_aligned_virt(virt, kind.size())?;
    match kind {
        PmapReserveKind::Superpage1G => Err(PmapError::InvalidRequest),
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            {
                let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)
            }
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            let l1_slot = l1.0[rv64_2m_leaf_index(virt.0)];
            if l1_slot == 0 {
                return Ok(None);
            }
            if !pte_is_branch(l1_slot) {
                return Err(PmapError::InvalidRequest);
            }
            let l0 = unsafe { page_table_mut_from_phys(pte_phys(l1_slot)) };
            {
                let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
                unmap_leaf_slot(slot, virt, kind)
            }
        }
    }
}

// 薄壳：修改内核映射叶子的权限（取全局根后转交 `_from_bag`）。
pub(crate) fn protect_kernel_mapping(
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    protect_kernel_mapping_from_bag(
        BootStaticBag::<IdentityDropped>::global_ref(),
        virt,
        kind,
        permissions,
    )
}

// 定位同粒度已存在叶子并改写其权限位，返回失效凭据（不在此刷 TLB）。
pub(super) fn protect_kernel_mapping_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    validate_rv64_leaf_permissions(permissions, false)?;
    validate_aligned_virt(virt, kind.size())?;
    match kind {
        PmapReserveKind::Superpage1G => Err(PmapError::InvalidRequest),
        PmapReserveKind::Superpage2M => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
        PmapReserveKind::Page4K => {
            let Some(l1) = l1_table_mut(bag, virt) else {
                return Ok(None);
            };
            let l1_slot = l1.0[rv64_2m_leaf_index(virt.0)];
            if l1_slot == 0 {
                return Ok(None);
            }
            if !pte_is_branch(l1_slot) {
                return Err(PmapError::InvalidRequest);
            }
            let l0 = unsafe { page_table_mut_from_phys(pte_phys(l1_slot)) };
            let slot = &mut l0.0[rv64_4k_leaf_index(virt.0)];
            protect_leaf_slot(slot, virt, kind, permissions)
        }
    }
}

// 内核映射失效执行：本地 sfence.vma 全刷（远程核的 IPI 在 lib.rs 里补）。
pub(crate) fn shootdown_kernel_mapping(_invalidation: PmapInvalidation) {
    sfence_vma_all();
}

// 直接映射辅助函数让对外的扩展路径保持精简：一个从已发布的 HAL 事实算出映射的
// 物理末端，另一个在根叶子提交后更新这些事实。
// 从已发布事实算出直接映射覆盖到的物理末端地址。
pub(super) fn direct_map_phys_end_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<usize, PmapError> {
    let Some(info) = bag.bootstrap_pmap_info_ref() else {
        return Err(PmapError::InvalidRequest);
    };
    let phys_start = info
        .direct_map
        .start
        .0
        .checked_sub(info.direct_map_base.0)
        .ok_or(PmapError::InvalidRequest)?;
    phys_start
        .checked_add(info.direct_map.size)
        .ok_or(PmapError::InvalidRequest)
}

// 从已发布事实算出直接映射覆盖的物理起始地址。
pub(super) fn direct_map_phys_start_from_bag<State>(
    bag: &BootStaticBag<State>,
) -> Result<usize, PmapError> {
    let Some(info) = bag.bootstrap_pmap_info_ref() else {
        return Err(PmapError::InvalidRequest);
    };
    info.direct_map
        .start
        .0
        .checked_sub(info.direct_map_base.0)
        .ok_or(PmapError::InvalidRequest)
}

/// 用 1 GiB 叶子覆盖引导直接映射起点以下的那段 RAM。
///
/// 引导页表只映射了 QEMU-virt 位于 0x8000_0000 的那 1 GiB，但像
/// VisionFive 2 这类板卡在设备树里报告 DDR 从 0x4000_0000 开始。
/// 本函数在引导事实发布期间调用（此时恒等映射仍在、根可写）：补写
/// 缺失的根叶子，并把已发布的 direct_map/mapped 范围下压，使 substrate
/// 覆盖检查和帧分配器看到真实跨度。QEMU 上是 no-op（区域正好从当前基址起）。
pub(crate) fn cover_direct_map_low_from_bag<State>(
    bag: &BootStaticBag<State>,
    lowest_phys: PhysAddr,
) -> Result<(), PmapError> {
    let current_start = direct_map_phys_start_from_bag(bag)?;
    let new_start = lowest_phys.0 - (lowest_phys.0 % SUPERPAGE_1G_SIZE);
    if new_start >= current_start {
        return Ok(());
    }

    let mut phys = new_start;
    while phys < current_start {
        let virt = direct_map_virt(phys);
        let expected = encode_leaf_pte(PhysAddr(phys), PTE_R | PTE_W | PTE_G);
        let root = unsafe { bag.bootstrap_root_mut() };
        let slot = &mut root.0[rv64_1g_leaf_index(virt)];
        if *slot == 0 {
            *slot = expected;
        } else if *slot != expected {
            return Err(PmapError::AlreadyMapped);
        }
        phys = phys
            .checked_add(SUPERPAGE_1G_SIZE)
            .ok_or(PmapError::InvalidRequest)?;
    }

    unsafe {
        lower_bootstrap_direct_map_info(bag, new_start, current_start);
    }
    sfence_vma_all();
    Ok(())
}

// 把已发布的直接映射事实起点下压到 new_start，并相应增大 size。
unsafe fn lower_bootstrap_direct_map_info<State>(
    bag: &BootStaticBag<State>,
    new_start: usize,
    old_start: usize,
) {
    let grown = old_start - new_start;
    unsafe {
        let Some(info) = bag.bootstrap_pmap_info_mut().as_mut() else {
            return;
        };
        info.direct_map.start = VirtAddr(info.direct_map_base.0 + new_start);
        info.direct_map.size += grown;
        info.mapped.start = PhysAddr(new_start);
        info.mapped.size += grown;
    }
}

// 把已发布的直接映射事实末端向后扩展到 phys_end（取 max，不缩小）。
unsafe fn extend_bootstrap_direct_map_info<State>(bag: &BootStaticBag<State>, phys_end: usize) {
    unsafe {
        let Some(info) = bag.bootstrap_pmap_info_mut().as_mut() else {
            return;
        };
        let Some(phys_start) = info.direct_map.start.0.checked_sub(info.direct_map_base.0) else {
            return;
        };
        if phys_end <= phys_start {
            return;
        }
        info.direct_map.size = info.direct_map.size.max(phys_end - phys_start);
        info.mapped.size = info.mapped.size.max(phys_end - info.mapped.start.0);
    }
}

// 预约辅助函数刻意保持几乎无副作用：把候选槽位归类为“已满足”、“冲突”、或
// “空闲可预约”，并让预约凭据继续持有本次新建的中间表。
// 按当前/期望 PTE 分类槽位：相等返回 None，非空冲突报错，空则生成预约凭据。
fn reservation_for_slot(
    virt: VirtAddr,
    phys: PhysAddr,
    kind: PmapReserveKind,
    current: u64,
    expected: u64,
    intermediates: PmapReservationIntermediates,
) -> Result<Option<PmapReservation>, PmapError> {
    if current == expected {
        return Ok(None);
    }
    if current != 0 {
        return Err(PmapError::AlreadyMapped);
    }
    Ok(Some(PmapReservation::new_with_intermediates(
        virt,
        phys,
        kind,
        intermediates,
    )))
}

// 叶子辅助函数由内核和进程根 pmap 代码共用。它们只接受恰好请求粒度的已存在
// 叶子；空叶子是良性 no-op，分支/拆分情形交给 VM 重新物化策略处理。
// 清除一个同粒度叶子槽位：空则 no-op，非叶子或错位报错，成功返回解除结果。
pub(super) fn unmap_leaf_slot(
    slot: &mut u64,
    virt: VirtAddr,
    kind: PmapReserveKind,
) -> Result<Option<PmapUnmapResult>, PmapError> {
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = pte_phys(current);
    if !phys.0.is_multiple_of(kind.size()) {
        return Err(PmapError::InvalidRequest);
    }
    *slot = 0;
    Ok(Some(PmapUnmapResult::new(virt, phys, kind)))
}

// 改写一个同粒度叶子槽位的权限：空则 no-op，非叶子/错位报错；权限不变返回
// None，否则写入新 PTE 并返回失效凭据。
pub(super) fn protect_leaf_slot(
    slot: &mut u64,
    virt: VirtAddr,
    kind: PmapReserveKind,
    permissions: PmapPermissions,
) -> Result<Option<PmapInvalidation>, PmapError> {
    let current = *slot;
    if current == 0 {
        return Ok(None);
    }
    if !pte_is_leaf(current) {
        return Err(PmapError::InvalidRequest);
    }
    let phys = pte_phys(current);
    if !phys.0.is_multiple_of(kind.size()) {
        return Err(PmapError::InvalidRequest);
    }

    let updated = encode_leaf_pte_with_permissions(phys, permissions);
    if current == updated {
        return Ok(None);
    }
    *slot = updated;
    Ok(Some(PmapInvalidation::new(virt, kind.size())))
}
