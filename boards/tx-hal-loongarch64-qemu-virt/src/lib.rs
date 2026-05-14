#![no_std]

#[cfg(test)]
extern crate std;

use tx_hal::{
    AllocError, Arch, ArchAuxvFacts, Asid, AuxvIf, BootArg, BootHandoff, BootInfo, BootInfoIf,
    BootPlatformIf, BootProtocol, BootstrapPmapInfo, CacheIf, ConsoleIf, CpuId, CpuMask, DmaIf,
    EntropyIf, FaultInfo, FpSimdIf, InitIf, IpiKind, IrqDispatchTable, IrqHandled, IrqIf,
    KernelTrapSink, MemoryRegion, MemoryRegionKind, MmioFlags, MmioRegion, ObserverIf, PercpuIf,
    PhysAddr, PhysRange, PlatformConfig, PlatformInfo, PlatformInfoIf, PmapError, PmapIf,
    PmapInvalidation, PmapPermissions, PmapReservation, PmapReservationIntermediates,
    PmapReserveKind, PmapRoot, PmapUnmapResult, Pod, PowerIf, PtNode, PtNodeAllocator,
    SavedSignalFrame, SecondaryEntry, SignalFrameIf, SignalFramePlacement, SignalFrameWrite,
    SignalHandlerRegs, SmpIf, TimeIf, TrapAction, TrapClass, TrapFrameMut, TrapFrameMutVtable,
    TrapFrameSnapshot, TrapFrameView, TrapIf, TrapPreviousMode, UserFpContext, UserPtr,
    UserSignalMaskAbi, UserTrapContext, VirtAddr, VirtRange,
};

use la64_irq_trap::{classify_la64_trap, ensure_static_boot_facts};
pub use la64_irq_trap::{dispatch_trap_frame, return_to_userspace};
use la64_pmap::{la64_cached_virt, la64_uncached_virt, uart_put_byte, uart_try_get_byte};

use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .section .text.boot, "ax"
    .equ TX_LA64_DMW_CACHED,   0x9000000000000011
    .equ TX_LA64_DMW_UNCACHED, 0x8000000000000001
    .equ TX_LA64_DMW_CACHED_BASE, 0x9000000000000000
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

    li.d    $t0, TX_LA64_DMW_CACHED
    csrwr   $t0, TX_LA64_CSR_DMW0
    li.d    $t0, TX_LA64_DMW_UNCACHED
    csrwr   $t0, TX_LA64_CSR_DMW1
    move    $t0, $zero
    csrwr   $t0, TX_LA64_CSR_DMW2
    csrwr   $t0, TX_LA64_CSR_DMW3
    invtlb  0x0, $zero, $zero

    li.d    $t2, TX_LA64_PHYS_ADDR_MASK
    and     $sp, $sp, $t2
    li.d    $t2, TX_LA64_DMW_CACHED_BASE
    or      $sp, $sp, $t2
    move    $a0, $s0
    move    $a1, $s1
    move    $a2, $s2
    move    $a3, $s3
    la.local $t0, rust_entry
    li.d    $t2, TX_LA64_PHYS_ADDR_MASK
    and     $t0, $t0, $t2
    li.d    $t2, TX_LA64_DMW_CACHED_BASE
    or      $t0, $t0, $t2
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
    .equ TX_LA64_CSR_TLBRERA_TRAP, 0x8a
    .equ TX_LA64_CSR_TLBRSAVE_TRAP, 0x8b
    .equ TX_LA64_CSR_TLBRELO0_TRAP, 0x8c
    .equ TX_LA64_CSR_TLBRELO1_TRAP, 0x8d
    .equ TX_LA64_RCTX_SP, 0
    .equ TX_LA64_RCTX_RA, 8
    .equ TX_LA64_RCTX_R21, 16
    .equ TX_LA64_RCTX_TP, 24
    .equ TX_LA64_RCTX_R22, 32
    .equ TX_LA64_PRMD_PPLV_USER, 3

    .section .text.trap, "ax"

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
    bl      tx_la64_qemu_kernel_trap_entry

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
    csrrd   $r12, TX_LA64_CSR_TLBRSAVE_TRAP
    ertn

1:
    // Missing page-table path: install invalid refill entry so the
    // next access is promoted to the normal page-fault path.
    csrwr   $zero, TX_LA64_CSR_TLBRELO0_TRAP
    csrwr   $zero, TX_LA64_CSR_TLBRELO1_TRAP
    tlbfill
    csrrd   $r12, TX_LA64_CSR_TLBRSAVE_TRAP
    ertn
    .size tx_la64_qemu_tlb_refill_vector, . - tx_la64_qemu_tlb_refill_vector

    .globl tx_la64_qemu_return_to_userspace
    .type tx_la64_qemu_return_to_userspace, @function
tx_la64_qemu_return_to_userspace:
    move    $r31, $a0
    ld.d    $r12, $r31, TX_LA64_TF_ERA
    csrwr   $r12, TX_LA64_CSR_ERA_TRAP
    ld.d    $r12, $r31, TX_LA64_TF_PRMD
    csrwr   $r12, TX_LA64_CSR_PRMD_TRAP
    csrwr   $zero, TX_LA64_CSR_TLBRERA_TRAP
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

    csrwr   $a3, 0x18
    csrwr   $a4, 0x19
    csrwr   $a5, 0x1a
    li.d    $r12, 0x00000000000000b0
    csrwr   $r12, 0x00
    invtlb  0x0, $zero, $zero

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

