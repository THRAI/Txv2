// LA64 trap, userspace-entry, FP context, and raw user-copy assembly.

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text.trap, "ax"
    .align 12
    .equ TX_LA64_TF_R0, 0
    .equ TX_LA64_TF_R1, 8
    .equ TX_LA64_TF_R2, 16
    .equ TX_LA64_TF_R3, 24
    .equ TX_LA64_TF_R4, 32
    .equ TX_LA64_TF_R5, 40
    .equ TX_LA64_TF_R6, 48
    .equ TX_LA64_TF_R7, 56
    .equ TX_LA64_TF_R8, 64
    .equ TX_LA64_TF_R9, 72
    .equ TX_LA64_TF_R10, 80
    .equ TX_LA64_TF_R11, 88
    .equ TX_LA64_TF_R12, 96
    .equ TX_LA64_TF_R13, 104
    .equ TX_LA64_TF_R14, 112
    .equ TX_LA64_TF_R15, 120
    .equ TX_LA64_TF_R16, 128
    .equ TX_LA64_TF_R17, 136
    .equ TX_LA64_TF_R18, 144
    .equ TX_LA64_TF_R19, 152
    .equ TX_LA64_TF_R20, 160
    .equ TX_LA64_TF_R21, 168
    .equ TX_LA64_TF_R22, 176
    .equ TX_LA64_TF_R23, 184
    .equ TX_LA64_TF_R24, 192
    .equ TX_LA64_TF_R25, 200
    .equ TX_LA64_TF_R26, 208
    .equ TX_LA64_TF_R27, 216
    .equ TX_LA64_TF_R28, 224
    .equ TX_LA64_TF_R29, 232
    .equ TX_LA64_TF_R30, 240
    .equ TX_LA64_TF_R31, 248
    .equ TX_LA64_TF_ESTAT, 256
    .equ TX_LA64_TF_ERA, 264
    .equ TX_LA64_TF_BADV, 272
    .equ TX_LA64_TF_CRMD, 280
    .equ TX_LA64_TF_PRMD, 288
    .equ TX_LA64_TF_SIZE, 304
    .equ TX_LA64_CSR_CRMD_TRAP, 0x00
    .equ TX_LA64_CSR_PRMD_TRAP, 0x01
    .equ TX_LA64_CSR_ESTAT_TRAP, 0x05
    .equ TX_LA64_CSR_ERA_TRAP, 0x06
    .equ TX_LA64_CSR_BADV_TRAP, 0x07
    .equ TX_LA64_CSR_KSAVE0_TRAP, 0x30
    .equ TX_LA64_CSR_KSAVE1_TRAP, 0x31
    .equ TX_LA64_CSR_KSAVE2_TRAP, 0x32
    .equ TX_LA64_CSR_KSAVE3_TRAP, 0x33
    .equ TX_LA64_CSR_PGD_TRAP, 0x1b
    .equ TX_LA64_CSR_EENTRY_TRAP, 0x0c
    .equ TX_LA64_CSR_TLBRERA_TRAP, 0x8a
    .equ TX_LA64_CSR_TLBRSAVE_TRAP, 0x8b
    .equ TX_LA64_CSR_TLBRELO0_TRAP, 0x8c
    .equ TX_LA64_CSR_TLBRELO1_TRAP, 0x8d
    .equ TX_LA64_CSR_MERRENTRY_TRAP, 0x93
    .equ TX_LA64_CSR_LLBCTL_TRAP, 0x60
    .equ TX_LA64_LLBCTL_KLO_TRAP, 0x4
    .equ TX_LA64_DMW_CACHED_BASE_TRAP, 0x9000000000000000
    .equ TX_LA64_UART0_UNCACHED_TRAP, 0x800000001fe00000
    .equ TX_LA64_UART_THR_TRAP, 0
    .equ TX_LA64_UART_LSR_TRAP, 5
    .equ TX_LA64_UART_LSR_THRE_TRAP, 0x20
    .equ TX_LA64_PHYS_ADDR_MASK_TRAP, 0x0000ffffffffffff
    .equ TX_LA64_RCTX_SP, 0
    .equ TX_LA64_RCTX_RA, 8
    .equ TX_LA64_RCTX_R21, 16
    .equ TX_LA64_RCTX_TP, 24
    .equ TX_LA64_RCTX_R22, 32
    .equ TX_LA64_PRMD_PPLV_USER, 3

    .macro TX_LA64_TRACE_CHAR ch, tmp0, tmp1
        li.d    \tmp0, TX_LA64_UART0_UNCACHED_TRAP
    987:
        ld.bu   \tmp1, \tmp0, TX_LA64_UART_LSR_TRAP
        andi    \tmp1, \tmp1, TX_LA64_UART_LSR_THRE_TRAP
        beqz    \tmp1, 987b
        li.w    \tmp1, \ch
        st.b    \tmp1, \tmp0, TX_LA64_UART_THR_TRAP
    .endm

    .section .text.trap, "ax"

    .globl tx_la64_qemu_trap_low_start
    .type tx_la64_qemu_trap_low_start, @function
