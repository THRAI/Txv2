//! Sv39 PTE(页表项)编码与解码辅助函数。
//!
//! 这里解读的核心数据结构/状态：
//! - PTE 标志常量 `PTE_*`：本后端使用的 Sv39 有效位、权限位、全局位、访问位
//!   和脏位。
//! - `PmapPermissions`：HAL 的权限词汇，会被翻译成 Sv39 叶子项。
//! - `PageTable`：在目标机上经由直接映射、在测试中经由宿主指针，从物理地址解码
//!   得到。
//!
//! 主要数据流函数：
//! - `validate_rv64_leaf_permissions()` 拒绝本后端无法安全编码的权限组合。
//! - `kernel_alias_permissions_for_phys()` 依据 `BootStaticBag` 里记录的链接器
//!   范围推导内核 text/rodata/data 的最终权限。
//! - `encode_leaf_pte_with_permissions()`、`encode_leaf_pte()` 和
//!   `encode_branch_pte()` 把物理地址转换成 PTE 字。
//! - `pte_is_branch()`、`pte_is_leaf()` 和 `pte_phys()` 从原始条目还原页表形态。
//! - `page_table_mut_from_phys()` 是唯一构造可变 `PageTable` 引用的 PTE 辅助函数。
//!
//! 这里的辅助函数不掌握策略：由更上层模块决定某个映射属于内核启动、直接映射、
//! MMIO 路径还是进程根页表。参见
//! `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`。

use tx_hal::{PhysAddr, PhysRange, PmapError, PmapPermissions};

use crate::boot_static::{BootStaticBag, PageTable};

#[cfg(target_arch = "riscv64")]
use super::topology::direct_map_virt;

pub(crate) const PTE_V: u64 = 1 << 0; // 有效位(Valid)
pub(crate) const PTE_R: u64 = 1 << 1; // 可读(Read)
pub(crate) const PTE_W: u64 = 1 << 2; // 可写(Write)
pub(crate) const PTE_X: u64 = 1 << 3; // 可执行(eXecute)
pub(crate) const PTE_U: u64 = 1 << 4; // 用户可访问(User)
pub(crate) const PTE_G: u64 = 1 << 5; // 全局映射(Global)
pub(crate) const PTE_A: u64 = 1 << 6; // 已访问(Accessed)
pub(crate) const PTE_D: u64 = 1 << 7; // 脏(Dirty)