pub struct Platform;

#[cfg(target_arch = "loongarch64")]
struct La64RawFixupEntry {
    pc_start: unsafe extern "C" fn(),
    pc_end: unsafe extern "C" fn(),
    recovery_pc: unsafe extern "C" fn(),
}

#[cfg(target_arch = "loongarch64")]
unsafe impl Sync for La64RawFixupEntry {}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_cfu_ld_s();
    fn tx_la64_cfu_ld_e();
    fn tx_la64_cfu_fault();
    fn tx_la64_ctu_st_s();
    fn tx_la64_ctu_st_e();
    fn tx_la64_ctu_fault();
    fn tx_la64_cfu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
    fn tx_la64_ctu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;
}

#[cfg(target_arch = "loongarch64")]
static LA64_FIXUP_TABLE: [La64RawFixupEntry; 2] = [
    La64RawFixupEntry {
        pc_start: tx_la64_cfu_ld_s,
        pc_end: tx_la64_cfu_ld_e,
        recovery_pc: tx_la64_cfu_fault,
    },
    La64RawFixupEntry {
        pc_start: tx_la64_ctu_st_s,
        pc_end: tx_la64_ctu_st_e,
        recovery_pc: tx_la64_ctu_fault,
    },
];

const QEMU_LA64_RAM_BASE: usize = 0;
const QEMU_LA64_RAM_SIZE: usize = 0x1000_0000;
const QEMU_LA64_RAM_END: usize = QEMU_LA64_RAM_BASE + QEMU_LA64_RAM_SIZE;
const QEMU_LA64_KERNEL_LOAD_BASE: usize = 0x0020_0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_PCH_PIC_BASE: usize = 0x1000_0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_ACPI_BASE: usize = 0x100d_0000;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_PM1_CNT: usize = QEMU_LA64_ACPI_BASE + 0x14;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_PM1_CNT_S5: u16 = (7 << 10) | (1 << 13);
const QEMU_LA64_GSI_BASE: u32 = 64;
const QEMU_LA64_PCH_PIC_IRQS: u32 = 64;
#[cfg_attr(not(test), allow(dead_code))]
const QEMU_LA64_UART0_IRQ: u32 = 66;
const QEMU_LA64_PCIE_ECAM_BASE: usize = 0x2000_0000;
const QEMU_LA64_PCIE_ECAM_SIZE: usize = 0x0800_0000;
const QEMU_LA64_PCIE_MMIO32_BASE: usize = 0x4000_0000;
const QEMU_LA64_PCIE_MMIO32_SIZE: usize = 0x4000_0000;
const QEMU_LA64_PCH_MSI_BASE: usize = 0x2ff0_0000;
const QEMU_LA64_PCH_MSI_SIZE: usize = 0x8;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const QEMU_LA64_FW_CFG_BASE: usize = 0x1e02_0000;
const QEMU_LA64_FDT_BASE: usize = 0x0010_0000;
const LA64_MAX_BOOT_CPUS: usize = 4;
#[cfg(target_arch = "loongarch64")]
const LA64_DEFAULT_POSSIBLE_CPUS: usize = LA64_MAX_BOOT_CPUS;
#[cfg(not(target_arch = "loongarch64"))]
const LA64_DEFAULT_POSSIBLE_CPUS: usize = 1;
const LA64_DMW_CACHED_BASE: usize = 0x9000_0000_0000_0000;
const LA64_DMW_UNCACHED_BASE: usize = 0x8000_0000_0000_0000;
const LA64_PHYS_ADDR_MASK: usize = (1usize << 48) - 1;
const LA64_CSR_CRMD: usize = 0x00;
#[cfg(target_arch = "loongarch64")]
const LA64_CSR_EUEN: usize = 0x02;
const LA64_CSR_ECFG: usize = 0x04;
const LA64_CSR_EENTRY: usize = 0x0c;
#[cfg(target_arch = "loongarch64")]
const LA64_CSR_KSAVE0: usize = 0x30;
const LA64_CSR_ASID: usize = 0x18;
const LA64_CSR_PGDL: usize = 0x19;
const LA64_CSR_PGDH: usize = 0x1a;
const LA64_CSR_PWCL: usize = 0x1c;
const LA64_CSR_PWCH: usize = 0x1d;
const LA64_CSR_STLBPS: usize = 0x1e;
const LA64_CSR_TLBRENTRY: usize = 0x88;
const LA64_CSR_TLBREHI: usize = 0x8e;
const LA64_CSR_MERRENTRY: usize = 0x93;
const LA64_CSR_TCFG: usize = 0x41;
const LA64_CSR_TICLR: usize = 0x44;
const LA64_CRMD_IE: usize = 1 << 2;
const LA64_CRMD_PG: usize = 1 << 4;
const LA64_CRMD_DATF_CC: usize = 0b01 << 5;
const LA64_CRMD_DATM_CC: usize = 0b01 << 7;
#[cfg(target_arch = "loongarch64")]
const LA64_EUEN_FPE: usize = 1 << 0;
const LA64_ASID_MASK: usize = 0x3ff;
const LA64_TCFG_ENABLE: usize = 1 << 0;
const LA64_TCFG_TICK_MASK: usize = !0x3;
const LA64_TICLR_CLEAR_TIMER: usize = 1 << 0;
const LA64_CPUCFG2_LLFTP: u32 = 1 << 14;
const LA64_CPUCFG2: usize = 0x2;
const LA64_CPUCFG4: usize = 0x4;
const LA64_CPUCFG5: usize = 0x5;
const LA64_ESTAT_IS_HWI_MASK: usize = 0xff << 2;
const LA64_ESTAT_IS_TIMER: usize = 1 << 11;
const LA64_ESTAT_IS_IPI: usize = 1 << 12;
const LA64_ESTAT_ECODE_SHIFT: usize = 16;
const LA64_ESTAT_ECODE_MASK: usize = 0x3f;
const LA64_ECODE_INT: usize = 0;
const LA64_ECODE_PIL: usize = 1;
const LA64_ECODE_PIS: usize = 2;
const LA64_ECODE_PIF: usize = 3;
const LA64_ECODE_PME: usize = 4;
const LA64_ECODE_PNR: usize = 5;
const LA64_ECODE_PNX: usize = 6;
const LA64_ECODE_PPI: usize = 7;
const LA64_ECODE_ADEF: usize = 8;
const LA64_ECODE_ADEM: usize = 9;
const LA64_ECODE_ALE: usize = 10;
const LA64_ECODE_SYS: usize = 11;
const LA64_ECODE_BRK: usize = 12;
const LA64_ECODE_INE: usize = 13;
const LA64_ECODE_IPE: usize = 14;
const LA64_ECODE_FPD: usize = 15;
const LA64_USER_TOP: usize = 0x0000_4000_0000_0000;
const LA64_PTE_PFN_MASK: u64 = ((1u64 << 48) - 1) & !((1u64 << 12) - 1);
const LA64_PTE_V: u64 = 1 << 0;
const LA64_PTE_A: u64 = 1 << 0;
const LA64_PTE_D: u64 = 1 << 1;
const LA64_PTE_PLV_USER: u64 = 0b11 << 2;
const LA64_PTE_MAT_SUC: u64 = 0b00 << 4;
const LA64_PTE_MAT_CC: u64 = 0b01 << 4;
const LA64_PTE_G: u64 = 1 << 6;
const LA64_PTE_PRESENT: u64 = 1 << 7;
const LA64_PTE_W: u64 = 1 << 8;
const LA64_PTE_M: u64 = 1 << 9;
const LA64_PTE_NR: u64 = 1 << 61;
const LA64_PTE_NX: u64 = 1 << 62;
const LA64_PTE_RPLV: u64 = 1 << 63;
const LA64_PRMD_PPLV_MASK: usize = 0x3;
const LA64_PRMD_PPLV_USER: usize = 0x3;
const LA64_PRMD_PIE: usize = 1 << 2;
const LA64_R_RA: usize = 1;
const LA64_R_TLS: usize = 2;
const LA64_R_SP: usize = 3;
const LA64_R_A0: usize = 4;
const LA64_R_A1: usize = 5;
const LA64_R_A2: usize = 6;
const LA64_R_A3: usize = 7;
const LA64_R_A4: usize = 8;
const LA64_R_A5: usize = 9;
const LA64_R_A7: usize = 11;
const LA64_SIGFRAME_ALIGN: usize = 16;
const LA64_SIGFRAME_MAGIC: u64 = 0x5458_5632_4c41_5331; // "TXV2LAS1"
const LA64_SIGFRAME_VERSION: u32 = 1;
const LA64_RT_SIGRETURN_SYSCALL: u32 = 139;
const LA64_ADDI_D_R11_ZERO_RT_SIGRETURN: u32 = la64_addi_d(11, 0, LA64_RT_SIGRETURN_SYSCALL);
const LA64_SYSCALL_0: u32 = 0x002b_0000;
const LA64_SIGRETURN_TRAMPOLINE: [u32; 2] = [LA64_ADDI_D_R11_ZERO_RT_SIGRETURN, LA64_SYSCALL_0];
const LA64_EIOINTC_BASE: usize = 0x1400;
const LA64_EIOINTC_ENABLE_START: usize = 0x200;
const LA64_EIOINTC_COREISR_START: usize = 0x400;
const LA64_EIOINTC_IRQS: u32 = 256;
const LA64_PCH_PIC_MASK_START: usize = 0x20;
const LA64_PCH_PIC_CLEAR_START: usize = 0x80;

