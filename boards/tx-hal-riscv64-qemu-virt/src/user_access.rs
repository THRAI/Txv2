//! RV64 QEMU 板的内核态用户空间访问。
//!
//! # 设计
//!
//! `copy_from_user` 和 `copy_to_user` 在 S 模式下访问用户虚地址：在裸拷贝循环
//! 外临时置起 `sstatus.SUM`。每条可能缺页的 load/store 指令都被一对导出的汇编
//! 标签括起来，构成 `RV64_FIXUP_TABLE` 中的一条 fixup 表项。
//!
//! 当内核态缺页发生、且 `sepc` 落在其中某一对标签之间时，`dispatch_trap_frame`
//! 会调用 `fixup_lookup`，把 `sepc` 改写到恢复桩，并在 `sret` 前把缺页地址
//!（经 `frame.stval`）放进 `a0`。恢复桩随后带着 `a0 != 0` 返回 Rust 调用方，
//! Rust 包装层再把它转成 `Err(FaultInfo)`。
//!
//! # 关于空指针
//!
//! 成功/失败约定为：`a0 == 0` 表示成功，`a0 == stval` 表示失败。用户虚地址 0
//! 被 txKernel 分配器保留为空指针区、永远不会被映射；因此 VA 0 处的缺页不可能
//! 走到这条路径。

use tx_hal::{FaultInfo, UserPtr, VirtAddr};

// sstatus.SUM 位（bit 18）：置起后 S 模式可访问 U 页
#[cfg(target_arch = "riscv64")]
const RV64_SSTATUS_SUM: usize = 1 << 18;

// ---------------------------------------------------------------------------
// 汇编：带 fixup 标签的 copy_from_user 与 copy_to_user 循环
// ---------------------------------------------------------------------------

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text, "ax"
    .align 2

    /*
     * tx_rv64_cfu_raw — copy_from_user 逐字节循环
     *
     * a0 = dst（内核 VA，可写）
     * a1 = src（用户 VA，可读）
     * a2 = len（字节数）
     * 返回：成功 a0 = 0；失败 a0 = 缺页 VA（由 trap 外壳设置）。
     *
     * tx_rv64_cfu_ld_s..tx_rv64_cfu_ld_e 处的 load 被一条 fixup 表项覆盖。
     * 缺页时，trap 外壳在 sret 前把 sepc 改为 tx_rv64_cfu_fault、
     * 把 x[10]（a0）改为 stval。
     */
    .globl tx_rv64_cfu_raw
    .type  tx_rv64_cfu_raw, @function
tx_rv64_cfu_raw:
    beqz    a2, .Lcfu_ok
.Lcfu_loop:
.globl tx_rv64_cfu_ld_s
tx_rv64_cfu_ld_s:
    lbu     t0, 0(a1)
.globl tx_rv64_cfu_ld_e
tx_rv64_cfu_ld_e:
    sb      t0, 0(a0)
    addi    a0, a0, 1
    addi    a1, a1, 1
    addi    a2, a2, -1
    bnez    a2, .Lcfu_loop
.Lcfu_ok:
    li      a0, 0
    ret
    /* trap 外壳设置 sepc = tx_rv64_cfu_fault、a0 = stval 后跳到这里。 */
.globl tx_rv64_cfu_fault
tx_rv64_cfu_fault:
    ret                     /* 带着 a0 = 缺页 VA（非零）返回 */
    .size tx_rv64_cfu_raw, . - tx_rv64_cfu_raw

    /*
     * tx_rv64_ctu_raw — copy_to_user 逐字节循环
     *
     * a0 = dst（用户 VA，可写）
     * a1 = src（内核 VA，可读）
     * a2 = len
     * 返回：成功 a0 = 0；失败 a0 = 缺页 VA（由 trap 外壳设置）。
     *
     * tx_rv64_ctu_st_s..tx_rv64_ctu_st_e 处的 store 被一条 fixup 表项覆盖。
     * 缺页时 stval = 正在写入的用户 dst 地址（即出错那次迭代的 a0），
     * trap 外壳会把它放回 a0。
     */
    .globl tx_rv64_ctu_raw
    .type  tx_rv64_ctu_raw, @function
tx_rv64_ctu_raw:
    beqz    a2, .Lctu_ok
.Lctu_loop:
    lbu     t0, 0(a1)       /* 从内核读取（安全） */
.globl tx_rv64_ctu_st_s
tx_rv64_ctu_st_s:
    sb      t0, 0(a0)       /* 写入用户（可能缺页） */