tx_la64_qemu_trap_low_start:

    .globl tx_la64_qemu_exception_vector
    .type tx_la64_qemu_exception_vector, @function
tx_la64_qemu_exception_vector:
    csrwr   $r12, TX_LA64_CSR_KSAVE3_TRAP
    csrrd   $r12, TX_LA64_CSR_PRMD_TRAP
    andi    $r12, $r12, TX_LA64_PRMD_PPLV_USER
    addi.d  $r12, $r12, -TX_LA64_PRMD_PPLV_USER
    bnez    $r12, .Ltx_la64_kernel_trap_stack_ready

    // User trap: switch onto the per-hart trap stack. KSAVE0 is primed
    // with trap_stack_top before entering user mode and on every clean
    // user-trap exit. After csrwr: sp = trap_stack_top, KSAVE0 = user sp.
    csrwr   $sp, TX_LA64_CSR_KSAVE0_TRAP
.Ltx_la64_kernel_trap_stack_ready:
    addi.d  $sp, $sp, -TX_LA64_TF_SIZE
    csrrd   $r12, TX_LA64_CSR_KSAVE3_TRAP
    st.d    $r12, $sp, TX_LA64_TF_R12
    st.d    $zero, $sp, TX_LA64_TF_R0
    st.d    $r1, $sp, TX_LA64_TF_R1
    st.d    $r2, $sp, TX_LA64_TF_R2
    csrrd   $r12, TX_LA64_CSR_PRMD_TRAP
    andi    $r12, $r12, TX_LA64_PRMD_PPLV_USER
    addi.d  $r12, $r12, -TX_LA64_PRMD_PPLV_USER
    bnez    $r12, .Ltx_la64_save_kernel_sp
    csrrd   $r12, TX_LA64_CSR_KSAVE0_TRAP
    b       .Ltx_la64_save_sp_done
.Ltx_la64_save_kernel_sp:
    addi.d  $r12, $sp, TX_LA64_TF_SIZE