const fn la64_addi_d(rd: u32, rj: u32, imm12: u32) -> u32 {
    0x02c0_0000 | ((imm12 & 0x0fff) << 10) | ((rj & 0x1f) << 5) | (rd & 0x1f)
}

static BOOT_FACTS_STATE: AtomicU8 = AtomicU8::new(0);
static LA64_BOOT_FIRMWARE_ARG: AtomicUsize = AtomicUsize::new(0);
static LA64_BOOT_EFI_BOOT: AtomicUsize = AtomicUsize::new(0);
static LA64_BOOT_CMDLINE_PTR: AtomicUsize = AtomicUsize::new(0);
static LA64_BOOT_SYSTEM_TABLE: AtomicUsize = AtomicUsize::new(0);
static INSTALLED_PT_NODE_ALLOCATOR: AtomicUsize = AtomicUsize::new(0);
static LA64_TIMEBASE_HZ: AtomicU64 = AtomicU64::new(0);
static LA64_POSSIBLE_CPU_COUNT: AtomicUsize = AtomicUsize::new(LA64_DEFAULT_POSSIBLE_CPUS);
static LA64_ONLINE_CPUS: AtomicU64 = AtomicU64::new(1);
static LA64_IPI_ACKED_CPUS: AtomicU64 = AtomicU64::new(0);
static LA64_IRQ_CONTEXT_DEPTH: AtomicUsize = AtomicUsize::new(0);
static LA64_IRQ_DISPATCH_TABLE: AtomicUsize = AtomicUsize::new(0);
static LA64_ALLOCATED_ASIDS: AtomicU64 = AtomicU64::new(1);
static LA64_KERNEL_PGDH_PHYS: AtomicUsize = AtomicUsize::new(0);
static LA64_ACTIVE_PGDL: AtomicUsize = AtomicUsize::new(0);
static LA64_ACTIVE_PGDH: AtomicUsize = AtomicUsize::new(0);
static LA64_ACTIVE_ASID: AtomicUsize = AtomicUsize::new(0);
static LA64_COMMITTED_PT_NODE_REGISTRY_LOCK: AtomicBool = AtomicBool::new(false);
static LA64_COMMITTED_PT_NODES: La64CommittedPtNodeRegistry =
    La64CommittedPtNodeRegistry(UnsafeCell::new([None; 256]));
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_KERNEL_TLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_EIOINTC_ENABLE0: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_EIOINTC_COREISR0: AtomicU64 = AtomicU64::new(0);
#[cfg(not(target_arch = "loongarch64"))]
static LA64_HOST_PCH_PIC_MASK: AtomicU64 = AtomicU64::new(u64::MAX);

