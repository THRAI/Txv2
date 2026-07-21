//! Sv39/QEMU 虚拟地址布局。
//!
//! 本模块是 pmap 常量与页大小策略的板级配置命名空间。
//!
//! 这里描述的核心数据结构/状态：
//! - 没有可变状态；本模块以常量形式定义 Sv39/QEMU 地址空间约定。
//! - 地址范围包括直接映射区、高位内核别名、用户顶部/辅助带、QEMU RAM 基址，
//!   以及固定的启动期页表节点池几何布局。
//! - 页大小常量定义 pmap 变更模块使用的 4 KiB / 2 MiB / 1 GiB 选项。
//!
//! 主要数据流函数：
//! - `validate_aligned_mapping()`、`validate_aligned_virt()` 和
//!   `validate_user_mapping_virt()` 在变更前拒绝违反 Sv39/QEMU 约定的请求。
//! - `bootstrap_satp_value()` 构造 H1 的 SATP 值。
//! - `direct_map_virt()` 为一个物理地址生成板级直接映射别名。
//! - `rv64_1g_leaf_index()`、`rv64_2m_leaf_index()` 和
//!   `rv64_4k_leaf_index()` 集中处理 Sv39 索引提取。
//!
//! 辅助函数刻意只做纯算术。非 pmap 的板级代码需要地址事实时从这里导入；
//! pmap 操作模块选择映射粒度时也从这里导入。参见
//! `docs/progress/decisions/2026-04-29-rv64-pmap-helper-extraction.md`。

use tx_hal::{PhysAddr, PmapError, PmapReserveKind};

// 架构与板级地址常量。高半区布局保留了直接映射区、高位内核别名，以及一小段
// 用户顶部辅助带；启动路径则临时保留一个低位恒等映射叶子。
#[cfg(test)]
pub(crate) const SV39_MODE: usize = 8; // Sv39 分页模式的 SATP MODE 编码值

pub(crate) const SV39_USER_TOP: usize = 0x0000_0040_0000_0000; // 用户地址空间上界(Sv39 低半区顶)
pub(crate) const USER_RESERVED_TOP_SIZE: usize = 4 * 1024 * 1024; // 用户顶部保留的辅助带大小(4 MiB)
pub(crate) const SV39_USER_ALLOC_TOP: usize = SV39_USER_TOP - USER_RESERVED_TOP_SIZE; // 用户可分配区上界(顶部保留带以下)
pub(crate) const DIRECT_MAP_BASE: usize = 0xffff_ffc0_0000_0000; // 直接映射区虚拟基址
pub(crate) const DIRECT_MAP_SIZE: usize = 128 * 1024 * 1024 * 1024; // 直接映射区大小(128 GiB)
pub(crate) const KERNEL_VIRT_BASE: usize = 0xffff_ffff_8020_0000; // 内核镜像高位虚拟基址

pub(crate) const QEMU_RAM_BASE: usize = 0x8000_0000; // QEMU virt 平台 RAM 物理基址
pub(crate) const QEMU_KERNEL_PHYS_BASE: usize = 0x8020_0000; // 内核加载的物理基址
pub(crate) const QEMU_BOOTSTRAP_MAP_SIZE: usize = 1024 * 1024 * 1024; // 启动期恒等映射窗口大小(1 GiB)
                                                                      // 启动期内核别名窗口：启动跳板在开启分页前映射 [kernel_phys_base .. +SIZE)，
                                                                      // 因此整个内核镜像(text + data + bss + 启动栈)都必须放得下。从 16M 提到 32M，
                                                                      // 因为 debug 内核镜像超过了 16M(main 的 8M `TX_OBSERVE_RINGS` 加上迁入的网络子
                                                                      // 系统把 `__kernel_end` 推到约 16.7M)；比此窗口更大的内核会在 satp 打开的瞬间
                                                                      // 触发缺页并在陷入向量里死循环。要与
                                                                      // `boot_trampoline.rs::TX_RV64_KERNEL_ALIAS_L0_TABLES` 保持同步(= SIZE / 2M)。