.Ltx_la64_save_sp_done:
    st.d    $r12, $sp, TX_LA64_TF_R3
    st.d    $r4, $sp, TX_LA64_TF_R4
    st.d    $r5, $sp, TX_LA64_TF_R5
    st.d    $r6, $sp, TX_LA64_TF_R6
    st.d    $r7, $sp, TX_LA64_TF_R7
    st.d    $r8, $sp, TX_LA64_TF_R8
    st.d    $r9, $sp, TX_LA64_TF_R9
    st.d    $r10, $sp, TX_LA64_TF_R10
    st.d    $r11, $sp, TX_LA64_TF_R11
    st.d    $r13, $sp, TX_LA64_TF_R13
    st.d    $r14, $sp, TX_LA64_TF_R14
    st.d    $r15, $sp, TX_LA64_TF_R15
    st.d    $r16, $sp, TX_LA64_TF_R16
    st.d    $r17, $sp, TX_LA64_TF_R17
    st.d    $r18, $sp, TX_LA64_TF_R18
    st.d    $r19, $sp, TX_LA64_TF_R19
    st.d    $r20, $sp, TX_LA64_TF_R20
    st.d    $r21, $sp, TX_LA64_TF_R21
    st.d    $r22, $sp, TX_LA64_TF_R22
    st.d    $r23, $sp, TX_LA64_TF_R23
    st.d    $r24, $sp, TX_LA64_TF_R24
    st.d    $r25, $sp, TX_LA64_TF_R25
    st.d    $r26, $sp, TX_LA64_TF_R26
    st.d    $r27, $sp, TX_LA64_TF_R27
    st.d    $r28, $sp, TX_LA64_TF_R28
    st.d    $r29, $sp, TX_LA64_TF_R29
    st.d    $r30, $sp, TX_LA64_TF_R30
    st.d    $r31, $sp, TX_LA64_TF_R31
    csrrd   $r12, TX_LA64_CSR_ESTAT_TRAP
    st.d    $r12, $sp, TX_LA64_TF_ESTAT
    csrrd   $r12, TX_LA64_CSR_ERA_TRAP
    st.d    $r12, $sp, TX_LA64_TF_ERA
    csrrd   $r12, TX_LA64_CSR_BADV_TRAP
    st.d    $r12, $sp, TX_LA64_TF_BADV
    csrrd   $r12, TX_LA64_CSR_CRMD_TRAP
    st.d    $r12, $sp, TX_LA64_TF_CRMD
    csrrd   $r12, TX_LA64_CSR_PRMD_TRAP
    st.d    $r12, $sp, TX_LA64_TF_PRMD

    // User traps arrive with user GPRs, including r21 and the user thread
    // pointer in r2. Recover the kernel TLS registers before entering Rust;
    // the user values remain saved in the trap frame and are restored below.
    ld.d    $r12, $sp, TX_LA64_TF_PRMD
    andi    $r12, $r12, TX_LA64_PRMD_PPLV_USER
    li.w    $r13, TX_LA64_PRMD_PPLV_USER
    bne     $r12, $r13, .Ltx_la64_kernel_tls_ready
    csrrd   $r21, TX_LA64_CSR_KSAVE1_TRAP
    csrrd   $r2, TX_LA64_CSR_KSAVE2_TRAP
.Ltx_la64_kernel_tls_ready:

    move    $a0, $sp
    la.local $r12, tx_la64_qemu_kernel_trap_entry
    li.d    $r13, TX_LA64_PHYS_ADDR_MASK_TRAP
    and     $r12, $r12, $r13
    li.d    $r13, TX_LA64_DMW_CACHED_BASE_TRAP
    or      $r12, $r12, $r13
    jirl    $r1, $r12, 0

    move    $r31, $sp
    ld.d    $r12, $r31, TX_LA64_TF_ERA
    csrwr   $r12, TX_LA64_CSR_ERA_TRAP
    ld.d    $r12, $r31, TX_LA64_TF_PRMD
    csrwr   $r12, TX_LA64_CSR_PRMD_TRAP

    ld.d    $r12, $r31, TX_LA64_TF_PRMD
    andi    $r12, $r12, TX_LA64_PRMD_PPLV_USER
    li.w    $r13, TX_LA64_PRMD_PPLV_USER
    bne     $r12, $r13, .Ltx_la64_restore_gprs
    addi.d  $r13, $r31, TX_LA64_TF_SIZE
    csrwr   $r13, TX_LA64_CSR_KSAVE0_TRAP
    li.w    $r13, TX_LA64_LLBCTL_KLO_TRAP
    csrxchg $r13, $r13, TX_LA64_CSR_LLBCTL_TRAP
.Ltx_la64_restore_gprs:
    ld.d    $r1, $r31, TX_LA64_TF_R1
    ld.d    $r2, $r31, TX_LA64_TF_R2
    ld.d    $r4, $r31, TX_LA64_TF_R4
    ld.d    $r5, $r31, TX_LA64_TF_R5
    ld.d    $r6, $r31, TX_LA64_TF_R6
    ld.d    $r7, $r31, TX_LA64_TF_R7
    ld.d    $r8, $r31, TX_LA64_TF_R8
    ld.d    $r9, $r31, TX_LA64_TF_R9
    ld.d    $r10, $r31, TX_LA64_TF_R10
    ld.d    $r11, $r31, TX_LA64_TF_R11
    ld.d    $r12, $r31, TX_LA64_TF_R12
    ld.d    $r13, $r31, TX_LA64_TF_R13
    ld.d    $r14, $r31, TX_LA64_TF_R14
    ld.d    $r15, $r31, TX_LA64_TF_R15
    ld.d    $r16, $r31, TX_LA64_TF_R16
    ld.d    $r17, $r31, TX_LA64_TF_R17
    ld.d    $r18, $r31, TX_LA64_TF_R18
    ld.d    $r19, $r31, TX_LA64_TF_R19
    ld.d    $r20, $r31, TX_LA64_TF_R20
    ld.d    $r21, $r31, TX_LA64_TF_R21
    ld.d    $r22, $r31, TX_LA64_TF_R22
    ld.d    $r23, $r31, TX_LA64_TF_R23
    ld.d    $r24, $r31, TX_LA64_TF_R24
    ld.d    $r25, $r31, TX_LA64_TF_R25
    ld.d    $r26, $r31, TX_LA64_TF_R26
    ld.d    $r27, $r31, TX_LA64_TF_R27
    ld.d    $r28, $r31, TX_LA64_TF_R28
    ld.d    $r29, $r31, TX_LA64_TF_R29
    ld.d    $r30, $r31, TX_LA64_TF_R30
    ld.d    $sp, $r31, TX_LA64_TF_R3
    ld.d    $r31, $r31, TX_LA64_TF_R31