pub fn capture_loongarch64_qemu_boot_args(
    efi_boot: usize,
    cmdline_phys: usize,
    system_table_phys: usize,
) {
    LA64_BOOT_EFI_BOOT.store(efi_boot, Ordering::Release);
    LA64_BOOT_CMDLINE_PTR.store(cmdline_phys, Ordering::Release);
    LA64_BOOT_SYSTEM_TABLE.store(system_table_phys, Ordering::Release);
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const LA64_FW_CFG_INITRD_CAPACITY: usize = 8 * 1024 * 1024;

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
#[repr(C, align(4096))]
struct La64FwCfgInitrdBuffer {
    bytes: [u8; LA64_FW_CFG_INITRD_CAPACITY],
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static mut LA64_FW_CFG_INITRD_BUFFER: La64FwCfgInitrdBuffer = La64FwCfgInitrdBuffer {
    bytes: [0; LA64_FW_CFG_INITRD_CAPACITY],
};

#[repr(C, align(8))]
pub struct KernelResumeCtx {
    pub sp: usize,
    pub ra: usize,
    pub r21: usize,
    pub tp: usize,
    pub r22: [usize; 10],
}

const _: () = assert!(core::mem::size_of::<KernelResumeCtx>() == 14 * 8);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, sp) == 0);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, ra) == 8);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, r21) == 16);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, tp) == 24);
const _: () = assert!(core::mem::offset_of!(KernelResumeCtx, r22) == 32);

#[repr(transparent)]
pub struct PerHartCell<T>(UnsafeCell<T>);

unsafe impl<T> Sync for PerHartCell<T> {}

impl<T> PerHartCell<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0.get()
    }
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static LA64_KERNEL_RESUME_CTX: [PerHartCell<KernelResumeCtx>; LA64_MAX_BOOT_CPUS] = [
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
    PerHartCell::new(KernelResumeCtx {
        sp: 0,
        ra: 0,
        r21: 0,
        tp: 0,
        r22: [0; 10],
    }),
];

const LA64_TRAP_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
pub struct La64TrapStack(pub [u8; LA64_TRAP_STACK_SIZE]);

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static LA64_TRAP_STACKS: [PerHartCell<La64TrapStack>; LA64_MAX_BOOT_CPUS] = [
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
    PerHartCell::new(La64TrapStack([0; LA64_TRAP_STACK_SIZE])),
];

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) fn la64_trap_stack_top_for_cpu(cpu: CpuId) -> usize {
    let stack = LA64_TRAP_STACKS[cpu.0].as_ptr();
    stack as usize + LA64_TRAP_STACK_SIZE
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) fn la64_kernel_resume_ctx_ptr_for_cpu(cpu: CpuId) -> *mut KernelResumeCtx {
    LA64_KERNEL_RESUME_CTX[cpu.0].as_ptr()
}

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
static LA64_ENTRY_TRAP_FRAMES: [PerHartCell<La64TrapFrame>; LA64_MAX_BOOT_CPUS] = [
    PerHartCell::new(La64TrapFrame::empty()),
    PerHartCell::new(La64TrapFrame::empty()),
    PerHartCell::new(La64TrapFrame::empty()),
    PerHartCell::new(La64TrapFrame::empty()),
];