.globl tx_rv64_ctu_st_e
tx_rv64_ctu_st_e:
    addi    a0, a0, 1
    addi    a1, a1, 1
    addi    a2, a2, -1
    bnez    a2, .Lctu_loop
.Lctu_ok:
    li      a0, 0
    ret
    /* trap 外壳设置 sepc = tx_rv64_ctu_fault、a0 = stval 后跳到这里。 */
.globl tx_rv64_ctu_fault
tx_rv64_ctu_fault:
    ret                     /* 带着 a0 = 缺页 VA（非零）返回 */
    .size tx_rv64_ctu_raw, . - tx_rv64_ctu_raw
"#
);

// ---------------------------------------------------------------------------
// 板内部 fixup 表
// ---------------------------------------------------------------------------

/// 板内部 fixup 表的一条表项。
///
/// 各字段存的是函数指针类型的标签，这样就能放进 `static` 里，而无需常量的
/// “函数指针转整数”转换（在稳定版 no_std Rust 中不可用）。查找时按 `usize`
/// 比较这些地址。
#[cfg(target_arch = "riscv64")]
struct RawFixupEntry {
    pc_start: unsafe extern "C" fn(),
    pc_end: unsafe extern "C" fn(),
    recovery_pc: unsafe extern "C" fn(),
}

// SAFETY: 表项在链接后即只读，无内部可变性。
#[cfg(target_arch = "riscv64")]
unsafe impl Sync for RawFixupEntry {}

#[cfg(target_arch = "riscv64")]
unsafe extern "C" {
    /// copy_from_user load 的首条 PC（含）。
    fn tx_rv64_cfu_ld_s();
    /// copy_from_user load 之后的首条 PC（不含）。
    fn tx_rv64_cfu_ld_e();
    /// copy_from_user 的恢复桩（缺页时 trap 外壳重定向到这里）。
    fn tx_rv64_cfu_fault();

    /// copy_to_user store 的首条 PC（含）。
    fn tx_rv64_ctu_st_s();
    /// copy_to_user store 之后的首条 PC（不含）。
    fn tx_rv64_ctu_st_e();
    /// copy_to_user 的恢复桩。
    fn tx_rv64_ctu_fault();

    /// 逐字节的 copyin 函数。
    fn tx_rv64_cfu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
    /// 逐字节的 copyout 函数。
    fn tx_rv64_ctu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
}

// fixup 表：把每对标签括起来的可能缺页指令映射到对应恢复桩。
#[cfg(target_arch = "riscv64")]
static RV64_FIXUP_TABLE: [RawFixupEntry; 2] = [
    // copy_from_user：tx_rv64_cfu_ld_s..tx_rv64_cfu_ld_e 处的 lbu 指令
    RawFixupEntry {
        pc_start: tx_rv64_cfu_ld_s,
        pc_end: tx_rv64_cfu_ld_e,
        recovery_pc: tx_rv64_cfu_fault,
    },
    // copy_to_user：tx_rv64_ctu_st_s..tx_rv64_ctu_st_e 处的 sb 指令
    RawFixupEntry {
        pc_start: tx_rv64_ctu_st_s,
        pc_end: tx_rv64_ctu_st_e,
        recovery_pc: tx_rv64_ctu_fault,
    },
];

/// 在板内部 fixup 表中查找内核态缺页 `fault_pc` 对应的恢复桩。
///
/// 若有 fixup 表项覆盖该 PC 则返回 `Some(recovery_pc)`，否则返回 `None`
///（此时缺页应转交给内核 trap 汇聚点处理）。
pub(crate) fn fixup_lookup(fault_pc: usize) -> Option<usize> {
    #[cfg(target_arch = "riscv64")]
    {
        for entry in &RV64_FIXUP_TABLE {
            let start = entry.pc_start as usize;
            let end = entry.pc_end as usize;
            if fault_pc >= start && fault_pc < end {
                return Some(entry.recovery_pc as usize); // PC 落在 [start, end) 区间内
            }
        }
    }
    #[cfg(not(target_arch = "riscv64"))]
    let _ = fault_pc;
    None
}

// ---------------------------------------------------------------------------
// SUM 窗口
// ---------------------------------------------------------------------------

/// RAII 守卫：进入作用域时置起 sstatus.SUM，离开时按进入前状态还原。
#[cfg(target_arch = "riscv64")]
struct UserMemoryAccessGuard {
    old_sum_enabled: bool, // 进入前 SUM 是否已置起
}