.Ltx_la64_trap_ertn:
    ertn
    .size tx_la64_qemu_exception_vector, . - tx_la64_qemu_exception_vector

    .align 12
    .globl tx_la64_qemu_tlb_refill_vector
    .type tx_la64_qemu_tlb_refill_vector, @function
tx_la64_qemu_tlb_refill_vector:
    // Fast TLB refill path: walk page-table directories directly
    // and fill TLB without constructing a full trap frame.
    csrwr   $r12, TX_LA64_CSR_TLBRSAVE_TRAP
    csrwr   $r13, TX_LA64_CSR_KSAVE3_TRAP
    csrrd   $r13, TX_LA64_CSR_KSAVE3_TRAP
    csrrd   $r12, TX_LA64_CSR_TLBRSAVE_TRAP
    csrrd   $r12, TX_LA64_CSR_PGD_TRAP

    // 4-level walk (Dir3 -> Dir2 -> Dir1 -> PTE pair).
    lddir   $r12, $r12, 3
    beqz    $r12, 1f
    srli.d  $r12, $r12, 12
    slli.d  $r12, $r12, 12

    lddir   $r12, $r12, 2
    beqz    $r12, 1f
    srli.d  $r12, $r12, 12
    slli.d  $r12, $r12, 12

    lddir   $r12, $r12, 1
    beqz    $r12, 1f
    srli.d  $r12, $r12, 12
    slli.d  $r12, $r12, 12

    ldpte   $r12, 0
    ldpte   $r12, 1
    tlbfill
    li.w    $r13, TX_LA64_LLBCTL_KLO_TRAP
    csrxchg $r13, $r13, TX_LA64_CSR_LLBCTL_TRAP
    csrrd   $r13, TX_LA64_CSR_KSAVE3_TRAP
    csrrd   $r12, TX_LA64_CSR_TLBRSAVE_TRAP
    ertn

1:
    // Missing page-table path: install invalid refill entry so the
    // next access is promoted to the normal page-fault path.
    csrwr   $zero, TX_LA64_CSR_TLBRELO0_TRAP
    csrwr   $zero, TX_LA64_CSR_TLBRELO1_TRAP
    tlbfill
    li.w    $r13, TX_LA64_LLBCTL_KLO_TRAP
    csrxchg $r13, $r13, TX_LA64_CSR_LLBCTL_TRAP
    csrrd   $r13, TX_LA64_CSR_KSAVE3_TRAP
    csrrd   $r12, TX_LA64_CSR_TLBRSAVE_TRAP
    ertn
    .size tx_la64_qemu_tlb_refill_vector, . - tx_la64_qemu_tlb_refill_vector

    .globl tx_la64_qemu_trap_low_end
    .type tx_la64_qemu_trap_low_end, @function
tx_la64_qemu_trap_low_end:

    .globl tx_la64_qemu_return_to_userspace
    .type tx_la64_qemu_return_to_userspace, @function
