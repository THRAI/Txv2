// Loongson 2K1000 U-Boot entry and cached-DMW transition for the BSP.

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text.boot.phys, "ax"
    .equ TX_LA64_DMW_CACHED,   0x9000000000000011
    .equ TX_LA64_DMW_UNCACHED, 0x8000000000000001
    .equ TX_LA64_HIGH_START,   0x9000000098001000
    .equ TX_LA64_CSR_DMW0, 0x180
    .equ TX_LA64_CSR_DMW1, 0x181
    .equ TX_LA64_CSR_DMW2, 0x182
    .equ TX_LA64_CSR_DMW3, 0x183
    .equ TX_LA64_CSR_CRMD, 0x0
    .equ TX_LA64_CSR_ECFG, 0x4
    .equ TX_LA64_CSR_TCFG, 0x41
    .equ TX_LA64_CSR_TICLR, 0x44

    .globl _start
_start:
    // Preserve the LoongArch U-Boot bootm arguments until Rust can capture
    // them. Phase 1 does not interpret the command line or EFI system table.
    move    $s0, $a0
    move    $s1, $a1
    move    $s2, $a2
    move    $s3, $a3
    csrrd   $s4, 0x20

    li.d    $t0, TX_LA64_DMW_CACHED
    csrwr   $t0, TX_LA64_CSR_DMW0
    li.d    $t0, TX_LA64_DMW_UNCACHED
    csrwr   $t0, TX_LA64_CSR_DMW1
    csrwr   $zero, TX_LA64_CSR_DMW2
    csrwr   $zero, TX_LA64_CSR_DMW3

    // Preserve PLV and memory-access attributes, but make the inherited
    // firmware state deterministic: interrupts off, direct mode off, paging
    // and DMW translation on.
    csrrd   $t0, TX_LA64_CSR_CRMD
    li.w    $t1, -29
    and     $t0, $t0, $t1
    ori     $t0, $t0, 0x10
    csrwr   $t0, TX_LA64_CSR_CRMD

    li.d    $t0, TX_LA64_HIGH_START
    jirl    $zero, $t0, 0

    .section .text.boot.high, "ax"
tx_la2k1000_high_start:
    // Invalidate only after instruction fetch is safely inside the cached DMW.
    // This remains valid whether U-Boot entered the trampoline through a low
    // TLB mapping or its cached high alias.
    invtlb  0x0, $zero, $zero
    ibar    0
    bnez    $s4, .Ltx_la2k1000_secondary_park
    la.local $sp, __tx_boot_stack_top

    la.local $t0, _bss_start
    la.local $t1, _bss_end
1:
    bgeu    $t0, $t1, 2f
    st.d    $zero, $t0, 0
    addi.d  $t0, $t0, 8
    b       1b

2:
    // U-Boot may leave controller and timer admission state behind. Stage 2
    // starts with every external source masked and admits only the local timer
    // after the full kernel vector is installed.
    csrwr   $zero, TX_LA64_CSR_TCFG
    li.w    $t0, 1
    csrwr   $t0, TX_LA64_CSR_TICLR
    csrwr   $zero, TX_LA64_CSR_ECFG

    move    $a0, $s4
    move    $a1, $s0
    move    $a2, $s1
    move    $a3, $s2
    move    $a4, $s3
    la.local $t0, rust_entry
    jirl    $zero, $t0, 0

3:
    b       3b

.Ltx_la2k1000_secondary_park:
    b       .Ltx_la2k1000_secondary_park
"#
);
