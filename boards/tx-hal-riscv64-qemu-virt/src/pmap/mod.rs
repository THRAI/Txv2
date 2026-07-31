//! RV64 QEMU 页表门面。
//!
//! 本模块是通过 `PmapIf` 对外暴露的、板级私有的 pmap 入口点。
//!
//! 这里维护的核心数据结构：
//! - `BootStaticBag<IdentityLive>` / `BootStaticBag<IdentityDropped>`：
//!   低地址到高地址引导迁移的类型状态（typestate）载体。
//! - `PageTable`：所有 pmap 模块共享的板级页表页形态。
//! - `EnsuredTable`：投机建出的 L1/L0 表，外加必须提交或回滚的 PT-node 授权。
//! - `HighSentinel`：在任何后续拆除恒等映射之前，证明高地址别名已生效的
//!   PC/SP/GP 三元组凭据。
//!
//! 主要数据流函数：
//! - 汇编跳板在启用 Sv39 之前构建第一份低 LMA 引导页表。
//! - `adopt_high_linked_bootstrap_pmap()` 将这些表接管为 HAL 事实，并在 Rust
//!   已运行于高地址后精修高内核别名的叶子项。
//! - `complete_post_entry_pipeline()` 校验高地址哨兵，并拆除临时的低地址恒等桥。
//!
//! 助手分组：
//! - 中间表助手：投机建出并回滚分支表；
//! - 表查找助手：把分支 PTE 解码为 `PageTable` 引用；
//! - 本地维护助手：`sfence.vma`、引导池索引、以及 PT-node 页清零。
//!
//! 各职责专属子模块分别拥有进程根、内核映射、PT-node 所有权、PTE 编码和拓扑常量。
//! 参见 `docs/progress/decisions/2026-04-29-rv64-pmap-module-extraction.md` 和
//! `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`。

use tx_hal::{
    BootstrapPmapInfo, PhysAddr, PhysRange, PmapError, PmapReservationIntermediates, PtNode,
    VirtAddr, VirtRange,
};

use crate::boot_static::{BootStaticBag, IdentityDropped, IdentityLive, PageTable};

mod address_space;
mod kernel_space;
mod pt_node;
mod pte;

pub(crate) mod topology;

pub(crate) use address_space::{
    commit_mapping, create_pmap_root, destroy_pmap_root, protect_mapping, reserve_mapping,
    rollback_mapping, shootdown_mapping, shootdown_mappings, synchronize_new_mappings,
    unmap_mapping, ASID_CAPACITY,
};
#[cfg(target_arch = "riscv64")]
pub(crate) use kernel_space::cover_boot_firmware_dtb_from_bag;
pub(crate) use kernel_space::{
    bootstrap_pmap_info, commit_kernel_direct_map_1g, commit_kernel_mapping, extend_direct_map,
    protect_kernel_mapping, reserve_kernel_direct_map_1g, reserve_kernel_mapping,
    rollback_kernel_mapping, shootdown_kernel_mapping, unmap_kernel_mapping,
};
pub(crate) use pt_node::{alloc_pt_node, free_pt_node, install_pt_node_allocator};
use pt_node::{alloc_pt_node_from_bag, free_pt_node_from_bag};
use pte::*;
use topology::*;

// 引导阶段 Sv39 地址空间布局：
//
//   低规范半区（lower canonical half）
//   0x0000_0000_0000_0000
//        | 虚拟内存就绪后，用户映射落在此处
//        |
//        +-- 0x0000_0000_8000_0000  QEMU RAM 的临时恒等桥
//        |                           根槽位 2，1 GiB 叶，仅引导期使用
//        |
//        +-- USER_ALLOC_TOP          普通用户分配上限
//        +-- USER_TOP - 4 MiB        预留的助手页带
//        |                           未来的 signal/trampoline/VDSO 页
//   0x0000_0040_0000_0000  USER_TOP
//
//   高规范半区（upper canonical half）
//   0xffff_ffc0_0000_0000  DIRECT_MAP_BASE
//        +-- +0x8000_0000           QEMU RAM 的直接映射别名
//        |                           根槽位 258，1 GiB 叶
//        |
//        +-- ...                     未来的 RAM/MMIO 直接映射扩展
//        |
//        +-- 0xffff_ffff_8020_0000   高内核别名
//                                    根槽位 510 -> 引导 L1，
//                                    此首片使用 2 MiB 叶
//
// 进程根会把低半区留作 AddressSpace 私有，而共享/复制高半区的内核条目。
// BSP 通过高内核别名进入 Rust。对这份低地址链接（low-linked）的 Rust 镜像，
// 低地址恒等叶特意保持存活，因为编译器生成的绝对地址表仍可能指向低地址的
// 代码；显式拆除被推迟到高地址链接（high-linker）的那一片再做。

