// LA64 early boot assembly: BSP entry, DMW setup, and AP mailbox park loop.

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text.boot.phys, "ax"
    .equ TX_LA64_DMW_CACHED,   0x9000000000000011
    .equ TX_LA64_DMW_UNCACHED, 0x8000000000000001
    .equ TX_LA64_DMW_CACHED_BASE, 0x9000000000000000
    .equ TX_LA64_HIGH_START,   0x9000000000201000
    .equ TX_LA64_PHYS_ADDR_MASK, 0x0000ffffffffffff
    .equ TX_LA64_CSR_DMW0, 0x180
    .equ TX_LA64_CSR_DMW1, 0x181
    .equ TX_LA64_CSR_DMW2, 0x182
    .equ TX_LA64_CSR_DMW3, 0x183
    .equ TX_LA64_IOCSR_IPI_EN, 0x1004
    .globl _start
_start:
    // QEMU's LoongArch direct-boot ABI passes Linux-style boot
    // parameters in a0/a1/a2. Preserve them before using argument
    // registers for Txv2's Rust entry point.
    move    $s1, $a0
    move    $s2, $a1
    move    $s3, $a2

    // Use CPUID CSR for hart identity.
    csrrd   $s0, 0x20

    li.d    $t0, TX_LA64_DMW_CACHED
    csrwr   $t0, TX_LA64_CSR_DMW0
    li.d    $t0, TX_LA64_DMW_UNCACHED
    csrwr   $t0, TX_LA64_CSR_DMW1
    move    $t0, $zero
    csrwr   $t0, TX_LA64_CSR_DMW2
    csrwr   $t0, TX_LA64_CSR_DMW3
    invtlb  0x0, $zero, $zero

    li.d    $t0, TX_LA64_HIGH_START
    jirl    $zero, $t0, 0

    .section .text.boot.high, "ax"
tx_la64_high_start:
    bnez    $s0, .Ltx_la64_secondary_wait
    la.local $sp, __tx_boot_stack_top
    li.d    $t2, 4
    bgeu    $s0, $t2, .Ltx_la64_bsp_stack_ready
    slli.d  $t1, $s0, 16
    sub.d   $sp, $sp, $t1
.Ltx_la64_bsp_stack_ready:

    la.local $t0, _bss_start
    la.local $t1, _bss_end
1:
    bgeu    $t0, $t1, 2f
    st.d    $zero, $t0, 0
    addi.d  $t0, $t0, 8
    b       1b

2:
    li.w    $t4, -1
    li.d    $t5, TX_LA64_IOCSR_IPI_EN
    iocsrwr.w $t4, $t5

    move    $a0, $s0
    move    $a1, $s1
    move    $a2, $s2
    move    $a3, $s3
    la.local $t0, rust_entry
    jirl    $zero, $t0, 0

3:
    idle    0
    b       3b

    // AP early park loop: configured by BSP via IOCSR mailbox + IPI.
    .equ TX_LA64_IOCSR_MBUF4, 0x1020
    .equ TX_LA64_IOCSR_MBUF5, 0x1028
    .equ TX_LA64_IOCSR_MBUF6, 0x1030
.Ltx_la64_secondary_wait:
    // Configure DMW like BSP so AP can execute kernel virtual addresses.
    li.d    $t0, TX_LA64_DMW_CACHED
    csrwr   $t0, TX_LA64_CSR_DMW0
    li.d    $t0, TX_LA64_DMW_UNCACHED
    csrwr   $t0, TX_LA64_CSR_DMW1
    move    $t0, $zero
    csrwr   $t0, TX_LA64_CSR_DMW2
    csrwr   $t0, TX_LA64_CSR_DMW3
    li.w    $t4, -1
    li.d    $t5, TX_LA64_IOCSR_IPI_EN
    iocsrwr.w $t4, $t5

.Ltx_la64_secondary_park:
    li.d    $t2, TX_LA64_IOCSR_MBUF4
    li.d    $t3, TX_LA64_IOCSR_MBUF5
    li.d    $t4, TX_LA64_IOCSR_MBUF6
    iocsrrd.d $t0, $t2
    beqz    $t0, .Ltx_la64_secondary_idle
    iocsrrd.d $t1, $t3
    iocsrrd.d $a0, $t4
    iocsrwr.d $zero, $t2
    iocsrwr.d $zero, $t3
    iocsrwr.d $zero, $t4
    li.d    $t2, TX_LA64_PHYS_ADDR_MASK
    and     $sp, $t1, $t2
    and     $t0, $t0, $t2
    li.d    $t2, TX_LA64_DMW_CACHED_BASE
    or      $sp, $sp, $t2
    or      $t0, $t0, $t2
    jirl    $zero, $t0, 0
.Ltx_la64_secondary_idle:
    // Keep polling the mailbox even if IPI delivery is masked/late.
    // This avoids AP bring-up stalling in `idle 0` before CRMD/ECFG
    // are fully configured for interrupt wakeups.
    nop
    b       .Ltx_la64_secondary_park

"#
);