tx_la64_qemu_return_to_userspace:
    move    $r31, $a0
    ld.d    $r12, $r31, TX_LA64_TF_ERA
    csrwr   $r12, TX_LA64_CSR_ERA_TRAP
    ld.d    $r12, $r31, TX_LA64_TF_PRMD
    csrwr   $r12, TX_LA64_CSR_PRMD_TRAP
    csrwr   $zero, TX_LA64_CSR_TLBRERA_TRAP
    li.w    $r12, TX_LA64_LLBCTL_KLO_TRAP
    csrxchg $r12, $r12, TX_LA64_CSR_LLBCTL_TRAP
    ld.d    $r1, $r31, TX_LA64_TF_R1
    ld.d    $r2, $r31, TX_LA64_TF_R2
    ld.d    $r4, $r31, TX_LA64_TF_R4
    ld.d    $r5, $r31, TX_LA64_TF_R5
    ld.d    $r6, $r31, TX_LA64_TF_R6
    ld.d    $r7, $r31, TX_LA64_TF_R7
    ld.d    $r8, $r31, TX_LA64_TF_R8
    ld.d    $r9, $r31, TX_LA64_TF_R9
    ld.d    $r10, $r31, TX_LA64_TF_R10
    ld.d    $r11, $r31, TX_LA64_TF_R11
    ld.d    $r12, $r31, TX_LA64_TF_R12
    ld.d    $r13, $r31, TX_LA64_TF_R13
    ld.d    $r14, $r31, TX_LA64_TF_R14
    ld.d    $r15, $r31, TX_LA64_TF_R15
    ld.d    $r16, $r31, TX_LA64_TF_R16
    ld.d    $r17, $r31, TX_LA64_TF_R17
    ld.d    $r18, $r31, TX_LA64_TF_R18
    ld.d    $r19, $r31, TX_LA64_TF_R19
    ld.d    $r20, $r31, TX_LA64_TF_R20
    ld.d    $r21, $r31, TX_LA64_TF_R21
    ld.d    $r22, $r31, TX_LA64_TF_R22
    ld.d    $r23, $r31, TX_LA64_TF_R23
    ld.d    $r24, $r31, TX_LA64_TF_R24
    ld.d    $r25, $r31, TX_LA64_TF_R25
    ld.d    $r26, $r31, TX_LA64_TF_R26
    ld.d    $r27, $r31, TX_LA64_TF_R27
    ld.d    $r28, $r31, TX_LA64_TF_R28
    ld.d    $r29, $r31, TX_LA64_TF_R29
    ld.d    $r30, $r31, TX_LA64_TF_R30
    ld.d    $sp, $r31, TX_LA64_TF_R3
    ld.d    $r31, $r31, TX_LA64_TF_R31
    ertn
    .size tx_la64_qemu_return_to_userspace, . - tx_la64_qemu_return_to_userspace

    .globl tx_la64_qemu_activate_enter_userspace
    .type tx_la64_qemu_activate_enter_userspace, @function
tx_la64_qemu_activate_enter_userspace:
    // a0 = *mut KernelResumeCtx
    // a1 = *const La64TrapFrame
    // a2 = trap_stack_top
    // a3 = asid
    // a4 = pgdl
    // a5 = pgdh
    // a6 = switch_required
    // All Rust stack-dependent work must be complete before this
    // function. After CRMD.PG is written, do not return to Rust.
    st.d    $sp, $a0, TX_LA64_RCTX_SP
    st.d    $r1, $a0, TX_LA64_RCTX_RA
    st.d    $r21, $a0, TX_LA64_RCTX_R21
    st.d    $r2, $a0, TX_LA64_RCTX_TP
    st.d    $r22, $a0, (TX_LA64_RCTX_R22 + 0)
    st.d    $r23, $a0, (TX_LA64_RCTX_R22 + 8)
    st.d    $r24, $a0, (TX_LA64_RCTX_R22 + 16)
    st.d    $r25, $a0, (TX_LA64_RCTX_R22 + 24)
    st.d    $r26, $a0, (TX_LA64_RCTX_R22 + 32)
    st.d    $r27, $a0, (TX_LA64_RCTX_R22 + 40)
    st.d    $r28, $a0, (TX_LA64_RCTX_R22 + 48)
    st.d    $r29, $a0, (TX_LA64_RCTX_R22 + 56)
    st.d    $r30, $a0, (TX_LA64_RCTX_R22 + 64)
    st.d    $r31, $a0, (TX_LA64_RCTX_R22 + 72)

    csrwr   $a2, TX_LA64_CSR_KSAVE0_TRAP
    csrwr   $r21, TX_LA64_CSR_KSAVE1_TRAP
    csrwr   $r2, TX_LA64_CSR_KSAVE2_TRAP

    move    $r31, $a1
    ld.d    $r12, $r31, TX_LA64_TF_ERA
    csrwr   $r12, TX_LA64_CSR_ERA_TRAP
    ld.d    $r12, $r31, TX_LA64_TF_PRMD
    csrwr   $r12, TX_LA64_CSR_PRMD_TRAP
    csrwr   $zero, TX_LA64_CSR_TLBRERA_TRAP

    beqz    $a6, .Ltx_la64_activate_done
    csrwr   $a3, 0x18
    csrwr   $a4, 0x19
    csrwr   $a5, 0x1a
    li.d    $r12, 0x00000000000000b0
    csrwr   $r12, 0x00
    invtlb  0x0, $zero, $zero
    la.local $r12, .Ltx_la64_activate_done
    li.d    $r13, TX_LA64_PHYS_ADDR_MASK_TRAP
    and     $r12, $r12, $r13
    li.d    $r13, TX_LA64_DMW_CACHED_BASE_TRAP
    or      $r12, $r12, $r13
    jirl    $zero, $r12, 0