// 高地址哨兵校验失败的原因：pc/sp/gp 三者中哪一个还未进入高别名区。
#[cfg(any(test, target_arch = "riscv64"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HighSentinelError {
    ProgramCounter,
    StackPointer,
    GlobalPointer,
}

/// reserve 路径返回的 L1/L0 表，外加新分配的 PT-node 授权：
/// 除非该 reserve 最终提交，否则该 node 必须回滚。
struct EnsuredTable {
    table: &'static mut PageTable,
    node: Option<PtNode>,
}

/// 拆除恒等映射前哨兵使用的寄存器快照。
///
/// 在真实 RV64 上捕获当前的 PC、SP、GP 寄存器；测试中则由传入的值构造，
/// 以便覆盖同一条校验路径。
#[cfg(any(test, target_arch = "riscv64"))]
#[derive(Clone, Copy)]
struct HighSentinel {
    pc: usize,
    sp: usize,
    gp: usize,
}

// IdentityLive 状态的 bag 把 H1/H2 迁移建模为一串"借出后归还"的步骤。
// 在高 VMA/低 LMA 路径下，进入 Rust 前的那半段是纯汇编，Rust 只在高别名生效
// 之后才开始运行。随后 Rust 接管/精修由汇编构建的表、发布 pmap 事实，最终在
// BootInfo 消费完固件指针之后移除临时恒等桥。
//
// 门面入口：接管精修高内核别名（委托给 bag 的同名方法）。
pub(crate) fn adopt_high_linked_bootstrap_pmap(bag: &mut BootStaticBag<IdentityLive>) {
    bag.adopt_high_linked_bootstrap_pmap();
}

// 装回次级恒等桥：在引导根槽位重新写入 QEMU RAM 的 1 GiB 恒等叶（RWX）并全刷 TLB。
pub(crate) fn install_secondary_identity_bridge() {
    let bag = BootStaticBag::<IdentityDropped>::global_ref();
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] =
            encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_X);
    }
    sfence_vma_all();
}

// 移除次级恒等桥：清零该恒等叶、抹掉 pmap info 中的 identity 记录并全刷 TLB。
pub(crate) fn remove_secondary_identity_bridge() {
    let bag = BootStaticBag::<IdentityDropped>::global_ref();
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] = 0;
        if let Some(info) = bag.bootstrap_pmap_info_mut().as_mut() {
            info.identity = None;
        }
    }
    sfence_vma_all();
}

impl BootStaticBag<IdentityLive> {
    // 接管高地址链接的引导 pmap：先精修高内核别名叶子，再发布 pmap 事实。
    pub(crate) fn adopt_high_linked_bootstrap_pmap(&mut self) -> &mut Self {
        self.refine_kernel_high_alias()
            .publish_bootstrap_pmap_info()
    }

    // 精修高内核别名：逐 4K 页遍历内核镜像物理范围，按所属段以 W^X 权限
    // 写入对应 L0 表的叶子项（把汇编建的粗粒度别名细化成分段精确权限）。
    fn refine_kernel_high_alias(&mut self) -> &mut Self {
        unsafe {
            let image = self.kernel_image_phys();
            let Some(image_end) = align_up(image.end().0, PAGE_SIZE) else {
                return self;
            };
            let alias_end = QEMU_KERNEL_PHYS_BASE + KERNEL_BOOTSTRAP_ALIAS_SIZE;
            let mut phys = image.start.0;
            while phys < image_end.min(alias_end) {
                if phys >= QEMU_KERNEL_PHYS_BASE {
                    let offset = phys - QEMU_KERNEL_PHYS_BASE;
                    let table_index = offset / SUPERPAGE_2M_SIZE;
                    if table_index < KERNEL_ALIAS_L0_TABLES {
                        let virt = KERNEL_VIRT_BASE + offset;
                        let permissions = kernel_alias_permissions_for_phys(self, phys);
                        let table = self.kernel_alias_l0_mut(table_index);
                        table.0[rv64_4k_leaf_index(virt)] =
                            encode_leaf_pte_with_permissions(PhysAddr(phys), permissions);
                    }
                }
                phys += PAGE_SIZE;
            }
        }
        self
    }