// 权限校验与内核别名分类放在一起，因为二者都界定了哪些 HAL 权限词汇对本 Sv39
// 后端合法。内核别名依据启动静态链接器信息拆分 text/rodata/data，而进程映射还可
// 能额外置上用户位。
// 校验一个叶子 PTE 的权限组合是否为本后端可安全编码(拒绝越权用户位/仅写不读等)
pub(crate) fn validate_rv64_leaf_permissions(
    permissions: PmapPermissions,
    allow_user: bool,
) -> Result<(), PmapError> {
    let readable = permissions.contains(PmapPermissions::READ);
    let writable = permissions.contains(PmapPermissions::WRITE);
    let executable = permissions.contains(PmapPermissions::EXECUTE);
    let user = permissions.contains(PmapPermissions::USER);
    // 非法组合：不允许用户位却带用户位、既不可读也不可执行、可写却不可读
    if (user && !allow_user) || (!readable && !executable) || (writable && !readable) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

// 以内核可读写权限编码一个内核映射叶子 PTE
pub(crate) fn encode_kernel_mapping_leaf(phys: PhysAddr) -> u64 {
    encode_leaf_pte_with_permissions(phys, PmapPermissions::KERNEL_RW)
}

// 依据物理地址落在哪个内核段(text/rodata/data 等)推导对应的内核别名权限
pub(crate) fn kernel_alias_permissions_for_phys<State>(
    bag: &BootStaticBag<State>,
    phys: usize,
) -> PmapPermissions {
    let phys = PhysAddr(phys);
    if range_contains(bag.kernel_text_phys(), phys) {
        PmapPermissions::KERNEL_RX // 代码段：内核可读可执行
    } else if range_contains(bag.kernel_rodata_phys(), phys) {
        PmapPermissions::KERNEL_RO // 只读数据段：内核只读
    } else {
        debug_assert!(
            range_contains(bag.kernel_data_phys(), phys)
                || range_contains(bag.kernel_bss_phys(), phys)
                || range_contains(bag.kernel_stack_phys(), phys)
                || range_contains(bag.kernel_image_phys(), phys)
        );
        PmapPermissions::KERNEL_RW // 其余(data/bss/栈等)：内核可读可写
    }
}

// 判断物理地址是否落在给定物理范围 [start, end) 内
fn range_contains(range: PhysRange, phys: PhysAddr) -> bool {
    phys.0 >= range.start.0 && phys.0 < range.end().0
}

// 编码辅助函数由带类型的 HAL 地址和权限值生成 Sv39 叶子项与分支项。调用方选择
// 映射角色；这些函数只按请求的叶子形态设置 V/A/D 以及 R/W/X/U/G。
// 由 HAL 权限值构造叶子 PTE 的标志位并编码成叶子项
pub(crate) fn encode_leaf_pte_with_permissions(
    phys: PhysAddr,
    permissions: PmapPermissions,
) -> u64 {
    let mut flags = 0;
    if permissions.contains(PmapPermissions::READ) {
        flags |= PTE_R; // 可读 → 置 R 位
    }
    if permissions.contains(PmapPermissions::WRITE) {
        flags |= PTE_W; // 可写 → 置 W 位
    }
    if permissions.contains(PmapPermissions::EXECUTE) {
        flags |= PTE_X; // 可执行 → 置 X 位
    }
    if permissions.contains(PmapPermissions::USER) {
        flags |= PTE_U; // 用户可访问 → 置 U 位
    }
    if permissions.contains(PmapPermissions::GLOBAL) {
        flags |= PTE_G; // 全局映射 → 置 G 位
    }
    encode_leaf_pte(phys, flags)
}

// 由物理地址和标志位编码叶子 PTE：物理页号左移到 PPN 字段，并补上 V/A/D
pub(crate) fn encode_leaf_pte(phys: PhysAddr, flags: u64) -> u64 {
    ((phys.0 as u64 >> 12) << 10) | flags | PTE_V | PTE_A | PTE_D // PPN 放 bit10 起，叶子恒置 V/A/D
}

// 编码分支 PTE(指向下级页表)：只放 PPN 并置 V，R/W/X 全为 0 表示这是分支
pub(crate) fn encode_branch_pte(phys: PhysAddr) -> u64 {
    ((phys.0 as u64 >> 12) << 10) | PTE_V // 只置 V、无 R/W/X → 分支项
}

// PTE 查询辅助函数把分支/叶子判定与物理地址还原集中在一处。其余 pmap 代码把非叶
// 子的分支项视为页表的所有权边、把叶子项视为映射状态。
// 判断是否为分支项(有效且 R/W/X 全为 0)
pub(crate) fn pte_is_branch(pte: u64) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) == 0
}

// 判断是否为叶子项(有效且 R/W/X 至少有一位置位)
pub(crate) fn pte_is_leaf(pte: u64) -> bool {
    pte & PTE_V != 0 && pte & (PTE_R | PTE_W | PTE_X) != 0
}

// 从 PTE 还原它指向的物理地址(取出 PPN 字段再左移回物理页对齐)
pub(crate) fn pte_phys(pte: u64) -> PhysAddr {
    PhysAddr(((pte >> 10) << 12) as usize) // 去掉低 10 位标志、还原 PPN 到物理地址
}

// 页表页在真实 RV64 上经由直接映射寻址，在单元测试中经由宿主指针寻址。把这个条件
// 编译留在这里，避免上层 pmap 代码各自硬编码物理地址到指针的转换。
// 由页表页物理地址得到其可变 `PageTable` 引用(unsafe：调用方需保证唯一性与有效性)
pub(crate) unsafe fn page_table_mut_from_phys(phys: PhysAddr) -> &'static mut PageTable {
    #[cfg(target_arch = "riscv64")]
    {
        unsafe { &mut *(direct_map_virt(phys.0) as *mut PageTable) }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        unsafe { &mut *(phys.0 as *mut PageTable) }
    }
}