#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
pub(crate) fn la64_entry_trap_frame_ptr_for_cpu(cpu: CpuId) -> *mut La64TrapFrame {
    LA64_ENTRY_TRAP_FRAMES[cpu.0].as_ptr()
}

struct La64CommittedPtNodeRegistry(UnsafeCell<[Option<PtNode>; 256]>);

unsafe impl Sync for La64CommittedPtNodeRegistry {}

struct La64CommittedPtNodeRegistryGuard;

impl Drop for La64CommittedPtNodeRegistryGuard {
    fn drop(&mut self) {
        LA64_COMMITTED_PT_NODE_REGISTRY_LOCK.store(false, Ordering::Release);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct La64TrapFrame {
    pub r: [usize; 32],
    pub estat: usize,
    pub era: usize,
    pub badv: usize,
    pub crmd: usize,
    pub prmd: usize,
}

impl La64TrapFrame {
    pub const fn empty() -> Self {
        Self {
            r: [0; 32],
            estat: 0,
            era: 0,
            badv: 0,
            crmd: 0,
            prmd: 0,
        }
    }

    pub const fn snapshot(&self) -> TrapFrameSnapshot {
        // `TrapFrameSnapshot` still carries RV64-flavoured field names. For
        // LA64, `scause/sepc/stval` transport `ESTAT/ERA/BADV` respectively.
        TrapFrameSnapshot {
            scause: self.estat,
            sepc: self.era,
            stval: self.badv,
        }
    }

    pub const fn previous_mode(&self) -> TrapPreviousMode {
        match self.prmd & LA64_PRMD_PPLV_MASK {
            LA64_PRMD_PPLV_USER => TrapPreviousMode::User,
            0 => TrapPreviousMode::Supervisor,
            _ => TrapPreviousMode::Unknown,
        }
    }

    pub const fn fault_address(&self) -> Option<VirtAddr> {
        match classify_la64_trap(self.estat) {
            TrapClass::PageFault { .. } | TrapClass::AlignmentFault { .. } => {
                Some(VirtAddr(self.badv))
            }
            _ => None,
        }
    }

    pub const fn faulting_instruction(&self) -> Option<VirtAddr> {
        match classify_la64_trap(self.estat) {
            TrapClass::TimerInterrupt
            | TrapClass::ExternalInterrupt
            | TrapClass::InterprocessorInterrupt
            | TrapClass::UnknownInterrupt => None,
            _ => Some(VirtAddr(self.era)),
        }
    }

    pub const fn interrupts_enabled_before(&self) -> bool {
        self.prmd & LA64_PRMD_PIE != 0
    }

    pub const fn view(&self) -> TrapFrameView {
        TrapFrameView::new(
            VirtAddr(self.era),
            VirtAddr(self.r[LA64_R_SP]),
            self.r[LA64_R_A7] as u64,
            [
                self.r[LA64_R_A0] as u64,
                self.r[LA64_R_A1] as u64,
                self.r[LA64_R_A2] as u64,
                self.r[LA64_R_A3] as u64,
                self.r[LA64_R_A4] as u64,
                self.r[LA64_R_A5] as u64,
            ],
            self.fault_address(),
            self.faulting_instruction(),
            self.previous_mode(),
            self.interrupts_enabled_before(),
            self.r[LA64_R_TLS] as u64,
        )
    }

    pub fn view_mut(&mut self) -> TrapFrameMut<'_> {
        let view = TrapFrameView::new(
            VirtAddr(self.era),
            VirtAddr(self.r[LA64_R_SP]),
            self.r[LA64_R_A7] as u64,
            [
                self.r[LA64_R_A0] as u64,
                self.r[LA64_R_A1] as u64,
                self.r[LA64_R_A2] as u64,
                self.r[LA64_R_A3] as u64,
                self.r[LA64_R_A4] as u64,
                self.r[LA64_R_A5] as u64,
            ],
            self.fault_address(),
            self.faulting_instruction(),
            self.previous_mode(),
            self.interrupts_enabled_before(),
            self.r[LA64_R_TLS] as u64,
        );
        let raw = NonNull::from(&mut *self).cast::<()>();
        unsafe { TrapFrameMut::from_raw_parts(view, raw, &LA64_TRAP_FRAME_MUT_VTABLE) }
    }

    fn set_pc(&mut self, pc: VirtAddr) {
        self.era = pc.0;
    }

    fn set_sp(&mut self, sp: VirtAddr) {
        self.r[LA64_R_SP] = sp.0;
    }

    fn set_syscall_return(&mut self, value: i64) {
        self.r[LA64_R_A0] = value as usize;
    }

    fn set_syscall_error(&mut self, errno: i32) {
        self.r[LA64_R_A0] = (-(errno as isize)) as usize;
    }

    fn set_user_tls_register(&mut self, value: u64) {
        self.r[LA64_R_TLS] = value as usize;
    }

    fn capture_user_context(&self) -> UserTrapContext {
        UserTrapContext {
            regs: self.r,
            pc: self.era,
            status: self.prmd,
            fp: la64_capture_fp_context(),
        }
    }

    fn restore_user_context(&mut self, context: &UserTrapContext) {
        self.r = context.regs;
        self.r[0] = 0;
        self.era = context.pc;
        self.prmd = context.status;
        la64_restore_fp_context(&context.fp);
        self.prepare_user_return();
    }

    fn set_signal_handler_regs(&mut self, regs: SignalHandlerRegs) {
        self.r[LA64_R_RA] = regs.return_pc.0;
        self.r[LA64_R_A0] = regs.args[0];
        self.r[LA64_R_A1] = regs.args[1];
        self.r[LA64_R_A2] = regs.args[2];
    }

    fn rewind_pc(&mut self, bytes: usize) {
        self.era = self.era.saturating_sub(bytes);
    }

    pub fn prepare_user_return(&mut self) {
        self.prmd &= !LA64_PRMD_PPLV_MASK;
        self.prmd |= LA64_PRMD_PPLV_USER | LA64_PRMD_PIE;
    }
}

static LA64_TRAP_FRAME_MUT_VTABLE: TrapFrameMutVtable = TrapFrameMutVtable {
    read_view: la64_read_view,
    set_pc: la64_set_pc,
    set_sp: la64_set_sp,
    set_syscall_return: la64_set_syscall_return,
    set_syscall_error: la64_set_syscall_error,
    set_user_tls_register: la64_set_user_tls_register,
    capture_user_context: la64_capture_user_context,
    restore_user_context: la64_restore_user_context,
    set_signal_handler_regs: la64_set_signal_handler_regs,
    rewind_pc: la64_rewind_pc,
};

fn la64_frame_ptr(raw: NonNull<()>) -> *mut La64TrapFrame {
    raw.cast::<La64TrapFrame>().as_ptr()
}

fn la64_read_view(raw: NonNull<()>) -> TrapFrameView {
    unsafe { (*la64_frame_ptr(raw)).view() }
}

fn la64_set_pc(raw: NonNull<()>, pc: VirtAddr) {
    unsafe { (*la64_frame_ptr(raw)).set_pc(pc) };
}

fn la64_set_sp(raw: NonNull<()>, sp: VirtAddr) {
    unsafe { (*la64_frame_ptr(raw)).set_sp(sp) };
}

fn la64_set_syscall_return(raw: NonNull<()>, value: i64) {
    unsafe { (*la64_frame_ptr(raw)).set_syscall_return(value) };
}

fn la64_set_syscall_error(raw: NonNull<()>, errno: i32) {
    unsafe { (*la64_frame_ptr(raw)).set_syscall_error(errno) };
}

fn la64_set_user_tls_register(raw: NonNull<()>, value: u64) {
    unsafe { (*la64_frame_ptr(raw)).set_user_tls_register(value) };
}

fn la64_capture_user_context(raw: NonNull<()>) -> UserTrapContext {
    unsafe { (*la64_frame_ptr(raw)).capture_user_context() }
}

fn la64_restore_user_context(raw: NonNull<()>, context: &UserTrapContext) {
    unsafe { (*la64_frame_ptr(raw)).restore_user_context(context) };
}

fn la64_set_signal_handler_regs(raw: NonNull<()>, regs: SignalHandlerRegs) {
    unsafe { (*la64_frame_ptr(raw)).set_signal_handler_regs(regs) };
}

fn la64_rewind_pc(raw: NonNull<()>, bytes: usize) {
    unsafe { (*la64_frame_ptr(raw)).rewind_pc(bytes) };
}

#[cfg(target_arch = "loongarch64")]
unsafe extern "C" {
    fn tx_la64_qemu_fp_save_context(ctx: *mut UserFpContext) -> usize;
    fn tx_la64_qemu_fp_restore_context(ctx: *const UserFpContext);
}

#[cfg(target_arch = "loongarch64")]
fn la64_capture_fp_context() -> UserFpContext {
    let mut fp = <Platform as FpSimdIf>::init_state();
    <Platform as FpSimdIf>::save(&mut fp);
    fp
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_capture_fp_context() -> UserFpContext {
    let mut fp = <Platform as FpSimdIf>::init_state();
    <Platform as FpSimdIf>::save(&mut fp);
    fp
}

#[cfg(target_arch = "loongarch64")]
fn la64_restore_fp_context(fp: &UserFpContext) {
    <Platform as FpSimdIf>::restore(fp);
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_restore_fp_context(fp: &UserFpContext) {
    <Platform as FpSimdIf>::restore(fp);
}

#[cfg(target_arch = "loongarch64")]
fn la64_set_fpu_enabled(enabled: bool) {
    let mut euen = la64_irq_trap::read_la64_csr(LA64_CSR_EUEN);
    if enabled {
        euen |= LA64_EUEN_FPE;
    } else {
        euen &= !LA64_EUEN_FPE;
    }
    la64_irq_trap::write_la64_csr(LA64_CSR_EUEN, euen);
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_set_fpu_enabled(_enabled: bool) {}

#[cfg(target_arch = "loongarch64")]
fn la64_save_fp_context(state: &mut UserFpContext) {
    let saved = unsafe { tx_la64_qemu_fp_save_context(core::ptr::addr_of_mut!(*state)) };
    if saved == 0 {
        *state = UserFpContext::empty();
    }
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_save_fp_context(state: &mut UserFpContext) {
    *state = UserFpContext::empty();
}

#[cfg(target_arch = "loongarch64")]
fn la64_restore_fp_context_raw(state: &UserFpContext) {
    unsafe { tx_la64_qemu_fp_restore_context(core::ptr::addr_of!(*state)) };
}

#[cfg(not(target_arch = "loongarch64"))]
fn la64_restore_fp_context_raw(state: &UserFpContext) {
    #[cfg(test)]
    {
        *TEST_RESTORED_FP_CONTEXT
            .lock()
            .expect("LA64 restored FP test mutex poisoned") = *state;
    }

    #[cfg(not(test))]
    {
        let _ = state;
    }
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
static TEST_RESTORED_FP_CONTEXT: std::sync::Mutex<UserFpContext> =
    std::sync::Mutex::new(UserFpContext::empty());

#[cfg(all(test, not(target_arch = "loongarch64")))]
fn la64_test_reset_restored_fp_context() {
    *TEST_RESTORED_FP_CONTEXT
        .lock()
        .expect("LA64 restored FP test mutex poisoned") = UserFpContext::empty();
}

#[cfg(all(test, not(target_arch = "loongarch64")))]
fn la64_test_restored_fp_context() -> UserFpContext {
    *TEST_RESTORED_FP_CONTEXT
        .lock()
        .expect("LA64 restored FP test mutex poisoned")
}

const LA64_BOOT_MEMORY_REGION_CAPACITY: usize = 8;
static mut BOOT_MEMORY_REGIONS: [MemoryRegion; LA64_BOOT_MEMORY_REGION_CAPACITY] = [
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Usable,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
    MemoryRegion {
        base: PhysAddr(0),
        size: 0,
        kind: MemoryRegionKind::Reserved,
    },
];

static mut BOOT_INFO: BootInfo = BootInfo::empty();
const LA64_BOOT_CMDLINE_CAPACITY: usize = 256;
static mut BOOT_CMDLINE: [u8; LA64_BOOT_CMDLINE_CAPACITY] = [0; LA64_BOOT_CMDLINE_CAPACITY];

static mut BOOTSTRAP_PMAP_INFO: BootstrapPmapInfo = BootstrapPmapInfo {
    root: PhysAddr(0),
    mapped: PhysRange::empty(),
    direct_map_base: VirtAddr(0),
    direct_map: VirtRange::empty(),
    kernel_image: VirtRange::empty(),
    identity: None,
    pt_node_pool: PhysRange::empty(),
    reserved_page_tables: &[],
};

// QEMU loongson3-virt exposes the first serial port as an 8250-compatible
// UART at 0x1fe0_01e0; Linux examples use earlycon=uart,mmio,0x1fe001e0.
const QEMU_LA64_UART0_BASE: usize = 0x1fe0_01e0;
const QEMU_LA64_UART0_SIZE: usize = 0x100;
#[cfg_attr(not(test), allow(dead_code))]
const QEMU_LA64_UART0_PAGE_BASE: usize = 0x1fe0_0000;
#[cfg(target_arch = "loongarch64")]
const UART_RBR: usize = 0x00;
const UART_THR: usize = 0x00;
#[cfg_attr(not(target_arch = "loongarch64"), allow(dead_code))]
const UART_IER: usize = 0x01;
#[cfg_attr(not(any(target_arch = "loongarch64", test)), allow(dead_code))]
const UART_IER_ERBFI: u8 = 1 << 0;
const UART_LSR: usize = 0x05;
#[cfg(target_arch = "loongarch64")]
const UART_LSR_DR: u8 = 1 << 0;
const UART_LSR_THRE: u8 = 1 << 5;

// The early UART is reachable through QEMU's current direct/identity execution
// convention. Phase-3 substrate MMIO mapping treats this exact page as already
// covered; all non-identity requests remain unsupported until LA64 owns real
// DMW/page-table mutation.
static MMIO_REGIONS: &[MmioRegion] = &[
    MmioRegion {
        name: "uart0",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_UART0_BASE),
            size: QEMU_LA64_UART0_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_UART0_BASE)),
            size: QEMU_LA64_UART0_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "pcie-ecam",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_PCIE_ECAM_BASE),
            size: QEMU_LA64_PCIE_ECAM_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_PCIE_ECAM_BASE)),
            size: QEMU_LA64_PCIE_ECAM_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "pcie-mmio32",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_PCIE_MMIO32_BASE),
            size: QEMU_LA64_PCIE_MMIO32_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_PCIE_MMIO32_BASE)),
            size: QEMU_LA64_PCIE_MMIO32_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
    MmioRegion {
        name: "pch-msi",
        phys: PhysRange {
            start: PhysAddr(QEMU_LA64_PCH_MSI_BASE),
            size: QEMU_LA64_PCH_MSI_SIZE,
        },
        virt: VirtRange {
            start: VirtAddr(la64_uncached_virt(QEMU_LA64_PCH_MSI_BASE)),
            size: QEMU_LA64_PCH_MSI_SIZE,
        },
        flags: MmioFlags::DEVICE_NGNRNE
            .union(MmioFlags::READ)
            .union(MmioFlags::WRITE),
    },
];