.Ltx_la64_activate_done:
    li.w    $r12, TX_LA64_LLBCTL_KLO_TRAP
    csrxchg $r12, $r12, TX_LA64_CSR_LLBCTL_TRAP
    ld.d    $r1, $r31, TX_LA64_TF_R1
    ld.d    $r2, $r31, TX_LA64_TF_R2
    ld.d    $r4, $r31, TX_LA64_TF_R4
    ld.d    $r5, $r31, TX_LA64_TF_R5
    ld.d    $r6, $r31, TX_LA64_TF_R6
    ld.d    $r7, $r31, TX_LA64_TF_R7
    ld.d    $r8, $r31, TX_LA64_TF_R8
    ld.d    $r9, $r31, TX_LA64_TF_R9
    ld.d    $r10, $r31, TX_LA64_TF_R10
    ld.d    $r11, $r31, TX_LA64_TF_R11
    ld.d    $r12, $r31, TX_LA64_TF_R12
    ld.d    $r13, $r31, TX_LA64_TF_R13
    ld.d    $r14, $r31, TX_LA64_TF_R14
    ld.d    $r15, $r31, TX_LA64_TF_R15
    ld.d    $r16, $r31, TX_LA64_TF_R16
    ld.d    $r17, $r31, TX_LA64_TF_R17
    ld.d    $r18, $r31, TX_LA64_TF_R18
    ld.d    $r19, $r31, TX_LA64_TF_R19
    ld.d    $r20, $r31, TX_LA64_TF_R20
    ld.d    $r21, $r31, TX_LA64_TF_R21
    ld.d    $r22, $r31, TX_LA64_TF_R22
    ld.d    $r23, $r31, TX_LA64_TF_R23
    ld.d    $r24, $r31, TX_LA64_TF_R24
    ld.d    $r25, $r31, TX_LA64_TF_R25
    ld.d    $r26, $r31, TX_LA64_TF_R26
    ld.d    $r27, $r31, TX_LA64_TF_R27
    ld.d    $r28, $r31, TX_LA64_TF_R28
    ld.d    $r29, $r31, TX_LA64_TF_R29
    ld.d    $r30, $r31, TX_LA64_TF_R30
    ld.d    $sp, $r31, TX_LA64_TF_R3
    ld.d    $r31, $r31, TX_LA64_TF_R31
    ertn
    .size tx_la64_qemu_activate_enter_userspace, . - tx_la64_qemu_activate_enter_userspace

    .globl tx_la64_resume_kernel_after_reschedule
    .type tx_la64_resume_kernel_after_reschedule, @function
tx_la64_resume_kernel_after_reschedule:
    ld.d    $sp,  $a0, TX_LA64_RCTX_SP
    ld.d    $r1,  $a0, TX_LA64_RCTX_RA
    ld.d    $r21, $a0, TX_LA64_RCTX_R21
    ld.d    $r2,  $a0, TX_LA64_RCTX_TP
    ld.d    $r22, $a0, (TX_LA64_RCTX_R22 + 0)
    ld.d    $r23, $a0, (TX_LA64_RCTX_R22 + 8)
    ld.d    $r24, $a0, (TX_LA64_RCTX_R22 + 16)
    ld.d    $r25, $a0, (TX_LA64_RCTX_R22 + 24)
    ld.d    $r26, $a0, (TX_LA64_RCTX_R22 + 32)
    ld.d    $r27, $a0, (TX_LA64_RCTX_R22 + 40)
    ld.d    $r28, $a0, (TX_LA64_RCTX_R22 + 48)
    ld.d    $r29, $a0, (TX_LA64_RCTX_R22 + 56)
    ld.d    $r30, $a0, (TX_LA64_RCTX_R22 + 64)
    ld.d    $r31, $a0, (TX_LA64_RCTX_R22 + 72)
    ret
    .size tx_la64_resume_kernel_after_reschedule, . - tx_la64_resume_kernel_after_reschedule

    .equ TX_LA64_CSR_EUEN, 0x02

    .globl tx_la64_qemu_fp_save_context
    .type tx_la64_qemu_fp_save_context, @function