    // 测试用：清零引导根表与内核别名 L1 表，作为构建的起点。
    #[cfg(test)]
    fn begin_bootstrap_pmap(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0.fill(0);
            self.kernel_alias_l1_mut().0.fill(0);
        }
        self
    }

    // 测试用：在引导根写入 QEMU RAM 的 1 GiB 恒等叶（RWX）。
    #[cfg(test)]
    fn map_identity_bridge(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] =
                encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_X);
        }
        self
    }

    // 测试用：在引导根写入 QEMU RAM 的直接映射别名叶（RW + 全局位 G）。
    #[cfg(test)]
    fn map_direct_map_window(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(direct_map_virt(QEMU_RAM_BASE))] =
                encode_leaf_pte(PhysAddr(QEMU_RAM_BASE), PTE_R | PTE_W | PTE_G);
        }
        self
    }

    // 测试用：完整搭建高内核别名——根槽指向 L1，L1 各项指向 L0，再逐 4K 页
    // 按段权限填入内核镜像叶子（在宿主上复现汇编跳板 + 精修的整套结构）。
    #[cfg(test)]
    fn map_kernel_high_alias(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(KERNEL_VIRT_BASE)] =
                encode_branch_pte(self.kernel_alias_l1_phys());

            self.kernel_alias_l1_mut().0.fill(0);
            for table_index in 0..KERNEL_ALIAS_L0_TABLES {
                let table = self.kernel_alias_l0_mut(table_index);
                table.0.fill(0);

                let virt = KERNEL_VIRT_BASE + table_index * SUPERPAGE_2M_SIZE;
                self.kernel_alias_l1_mut().0[rv64_2m_leaf_index(virt)] =
                    encode_branch_pte(self.kernel_alias_l0_phys(table_index));
            }

            let image = self.kernel_image_phys();
            let Some(image_end) = align_up(image.end().0, PAGE_SIZE) else {
                return self;
            };
            let alias_end = QEMU_KERNEL_PHYS_BASE + KERNEL_BOOTSTRAP_ALIAS_SIZE;
            let mut phys = image.start.0;
            while phys < image_end.min(alias_end) {
                if phys >= QEMU_KERNEL_PHYS_BASE {
                    let offset = phys - QEMU_KERNEL_PHYS_BASE;
                    let virt = KERNEL_VIRT_BASE + offset;
                    let table_index = offset / SUPERPAGE_2M_SIZE;
                    let table = self.kernel_alias_l0_mut(table_index);
                    table.0[rv64_4k_leaf_index(virt)] = encode_leaf_pte_with_permissions(
                        PhysAddr(phys),
                        kernel_alias_permissions_for_phys(self, phys),
                    );
                }
                phys += PAGE_SIZE;
            }
        }
        self
    }

    // 发布引导 pmap 事实：登记预留页表清单，并填好供 HAL/BootInfo 使用的
    // BootstrapPmapInfo（根表、已映射区、直接映射、内核镜像、恒等区、PT-node 池等）。
    fn publish_bootstrap_pmap_info(&mut self) -> &mut Self {
        let root = self.bootstrap_root_phys();
        let kernel_alias_l1 = self.kernel_alias_l1_phys();
        let kernel_alias_l0 = self.kernel_alias_l0_phys_range();
        let pt_node_pool = self.pt_node_pool_phys_range();
        unsafe {
            let reserved_page_tables = self.reserved_page_tables_mut();
            *reserved_page_tables = [
                PhysRange {
                    start: root,
                    size: PAGE_SIZE,
                },
                PhysRange {
                    start: kernel_alias_l1,
                    size: PAGE_SIZE,
                },
                kernel_alias_l0,
                pt_node_pool,
            ];

            *self.bootstrap_pmap_info_mut() = Some(BootstrapPmapInfo {
                root,
                mapped: PhysRange {
                    start: PhysAddr(QEMU_RAM_BASE),
                    size: QEMU_BOOTSTRAP_MAP_SIZE,
                },
                direct_map_base: VirtAddr(DIRECT_MAP_BASE),
                direct_map: VirtRange {
                    start: VirtAddr(direct_map_virt(QEMU_RAM_BASE)),
                    size: QEMU_BOOTSTRAP_MAP_SIZE,
                },
                kernel_image: VirtRange {
                    start: VirtAddr(KERNEL_VIRT_BASE),
                    size: KERNEL_BOOTSTRAP_ALIAS_SIZE,
                },
                identity: Some(VirtRange {
                    start: VirtAddr(QEMU_RAM_BASE),
                    size: QEMU_BOOTSTRAP_MAP_SIZE,
                }),
                pt_node_pool,
                reserved_page_tables: &reserved_page_tables[..],
            });
        }
        self
    }

    // 高地址哨兵：校验当前 pc/sp/gp 是否都已落在高别名区，否则原地自旋。
    #[cfg(target_arch = "riscv64")]
    pub(crate) fn require_current_high_sentinel_or_spin(self) -> Self {
        // 引导路径开始依赖高别名之前的最后一道防线：若高地址跳转或 sp/gp
        // 改写出现回退，就在低地址内存仍映射着的时候自旋（便于诊断而非跑飞）。
        match self.require_high_sentinel(HighSentinel::current()) {
            Ok(bag) => bag,
            Err(_) => loop {
                core::hint::spin_loop();
            },
        }
    }

    // 宿主（非 RV64）路径：无 MMU，直接转入 IdentityDropped 状态。
    #[cfg(not(target_arch = "riscv64"))]
    pub(crate) fn finish_host_boot_without_mmu(self) -> BootStaticBag<IdentityDropped> {
        self.into_dropped()
    }

    // 拆桥总管：进入 Rust 后的收尾流水线——RV64 上先过高地址哨兵再拆低恒等桥，
    // 宿主上则走无 MMU 的收尾路径。
    pub(crate) fn complete_post_entry_pipeline(self) -> BootStaticBag<IdentityDropped> {
        #[cfg(target_arch = "riscv64")]
        {
            self.require_current_high_sentinel_or_spin().drop_lower()
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            self.finish_host_boot_without_mmu()
        }
    }

    // 测试用：以给定 pc/sp/gp 走一遍哨兵校验路径（不改状态）。
    #[cfg(test)]
    pub(crate) fn validate_high_values(
        &self,
        pc: usize,
        sp: usize,
        gp: usize,
    ) -> Result<&Self, HighSentinelError> {
        validate_high_sentinel(HighSentinel { pc, sp, gp })?;
        Ok(self)
    }

    // 测试用：以给定 pc/sp/gp 过哨兵后拆低恒等桥并转入 IdentityDropped。
    #[cfg(test)]
    pub(crate) fn drop_lower_after_high_values(
        self,
        pc: usize,
        sp: usize,
        gp: usize,
    ) -> Result<BootStaticBag<IdentityDropped>, HighSentinelError> {
        Ok(self
            .require_high_sentinel(HighSentinel { pc, sp, gp })?
            .drop_lower())
    }

    // 哨兵校验：通过则原样返回 self，失败返回错误（由调用方决定自旋或报错）。
    #[cfg(any(test, target_arch = "riscv64"))]
    fn require_high_sentinel(self, sentinel: HighSentinel) -> Result<Self, HighSentinelError> {
        validate_high_sentinel(sentinel)?;
        Ok(self)
    }

    // 拆低恒等桥并把类型状态从 IdentityLive 迁移为 IdentityDropped。
    #[cfg(any(test, target_arch = "riscv64"))]
    pub(crate) fn drop_lower(mut self) -> BootStaticBag<IdentityDropped> {
        self.drop_identity_bridge();
        self.into_dropped()
    }

    // 拆恒等桥：清零根表中 QEMU RAM 的恒等叶、抹掉 pmap info 的 identity 记录，
    // 然后 sfence.vma 全刷本核 TLB 让改动生效。
    #[cfg(any(test, target_arch = "riscv64"))]
    fn drop_identity_bridge(&mut self) -> &mut Self {
        unsafe {
            self.bootstrap_root_mut().0[rv64_1g_leaf_index(QEMU_RAM_BASE)] = 0;

            if let Some(info) = self.bootstrap_pmap_info_mut().as_mut() {
                info.identity = None;
            }
        }
        sfence_vma_all();
        self
    }
}