static mut PLATFORM_INFO: PlatformInfo = PlatformInfo {
    board: Platform::BOARD,
    spi_sd: None,
    mmio_regions: MMIO_REGIONS,
    timebase_frequency_hz: 0,
    possible_cpu_count: LA64_DEFAULT_POSSIBLE_CPUS,
};

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct La64SignalFrame {
    magic: u64,
    version: u32,
    frame_size: u32,
    sig_no: u32,
    _reserved0: u32,
    flags: u64,
    siginfo: tx_hal::UserSigInfoAbi,
    saved_mask: UserSignalMaskAbi,
    // Includes full GPR + PC + status and UserFpContext payload.
    user_context: UserTrapContext,
    trampoline: [u32; 2],
}

unsafe impl Pod for La64SignalFrame {}

impl La64SignalFrame {
    fn new(tf: &TrapFrameMut<'_>, setup: &SignalFrameWrite) -> Self {
        Self {
            magic: LA64_SIGFRAME_MAGIC,
            version: LA64_SIGFRAME_VERSION,
            frame_size: core::mem::size_of::<Self>() as u32,
            sig_no: setup.sig_no,
            _reserved0: 0,
            flags: setup.flags.bits,
            siginfo: setup.siginfo,
            saved_mask: setup.old_mask,
            user_context: tf.capture_user_context(),
            trampoline: LA64_SIGRETURN_TRAMPOLINE,
        }
    }