pub(crate) const KERNEL_BOOTSTRAP_ALIAS_SIZE: usize = 32 * 1024 * 1024; // 启动期内核别名窗口大小(32 MiB)
pub(crate) const SUPERPAGE_1G_SIZE: usize = 1024 * 1024 * 1024; // 1 GiB 大页大小
pub(crate) const SUPERPAGE_2M_SIZE: usize = 2 * 1024 * 1024; // 2 MiB 大页大小
pub(crate) const KERNEL_ALIAS_L0_TABLES: usize = KERNEL_BOOTSTRAP_ALIAS_SIZE / SUPERPAGE_2M_SIZE; // 内核别名窗口所需的 2 MiB 叶子数量
pub(crate) const PT_NODE_POOL_ENTRIES: usize = 8; // 固定启动期页表节点池的条目数
pub(crate) const PAGE_SIZE: usize = 4096; // 基础页大小(4 KiB)

// 校验辅助函数在改动页表前强制对齐和用户顶部策略。语义决定权仍归调用方；
// 这些检查只拒绝不可能或违反 Sv39 约定的请求。
// 校验虚拟地址与物理地址均按映射大小对齐
pub(crate) fn validate_aligned_mapping(
    virt: tx_hal::VirtAddr,
    phys: PhysAddr,
    size: usize,
) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) || !phys.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

// 校验单个虚拟地址是否按给定大小对齐
pub(crate) fn validate_aligned_virt(virt: tx_hal::VirtAddr, size: usize) -> Result<(), PmapError> {
    if !virt.0.is_multiple_of(size) {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

// 校验用户映射的虚拟范围未越过用户可分配区上界
pub(crate) fn validate_user_mapping_virt(
    virt: tx_hal::VirtAddr,
    kind: PmapReserveKind,
) -> Result<(), PmapError> {
    let end = virt
        .0
        .checked_add(kind.size())
        .ok_or(PmapError::InvalidRequest)?;
    if end > SV39_USER_ALLOC_TOP {
        return Err(PmapError::InvalidRequest);
    }
    Ok(())
}

// 索引与地址构造辅助函数集中处理 Sv39 位切分，使映射代码以页表层级而非移位来表达。
// 由根页表物理地址构造 SATP 值：MODE 置于高位，物理页号(root>>12)放低位
#[cfg(test)]
pub(crate) fn bootstrap_satp_value(root: PhysAddr) -> usize {
    (SV39_MODE << 60) | (root.0 >> 12) // 高位放 Sv39 MODE，低位放根页表的物理页号
}

// 把 value 向上对齐到 align 的整数倍；溢出返回 None
pub(crate) fn align_up(value: usize, align: usize) -> Option<usize> {
    let remainder = value % align;
    if remainder == 0 {
        Some(value)
    } else {
        value.checked_add(align - remainder)
    }
}

// 提取 1 GiB 大页(一级页表)的页表索引
pub(crate) fn rv64_1g_leaf_index(virt: usize) -> usize {
    (virt >> 30) & 0x1ff // 取 VPN[2]：右移 30 位后取低 9 位
}

// 提取 2 MiB 大页(二级页表)的页表索引
pub(crate) fn rv64_2m_leaf_index(virt: usize) -> usize {
    (virt >> 21) & 0x1ff // 取 VPN[1]：右移 21 位后取低 9 位
}

// 提取 4 KiB 页(三级页表)的页表索引
pub(crate) fn rv64_4k_leaf_index(virt: usize) -> usize {
    (virt >> 12) & 0x1ff // 取 VPN[0]：右移 12 位后取低 9 位
}

// 把物理地址转换为直接映射区中的虚拟地址(加上直接映射基址)
pub(crate) const fn direct_map_virt(phys: usize) -> usize {
    DIRECT_MAP_BASE + phys
}