// 哨兵刻意只做窄校验：仅证明控制流（pc）以及两个仍可能引用低地址的
// "准静态"寄存器（sp、gp）都已跨入内核别名区间。
#[cfg(target_arch = "riscv64")]
impl HighSentinel {
    // 采样当前 pc/sp/gp（内联汇编读取），构造哨兵快照。
    fn current() -> Self {
        let pc: usize;
        let sp: usize;
        let gp: usize;

        unsafe {
            core::arch::asm!("auipc {pc}, 0", pc = out(reg) pc, options(nomem, nostack));
            core::arch::asm!("mv {sp}, sp", sp = out(reg) sp, options(nomem, nostack));
            core::arch::asm!("mv {gp}, gp", gp = out(reg) gp, options(nomem, nostack));
        }

        Self { pc, sp, gp }
    }
}

// 逐一校验 pc/sp/gp 是否都落在高内核别名区间，任一未过则返回对应错误。
#[cfg(any(test, target_arch = "riscv64"))]
fn validate_high_sentinel(sentinel: HighSentinel) -> Result<(), HighSentinelError> {
    if !is_high_kernel_alias(sentinel.pc) {
        return Err(HighSentinelError::ProgramCounter);
    }
    if !is_high_kernel_alias(sentinel.sp) {
        return Err(HighSentinelError::StackPointer);
    }
    if !is_high_kernel_alias(sentinel.gp) {
        return Err(HighSentinelError::GlobalPointer);
    }
    Ok(())
}

