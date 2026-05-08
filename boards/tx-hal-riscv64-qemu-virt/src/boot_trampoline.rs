// Auto-extracted from `boards/tx-hal-riscv64-qemu-virt/src/lib.rs` (2026-05-08
// jumbo split).
//
// Boot trampoline assembly: low-half identity bring-up, kernel-alias
// installation, satp activation, and high-half jump for the BSP and secondary
// harts. The QEMU-virt-specific symbol prefix (`__bootstrap_root_load` etc.)
// is provided by the linker script. Kept in its own module so the host build
// (which never sees this `global_asm!`) and the rv64 build share an obvious
// boundary.

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text.trampoline, "ax"
    .equ TX_RV64_KERNEL_VIRT_OFFSET, 0xffffffff00000000
    .equ TX_RV64_QEMU_RAM_BASE, 0x80000000
    .equ TX_RV64_DIRECT_MAP_ROOT_SLOT, 258
    .equ TX_RV64_IDENTITY_ROOT_SLOT, 2
    .equ TX_RV64_KERNEL_ROOT_SLOT, 510
    .equ TX_RV64_KERNEL_L1_START_SLOT, 1
    .equ TX_RV64_KERNEL_ALIAS_L0_TABLES, 8
    .equ TX_RV64_PAGE_SIZE, 4096
    .equ TX_RV64_MAX_BOOT_CPUS, 4
    .equ TX_RV64_SATP_SV39, 0x8000000000000000
    .equ TX_RV64_PTE_V, 0x001
    .equ TX_RV64_PTE_R, 0x002
    .equ TX_RV64_PTE_W, 0x004
    .equ TX_RV64_PTE_X, 0x008
    .equ TX_RV64_PTE_G, 0x020
    .equ TX_RV64_PTE_A, 0x040
    .equ TX_RV64_PTE_D, 0x080
    .equ TX_RV64_PTE_IDENTITY, TX_RV64_PTE_V | TX_RV64_PTE_R | TX_RV64_PTE_W | TX_RV64_PTE_X | TX_RV64_PTE_A | TX_RV64_PTE_D
    .equ TX_RV64_PTE_DIRECT, TX_RV64_PTE_V | TX_RV64_PTE_R | TX_RV64_PTE_W | TX_RV64_PTE_G | TX_RV64_PTE_A | TX_RV64_PTE_D
    .equ TX_RV64_PTE_KERNEL_BOOT, TX_RV64_PTE_V | TX_RV64_PTE_R | TX_RV64_PTE_W | TX_RV64_PTE_X | TX_RV64_PTE_G | TX_RV64_PTE_A | TX_RV64_PTE_D

    .globl _start
_start:
    mv s0, a0
    mv s1, a1
    la sp, __tx_boot_stack_top_load
    li t0, TX_RV64_MAX_BOOT_CPUS
    bgeu s0, t0, .Ltx_bsp_stack_ready
    slli t1, s0, 16
    sub sp, sp, t1
.Ltx_bsp_stack_ready:

    la t0, __bss_start_load
    la t1, __bss_end_load
1:
    bgeu t0, t1, 2f
    sd zero, 0(t0)
    addi t0, t0, 8
    j 1b

2:
    la s2, __bootstrap_root_load
    li t0, TX_RV64_QEMU_RAM_BASE
    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_RV64_PTE_IDENTITY
    li t2, TX_RV64_IDENTITY_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    srli t1, t0, 12
    slli t1, t1, 10
    ori t1, t1, TX_RV64_PTE_DIRECT
    li t2, TX_RV64_DIRECT_MAP_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    la s3, __kernel_alias_l1_load
    srli t1, s3, 12
    slli t1, t1, 10
    ori t1, t1, TX_RV64_PTE_V
    li t2, TX_RV64_KERNEL_ROOT_SLOT
    slli t2, t2, 3
    add t3, s2, t2
    sd t1, 0(t3)

    la s4, __kernel_alias_l0_tables_load
    li t0, 0
    li t1, TX_RV64_KERNEL_ALIAS_L0_TABLES
3:
    bgeu t0, t1, 4f
    slli t2, t0, 12
    add t3, s4, t2
    srli t4, t3, 12
    slli t4, t4, 10
    ori t4, t4, TX_RV64_PTE_V
    li t5, TX_RV64_KERNEL_L1_START_SLOT
    add t5, t5, t0
    slli t5, t5, 3
    add t6, s3, t5
    sd t4, 0(t6)
    addi t0, t0, 1
    j 3b

4:
    la s5, __kernel_start_load
    la s6, __kernel_end_load
    li s7, TX_RV64_PAGE_SIZE
    mv t0, s5
5:
    bgeu t0, s6, 6f
    sub t1, t0, s5
    srli t2, t1, 21
    slli t2, t2, 12
    add t3, s4, t2
    srli t4, t1, 12
    andi t4, t4, 0x1ff
    slli t4, t4, 3
    add t3, t3, t4
    srli t5, t0, 12
    slli t5, t5, 10
    ori t5, t5, TX_RV64_PTE_KERNEL_BOOT
    sd t5, 0(t3)
    add t0, t0, s7
    j 5b

6:
    srli t0, s2, 12
    li t1, TX_RV64_SATP_SV39
    or a0, t0, t1

    csrw satp, a0
    sfence.vma

    li t0, TX_RV64_KERNEL_VIRT_OFFSET
    la sp, __tx_boot_stack_top_load
    li t1, TX_RV64_MAX_BOOT_CPUS
    bgeu s0, t1, .Ltx_bsp_high_stack_ready
    slli t2, s0, 16
    sub sp, sp, t2
.Ltx_bsp_high_stack_ready:
    add sp, sp, t0
    .option push
    .option norelax
    la gp, __global_pointer_load
    add gp, gp, t0
    .option pop

    mv a0, s0
    mv a1, s1
    la t1, __rust_entry_load
    add t1, t1, t0
    jr t1

    .globl tx_rv64_qemu_secondary_start
    .type tx_rv64_qemu_secondary_start, @function
tx_rv64_qemu_secondary_start:
    mv s0, a0
    mv s1, a1
    li t0, TX_RV64_MAX_BOOT_CPUS
    bgeu s0, t0, 9f

    la sp, __tx_boot_stack_top_load
    slli t1, s0, 16
    sub sp, sp, t1
    li t0, TX_RV64_KERNEL_VIRT_OFFSET
    add sp, sp, t0
    .option push
    .option norelax
    la gp, __global_pointer_load
    add gp, gp, t0
    .option pop

    la t0, __bootstrap_root_load
    srli t0, t0, 12
    li t1, TX_RV64_SATP_SV39
    or t0, t0, t1
    csrw satp, t0
    sfence.vma

    mv a0, s0
    jr s1

9:
    wfi
    j 9b
    .size tx_rv64_qemu_secondary_start, . - tx_rv64_qemu_secondary_start

    .globl tx_rv64_qemu_install_kernel_stack
    .type tx_rv64_qemu_install_kernel_stack, @function
tx_rv64_qemu_install_kernel_stack:
    mv sp, a0
    ret
    .size tx_rv64_qemu_install_kernel_stack, . - tx_rv64_qemu_install_kernel_stack

7:
    wfi
    j 7b

"#
);