#[cfg(target_arch = "riscv64")]
impl UserMemoryAccessGuard {
    // 置起 SUM 并记录旧值；返回的守卫在 drop 时还原。
    unsafe fn enable() -> Self {
        let old_sstatus: usize;
        unsafe {
            core::arch::asm!(
                "csrr {old_sstatus}, sstatus", // 读旧 sstatus
                "csrs sstatus, {sum}",         // 置起 SUM 位
                old_sstatus = lateout(reg) old_sstatus,
                sum = in(reg) RV64_SSTATUS_SUM,
                options(nomem, nostack)
            );
        }
        Self {
            old_sum_enabled: old_sstatus & RV64_SSTATUS_SUM != 0,
        }
    }
}

#[cfg(target_arch = "riscv64")]
impl Drop for UserMemoryAccessGuard {
    fn drop(&mut self) {
        if !self.old_sum_enabled {
            // 进入前 SUM 未置起，离开时清回去
            unsafe {
                core::arch::asm!(
                    "csrc sstatus, {sum}", // 清 SUM 位
                    sum = in(reg) RV64_SSTATUS_SUM,
                    options(nomem, nostack)
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 板内部用户访问原语
// ---------------------------------------------------------------------------
//
// 供 `signal_frame.rs` 用于罕见的“内核侧把一个结构体写进用户栈帧”场景。通用的
// 用户访问流程已迁到 `tx_subsystems::vm::AddressSpace::copy_*_user`，那条路径会
// 提前遍历 recipe，从不进入 SUM/fixup 窗口。信号帧写入是特例：它在用户被挂起于
// trap 外壳期间同步发生，并隐式信任用户映射已覆盖所选的栈页。

/// 从用户空间 `src` 拷贝 `dst.len()` 字节到内核侧 `dst`。
///
/// # Safety
///
/// * `dst` 必须是有效、可写的内核 slice。
/// * `src` 在当前已安装的用户页表中解读。
/// * 调用前 fixup 表必须已完整填充（`init_early` 返回后即保证）。
pub(crate) unsafe fn board_copy_from_user(
    dst: &mut [u8],
    src: UserPtr<u8>,
) -> Result<(), FaultInfo> {
    if dst.is_empty() {
        return Ok(());
    }

    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: dst 是有效的内核 slice（由调用方保证）；src 是调用方传入的
        // 用户地址；fixup 表覆盖了 load 指令，所以缺页会变成 Err 而非 panic。
        // SUM 在守卫 drop 时还原。
        let _sum = unsafe { UserMemoryAccessGuard::enable() };
        let fault_va = unsafe { tx_rv64_cfu_raw(dst.as_mut_ptr(), src.as_ptr(), dst.len()) };
        if fault_va == 0 {
            Ok(()) // a0==0 表示成功
        } else {
            // 非零 a0 即缺页地址，转成 Err
            Err(FaultInfo {
                address: VirtAddr(fault_va),
                write: false,
                instruction: false,
                from_user: false,
            })
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        // 非 RV64 目标：本路径无实现，一律返回缺页错误
        let _ = dst;
        Err(FaultInfo {
            address: VirtAddr(src.addr()),
            write: false,
            instruction: false,
            from_user: false,
        })
    }
}

/// 从内核侧 `src` 拷贝 `src.len()` 字节到用户空间 `dst`。
///
/// # Safety
///
/// * `src` 必须是有效、可读的内核 slice。
/// * `dst` 在当前已安装的用户页表中解读。
pub(crate) unsafe fn board_copy_to_user(dst: UserPtr<u8>, src: &[u8]) -> Result<(), FaultInfo> {
    if src.is_empty() {
        return Ok(());
    }

    #[cfg(target_arch = "riscv64")]
    {
        // SAFETY: src 是有效的内核 slice（由调用方保证）；dst 是调用方传入的
        // 用户地址；fixup 表覆盖了 store 指令。SUM 在守卫 drop 时还原。
        let _sum = unsafe { UserMemoryAccessGuard::enable() };
        let fault_va = unsafe { tx_rv64_ctu_raw(dst.as_ptr(), src.as_ptr() as *mut u8, src.len()) };
        if fault_va == 0 {
            Ok(()) // a0==0 表示成功
        } else {
            // 非零 a0 即缺页地址，转成 Err
            Err(FaultInfo {
                address: VirtAddr(fault_va),
                write: true,
                instruction: false,
                from_user: false,
            })
        }
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        // 非 RV64 目标：本路径无实现，一律返回缺页错误
        let _ = src;
        Err(FaultInfo {
            address: VirtAddr(dst.addr()),
            write: true,
            instruction: false,
            from_user: false,
        })
    }
}