// 判断给定地址值是否落在高内核别名区间 [基址, 基址+别名大小)。
#[cfg(any(test, target_arch = "riscv64"))]
fn is_high_kernel_alias(value: usize) -> bool {
    (KERNEL_VIRT_BASE..KERNEL_VIRT_BASE + KERNEL_BOOTSTRAP_ALIAS_SIZE).contains(&value)
}

// 内核映射共享的中间表构建助手。这些函数投机地写入分支 PTE，并把新分配的
// PT-node 装进 reserve 令牌里，从而当 reserve 被放弃时可以回滚而不泄漏页表帧。
//
// 确保 virt 对应的 L1 表存在：已存在则直接返回（node=None）；否则新建 PT-node、
// 在根表写入分支 PTE 并返回带 node 的 EnsuredTable（供后续提交或回滚）。
fn ensure_l1_table_for_reservation<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Result<EnsuredTable, PmapError> {
    if let Some(table) = l1_table_mut(bag, virt) {
        return Ok(EnsuredTable { table, node: None });
    }

    let node = alloc_pt_node_from_bag(bag).map_err(|_| PmapError::Exhausted)?;
    unsafe {
        bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] = encode_branch_pte(node.phys);
    }
    let Some(table) = l1_table_mut(bag, virt) else {
        unsafe {
            bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] = 0;
        }
        free_pt_node_from_bag(bag, node);
        return Err(PmapError::InvalidRequest);
    };
    Ok(EnsuredTable {
        table,
        node: Some(node),
    })
}

// 确保 L1 表下 virt 对应的 L0 表存在：已存在则直接返回；若该槽已是叶（非分支）
// 则报 AlreadyMapped；否则新建 PT-node、写入分支 PTE 并返回带 node 的 EnsuredTable。
fn ensure_l0_table_for_reservation<State>(
    bag: &BootStaticBag<State>,
    l1: &mut PageTable,
    virt: VirtAddr,
) -> Result<EnsuredTable, PmapError> {
    if let Some(table) = l0_table_mut(l1, virt) {
        return Ok(EnsuredTable { table, node: None });
    }

    let index = rv64_2m_leaf_index(virt.0);
    if l1.0[index] != 0 {
        return Err(PmapError::AlreadyMapped);
    }

    let node = alloc_pt_node_from_bag(bag).map_err(|_| PmapError::Exhausted)?;
    l1.0[index] = encode_branch_pte(node.phys);
    let Some(table) = l0_table_mut(l1, virt) else {
        l1.0[index] = 0;
        free_pt_node_from_bag(bag, node);
        return Err(PmapError::InvalidRequest);
    };
    Ok(EnsuredTable {
        table,
        node: Some(node),
    })
}

// 回滚 reserve 期间投机建出的中间表：先撤 L0（把 L1 中匹配的分支槽清零并释放
// node），再撤 L1（把根表中匹配的分支槽清零并释放 node）；仅当槽仍指向该 node
// 时才清，避免误伤已被他人占用的槽。
fn rollback_intermediates_from_bag<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
    intermediates: PmapReservationIntermediates,
) {
    if let Some(l0) = intermediates.l0 {
        if let Some(l1) = l1_table_mut(bag, virt) {
            let slot = &mut l1.0[rv64_2m_leaf_index(virt.0)];
            if pte_is_branch(*slot) && pte_phys(*slot) == l0.phys {
                *slot = 0;
            }
        }
        free_pt_node_from_bag(bag, l0);
    }

    if let Some(l1) = intermediates.l1 {
        let slot = unsafe { &mut bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] };
        if pte_is_branch(*slot) && pte_phys(*slot) == l1.phys {
            *slot = 0;
        }
        free_pt_node_from_bag(bag, l1);
    }
}