tx_la64_qemu_fp_save_context:
    csrrd   $t0, TX_LA64_CSR_EUEN
    andi    $t0, $t0, 1
    beqz    $t0, .Ltx_la64_fp_save_none

    fst.d   $f0,  $a0,   0
    fst.d   $f1,  $a0,   8
    fst.d   $f2,  $a0,  16
    fst.d   $f3,  $a0,  24
    fst.d   $f4,  $a0,  32
    fst.d   $f5,  $a0,  40
    fst.d   $f6,  $a0,  48
    fst.d   $f7,  $a0,  56
    fst.d   $f8,  $a0,  64
    fst.d   $f9,  $a0,  72
    fst.d   $f10, $a0,  80
    fst.d   $f11, $a0,  88
    fst.d   $f12, $a0,  96
    fst.d   $f13, $a0, 104
    fst.d   $f14, $a0, 112
    fst.d   $f15, $a0, 120
    fst.d   $f16, $a0, 128
    fst.d   $f17, $a0, 136
    fst.d   $f18, $a0, 144
    fst.d   $f19, $a0, 152
    fst.d   $f20, $a0, 160
    fst.d   $f21, $a0, 168
    fst.d   $f22, $a0, 176
    fst.d   $f23, $a0, 184
    fst.d   $f24, $a0, 192
    fst.d   $f25, $a0, 200
    fst.d   $f26, $a0, 208
    fst.d   $f27, $a0, 216
    fst.d   $f28, $a0, 224
    fst.d   $f29, $a0, 232
    fst.d   $f30, $a0, 240
    fst.d   $f31, $a0, 248

    movfcsr2gr $t1, $fcsr0
    st.w    $t1, $a0, 256

    move    $t0, $zero
    movcf2gr $t1, $fcc7
    or      $t0, $t0, $t1
    slli.w  $t0, $t0, 1
    movcf2gr $t1, $fcc6
    or      $t0, $t0, $t1
    slli.w  $t0, $t0, 1
    movcf2gr $t1, $fcc5
    or      $t0, $t0, $t1
    slli.w  $t0, $t0, 1
    movcf2gr $t1, $fcc4
    or      $t0, $t0, $t1
    slli.w  $t0, $t0, 1
    movcf2gr $t1, $fcc3
    or      $t0, $t0, $t1
    slli.w  $t0, $t0, 1
    movcf2gr $t1, $fcc2
    or      $t0, $t0, $t1
    slli.w  $t0, $t0, 1
    movcf2gr $t1, $fcc1
    or      $t0, $t0, $t1
    slli.w  $t0, $t0, 1
    movcf2gr $t1, $fcc0
    or      $t0, $t0, $t1
    st.b    $t0, $a0, 260

    li.w    $t0, 3
    st.w    $t0, $a0, 264
    li.w    $a0, 1
    jr      $ra

.Ltx_la64_fp_save_none:
    st.w    $zero, $a0, 264
    move    $a0, $zero
    jr      $ra
    .size tx_la64_qemu_fp_save_context, . - tx_la64_qemu_fp_save_context

    .globl tx_la64_qemu_fp_restore_context
    .type tx_la64_qemu_fp_restore_context, @function
