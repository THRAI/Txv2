// LA64 early boot assembly: BSP entry, DMW setup, and AP mailbox park loop.

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text.boot.phys, "ax"
    .equ TX_LA64_DMW_CACHED,   0x9000000000000011
    .equ TX_LA64_DMW_UNCACHED, 0x8000000000000001
    .equ TX_LA64_DMW_CACHED_BASE, 0x9000000000000000
    // KERNEL_LINK_BASE + KERNEL_BOOT_PHYS_SIZE (linker script): first
    // high-half instruction. Works from both entry modes: QEMU enters
    // in DA mode where the address truncates to phys 0x90001000; the
    // 2K1000 U-Boot enters through the cached DMW window where it is
    // used as-is.
    .equ TX_LA64_HIGH_START,   0x9000000090001000
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
    move    $s4, $zero

    // Use CPUID CSR for hart identity.
    csrrd   $s0, 0x20

    li.d    $t0, TX_LA64_DMW_CACHED
    csrwr   $t0, TX_LA64_CSR_DMW0
    li.d    $t0, TX_LA64_DMW_UNCACHED
    csrwr   $t0, TX_LA64_CSR_DMW1
    csrwr   $zero, TX_LA64_CSR_DMW2
    csrwr   $zero, TX_LA64_CSR_DMW3
    invtlb  0x0, $zero, $zero

    li.d    $t0, TX_LA64_HIGH_START
    jirl    $zero, $t0, 0

    // QEMU direct boot parks every AP in its built-in slave ROM. The ROM
    // reads mailbox 0 and jumps here, but does not provide a stack or a0.
    // Keep this trampoline in the physical boot segment so it is executable
    // while the AP is still in direct-address mode.
    .globl tx_la64_secondary_start
tx_la64_secondary_start:
    csrrd   $s0, 0x20
    li.d    $t0, TX_LA64_DMW_CACHED
    csrwr   $t0, TX_LA64_CSR_DMW0
    li.d    $t0, TX_LA64_DMW_UNCACHED
    csrwr   $t0, TX_LA64_CSR_DMW1
    csrwr   $zero, TX_LA64_CSR_DMW2
    csrwr   $zero, TX_LA64_CSR_DMW3
    invtlb  0x0, $zero, $zero
    li.d    $s4, 1
    li.d    $t0, TX_LA64_HIGH_START
    jirl    $zero, $t0, 0

    .section .text.boot.high, "ax"
tx_la64_high_start:
    bnez    $s4, tx_la64_secondary_high_start
    bnez    $s0, .Ltx_la64_secondary_wait
    la.local $sp, __tx_boot_stack_top
    // Keep the static stack geometry in sync with LA64_MAX_BOOT_CPUS and
    // LA64_BOOT_STACK_STRIDE (12 harts × 512 KiB).
    li.d    $t2, 12
    bgeu    $s0, $t2, .Ltx_la64_bsp_stack_ready
    slli.d  $t1, $s0, 19
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
    csrwr   $zero, TX_LA64_CSR_DMW2
    csrwr   $zero, TX_LA64_CSR_DMW3
    li.w    $t4, -1
    li.d    $t5, TX_LA64_IOCSR_IPI_EN
    iocsrwr.w $t4, $t5

.Ltx_la64_secondary_park:
    li.d    $t2, TX_LA64_IOCSR_MBUF4
    iocsrrd.d $t0, $t2
    beqz    $t0, .Ltx_la64_secondary_idle
    iocsrwr.d $zero, $t2
    li.d    $t2, TX_LA64_PHYS_ADDR_MASK
    and     $t0, $t0, $t2
    jirl    $zero, $t0, 0
.Ltx_la64_secondary_idle:
    // Keep polling the mailbox even if IPI delivery is masked/late.
    // This avoids AP bring-up stalling in `idle 0` before CRMD/ECFG
    // are fully configured for interrupt wakeups.
    nop
    b       .Ltx_la64_secondary_park

tx_la64_secondary_high_start:
    // CSR.CPUID is both the QEMU arch id and Txv2 logical CpuId on this
    // board. Reject a malformed id before indexing the static stack arena.
    csrrd   $a0, 0x20
    li.d    $t2, 12
    bgeu    $a0, $t2, .Ltx_la64_secondary_bad_id

    la.local $sp, __tx_boot_stack_top
    slli.d  $t1, $a0, 19
    sub.d   $sp, $sp, $t1

    // Mailbox 1 contains CoreInit::<P>::secondary_cpu_entry. Mailbox 0 was
    // consumed and cleared by QEMU's slave ROM before entering the physical
    // trampoline.
    li.d    $t2, TX_LA64_IOCSR_MBUF5
    iocsrrd.d $t0, $t2
    iocsrwr.d $zero, $t2
    beqz    $t0, .Ltx_la64_secondary_bad_id
    jirl    $zero, $t0, 0

.Ltx_la64_secondary_bad_id:
    idle    0
    b       .Ltx_la64_secondary_bad_id

"#
);