// 表查找助手：把分支 PTE 解码成直接映射指针。它们刻意精简，供内核空间操作与
// 测试共享；进程根在 `address_space` 里有自己的根相对版本。
//
// 从引导根表取 virt 对应的 L1 表：该槽是分支 PTE 才返回可变引用，否则 None。
fn l1_table_mut<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Option<&'static mut PageTable> {
    let pte = unsafe { bag.bootstrap_root_mut().0[rv64_1g_leaf_index(virt.0)] };
    if !pte_is_branch(pte) {
        return None;
    }
    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

// 从 L1 表取 virt 对应的 L0 表：该槽是分支 PTE 才返回可变引用，否则 None。
fn l0_table_mut(l1: &mut PageTable, virt: VirtAddr) -> Option<&'static mut PageTable> {
    let pte = l1.0[rv64_2m_leaf_index(virt.0)];
    if !pte_is_branch(pte) {
        return None;
    }
    Some(unsafe { page_table_mut_from_phys(pte_phys(pte)) })
}

// 测试用：L1 表的只读查找封装。
#[cfg(test)]
fn l1_table_for_test<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Option<&'static PageTable> {
    l1_table_mut(bag, virt).map(|table| &*table)
}

// 测试用：先取 L1 再取 L0 的只读查找封装。
#[cfg(test)]
fn l0_table_for_test<State>(
    bag: &BootStaticBag<State>,
    virt: VirtAddr,
) -> Option<&'static PageTable> {
    let l1 = l1_table_mut(bag, virt)?;
    l0_table_mut(l1, virt).map(|table| &*table)
}

// 底层表维护与引导池指针助手。此 v1 路径的 `sfence.vma` 只做本核失效；跨核
// shootdown 仍作为后续 substrate 阻塞项记录在进度备忘里。
//
// 全刷本核 TLB（不带地址/ASID）。
pub(crate) fn sfence_vma_all() {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("sfence.vma", options(nostack));
    }
}

// 按 ASID 对给定虚拟范围逐页 sfence.vma，只失效该地址段的本核 TLB 项。
pub(crate) fn sfence_vma_range_asid(virt: VirtAddr, size: usize, asid: tx_hal::Asid) {
    #[cfg(target_arch = "riscv64")]
    unsafe {
        let mut addr = virt.0;
        let end = virt.0.saturating_add(size);
        while addr < end {
            core::arch::asm!(
                "sfence.vma {addr}, {asid}",
                addr = in(reg) addr,
                asid = in(reg) asid.0 as usize,
                options(nostack)
            );
            addr = addr.saturating_add(PAGE_SIZE);
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = (virt, size, asid);
}

// 把物理地址换算成 PT-node 引导池的页索引：越界或未 4K 对齐则返回 None。
fn pool_index<State>(bag: &BootStaticBag<State>, phys: PhysAddr) -> Option<usize> {
    let range = bag.pt_node_pool_phys_range();
    let base = range.start.0;
    let end = range.end().0;
    if phys.0 < base || phys.0 >= end || !(phys.0 - base).is_multiple_of(PAGE_SIZE) {
        return None;
    }
    Some((phys.0 - base) / PAGE_SIZE)
}

// 返回引导池第 index 个 PT-node 页可写指针：RV64 用直接映射虚址，宿主用物理址。
fn pt_node_zero_ptr<State>(bag: &BootStaticBag<State>, index: usize) -> *mut u8 {
    #[cfg(target_arch = "riscv64")]
    {
        bag.pt_node_direct_va(index).0 as *mut u8
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        bag.pt_node_phys(index).0 as *mut u8
    }
}

// 将一整页（PAGE_SIZE 字节）清零。
unsafe fn zero_page(page: *mut u8) {
    core::ptr::write_bytes(page, 0, PAGE_SIZE);
}

// 测试用：重置 PT-node 池分配状态与 ASID 分配，使各测试互不干扰。
#[cfg(test)]
pub(crate) fn reset_pt_node_pool_for_test() {
    pt_node::reset_pt_node_pool_allocations_for_test();
    address_space::reset_asids_for_test();
}

#[cfg(test)]
mod tests;