tx_la64_qemu_fp_restore_context:
    ld.w    $t0, $a0, 264
    andi    $t1, $t0, 1
    beqz    $t1, .Ltx_la64_fp_restore_disable

    csrrd   $t2, TX_LA64_CSR_EUEN
    ori     $t2, $t2, 1
    csrwr   $t2, TX_LA64_CSR_EUEN

    fld.d   $f0,  $a0,   0
    fld.d   $f1,  $a0,   8
    fld.d   $f2,  $a0,  16
    fld.d   $f3,  $a0,  24
    fld.d   $f4,  $a0,  32
    fld.d   $f5,  $a0,  40
    fld.d   $f6,  $a0,  48
    fld.d   $f7,  $a0,  56
    fld.d   $f8,  $a0,  64
    fld.d   $f9,  $a0,  72
    fld.d   $f10, $a0,  80
    fld.d   $f11, $a0,  88
    fld.d   $f12, $a0,  96
    fld.d   $f13, $a0, 104
    fld.d   $f14, $a0, 112
    fld.d   $f15, $a0, 120
    fld.d   $f16, $a0, 128
    fld.d   $f17, $a0, 136
    fld.d   $f18, $a0, 144
    fld.d   $f19, $a0, 152
    fld.d   $f20, $a0, 160
    fld.d   $f21, $a0, 168
    fld.d   $f22, $a0, 176
    fld.d   $f23, $a0, 184
    fld.d   $f24, $a0, 192
    fld.d   $f25, $a0, 200
    fld.d   $f26, $a0, 208
    fld.d   $f27, $a0, 216
    fld.d   $f28, $a0, 224
    fld.d   $f29, $a0, 232
    fld.d   $f30, $a0, 240
    fld.d   $f31, $a0, 248

    ld.w    $t1, $a0, 256
    movgr2fcsr $fcsr0, $t1

    ld.b    $t1, $a0, 260
    movgr2cf $fcc0, $t1
    srli.w  $t1, $t1, 1
    movgr2cf $fcc1, $t1
    srli.w  $t1, $t1, 1
    movgr2cf $fcc2, $t1
    srli.w  $t1, $t1, 1
    movgr2cf $fcc3, $t1
    srli.w  $t1, $t1, 1
    movgr2cf $fcc4, $t1
    srli.w  $t1, $t1, 1
    movgr2cf $fcc5, $t1
    srli.w  $t1, $t1, 1
    movgr2cf $fcc6, $t1
    srli.w  $t1, $t1, 1
    movgr2cf $fcc7, $t1
    jr      $ra

.Ltx_la64_fp_restore_disable:
    csrrd   $t2, TX_LA64_CSR_EUEN
    andi    $t2, $t2, 0xffe
    csrwr   $t2, TX_LA64_CSR_EUEN
    jr      $ra
    .size tx_la64_qemu_fp_restore_context, . - tx_la64_qemu_fp_restore_context
"#
);

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text, "ax"
    .align 2

    .globl tx_la64_cfu_raw
    .type  tx_la64_cfu_raw, @function
tx_la64_cfu_raw:
    beqz    $a2, .Lla64_cfu_ok
.Lla64_cfu_loop:
.globl tx_la64_cfu_ld_s
tx_la64_cfu_ld_s:
    ld.bu   $t0, $a1, 0
.globl tx_la64_cfu_ld_e
tx_la64_cfu_ld_e:
    st.b    $t0, $a0, 0
    addi.d  $a0, $a0, 1
    addi.d  $a1, $a1, 1
    addi.d  $a2, $a2, -1
    bnez    $a2, .Lla64_cfu_loop
.Lla64_cfu_ok:
    move    $a0, $zero
    jr      $ra
.globl tx_la64_cfu_fault
tx_la64_cfu_fault:
    jr      $ra
    .size tx_la64_cfu_raw, . - tx_la64_cfu_raw

    .globl tx_la64_ctu_raw
    .type  tx_la64_ctu_raw, @function
tx_la64_ctu_raw:
    beqz    $a2, .Lla64_ctu_ok
.Lla64_ctu_loop:
    ld.bu   $t0, $a1, 0
.globl tx_la64_ctu_st_s
tx_la64_ctu_st_s:
    st.b    $t0, $a0, 0
.globl tx_la64_ctu_st_e
tx_la64_ctu_st_e:
    addi.d  $a0, $a0, 1
    addi.d  $a1, $a1, 1
    addi.d  $a2, $a2, -1
    bnez    $a2, .Lla64_ctu_loop
.Lla64_ctu_ok:
    move    $a0, $zero
    jr      $ra
.globl tx_la64_ctu_fault
tx_la64_ctu_fault:
    jr      $ra
    .size tx_la64_ctu_raw, . - tx_la64_ctu_raw
"#
);