    fn validate(&self, user_sp: UserPtr<u8>) -> Result<(), FaultInfo> {
        if self.magic == LA64_SIGFRAME_MAGIC
            && self.version == LA64_SIGFRAME_VERSION
            && self.frame_size as usize == core::mem::size_of::<Self>()
            && self.trampoline == LA64_SIGRETURN_TRAMPOLINE
        {
            Ok(())
        } else {
            Err(FaultInfo {
                address: VirtAddr(user_sp.addr()),
                write: false,
                instruction: false,
                from_user: false,
            })
        }
    }
}

impl PlatformConfig for Platform {
    const ARCH: Arch = Arch::LoongArch64;
    const BOARD: &'static str = "qemu-loongarch64-virt";
    const SUBSTRATE_BOOT_READY: bool = true;
    const PHYS_ADDR_BITS: u8 = 48;
    const VIRT_ADDR_BITS: u8 = 48;
    const DIRECT_MAP_BASE: VirtAddr = VirtAddr(LA64_DMW_CACHED_BASE);
    const DIRECT_MAP_SIZE: usize = QEMU_LA64_RAM_SIZE;
    const KERNEL_VIRT_BASE: VirtAddr = VirtAddr(la64_cached_virt(QEMU_LA64_KERNEL_LOAD_BASE));
    const USER_TOP: VirtAddr = VirtAddr(LA64_USER_TOP);
    const KERNEL_STACK_SIZE: usize = 128 * 1024;
    const KERNEL_STACK_ALIGN: usize = Self::PAGE_SIZE;
    const PAGE_TABLE_LEVELS: u8 = 4;
    const ASID_BITS: u8 = 10;
    const CACHE_LINE_SIZE: usize = 64;
    const DMA_COHERENT: bool = true;
}

impl BootPlatformIf for Platform {
    const BOOT_PROTOCOL: BootProtocol = BootProtocol::LoongArchFirmware;

    fn boot_handoff(cpu_id: usize, firmware_arg: usize) -> BootHandoff {
        LA64_BOOT_FIRMWARE_ARG.store(firmware_arg, Ordering::Release);
        ensure_static_boot_facts();

        BootHandoff {
            cpu_id: CpuId(cpu_id),
            firmware_arg: BootArg(firmware_arg),
            protocol: Self::BOOT_PROTOCOL,
        }
    }
}

impl InitIf for Platform {
    fn init_early(_handoff: BootHandoff) {}
    fn init_later(_handoff: BootHandoff) {}
}

impl BootInfoIf for Platform {
    fn boot_info() -> &'static BootInfo {
        ensure_static_boot_facts();

        unsafe { &*core::ptr::addr_of!(BOOT_INFO) }
    }
}

impl PlatformInfoIf for Platform {
    fn platform_info() -> &'static PlatformInfo {
        ensure_static_boot_facts();

        unsafe { &*core::ptr::addr_of!(PLATFORM_INFO) }
    }
}

impl AuxvIf for Platform {
    fn arch_auxv_facts() -> ArchAuxvFacts {
        ArchAuxvFacts::new(Self::PAGE_SIZE, 0, 0, "loongarch64")
    }
}
impl ConsoleIf for Platform {
    fn write_bytes(bytes: &[u8]) {
        for &byte in bytes {
            uart_put_byte(byte);
        }
    }

    fn read_bytes(buf: &mut [u8]) -> usize {
        let mut read = 0;
        for byte in buf {
            let Some(next) = uart_try_get_byte() else {
                break;
            };
            *byte = next;
            read += 1;
        }
        read
    }
}

impl ObserverIf for Platform {}

mod dtb;
mod la64_irq_trap;
mod la64_pmap;
mod la64_unaligned;
mod platform_impls;

#[cfg(test)]
mod tests;
