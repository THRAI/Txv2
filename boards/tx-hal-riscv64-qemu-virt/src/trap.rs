use core::ptr::NonNull;

use crate::{boot_static, user_access, Platform};
#[cfg(target_arch = "riscv64")]
use crate::{current_kernel_resume_ctx_ptr, trap_stack_top_for_cpu, KernelResumeCtx};
#[cfg(target_arch = "riscv64")]
use tx_hal::SmpIf;
use tx_hal::{
    FaultInfo, KernelTrapSink, PercpuIf, PmapIf, PmapRoot, SignalHandlerRegs, TrapAction,
    TrapClass, TrapFrameMut, TrapFrameMutVtable, TrapFrameSnapshot, TrapFrameView, TrapIf,
    TrapPreviousMode, UserFpContext, UserTrapContext, VirtAddr,
};

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text.trap, "ax"
    .option push
    .option arch, +f, +d
    .align 2
    .equ TX_RV64_TF_X0, 0
    .equ TX_RV64_TF_X1, 8
    .equ TX_RV64_TF_X2, 16
    .equ TX_RV64_TF_X3, 24
    .equ TX_RV64_TF_X4, 32
    .equ TX_RV64_TF_X5, 40
    .equ TX_RV64_TF_X6, 48
    .equ TX_RV64_TF_X7, 56
    .equ TX_RV64_TF_X8, 64
    .equ TX_RV64_TF_X9, 72
    .equ TX_RV64_TF_X10, 80
    .equ TX_RV64_TF_X11, 88
    .equ TX_RV64_TF_X12, 96
    .equ TX_RV64_TF_X13, 104
    .equ TX_RV64_TF_X14, 112
    .equ TX_RV64_TF_X15, 120
    .equ TX_RV64_TF_X16, 128
    .equ TX_RV64_TF_X17, 136
    .equ TX_RV64_TF_X18, 144
    .equ TX_RV64_TF_X19, 152
    .equ TX_RV64_TF_X20, 160
    .equ TX_RV64_TF_X21, 168
    .equ TX_RV64_TF_X22, 176
    .equ TX_RV64_TF_X23, 184
    .equ TX_RV64_TF_X24, 192
    .equ TX_RV64_TF_X25, 200
    .equ TX_RV64_TF_X26, 208
    .equ TX_RV64_TF_X27, 216
    .equ TX_RV64_TF_X28, 224
    .equ TX_RV64_TF_X29, 232
    .equ TX_RV64_TF_X30, 240
    .equ TX_RV64_TF_X31, 248
    .equ TX_RV64_TF_SCAUSE, 256
    .equ TX_RV64_TF_SEPC, 264
    .equ TX_RV64_TF_STVAL, 272
    .equ TX_RV64_TF_SSTATUS, 280
    .equ TX_RV64_TF_F_BASE, 288
    .equ TX_RV64_TF_F0,  TX_RV64_TF_F_BASE + 0*8
    .equ TX_RV64_TF_F1,  TX_RV64_TF_F_BASE + 1*8
    .equ TX_RV64_TF_F2,  TX_RV64_TF_F_BASE + 2*8
    .equ TX_RV64_TF_F3,  TX_RV64_TF_F_BASE + 3*8
    .equ TX_RV64_TF_F4,  TX_RV64_TF_F_BASE + 4*8
    .equ TX_RV64_TF_F5,  TX_RV64_TF_F_BASE + 5*8
    .equ TX_RV64_TF_F6,  TX_RV64_TF_F_BASE + 6*8
    .equ TX_RV64_TF_F7,  TX_RV64_TF_F_BASE + 7*8
    .equ TX_RV64_TF_F8,  TX_RV64_TF_F_BASE + 8*8
    .equ TX_RV64_TF_F9,  TX_RV64_TF_F_BASE + 9*8
    .equ TX_RV64_TF_F10, TX_RV64_TF_F_BASE + 10*8
    .equ TX_RV64_TF_F11, TX_RV64_TF_F_BASE + 11*8
    .equ TX_RV64_TF_F12, TX_RV64_TF_F_BASE + 12*8
    .equ TX_RV64_TF_F13, TX_RV64_TF_F_BASE + 13*8
    .equ TX_RV64_TF_F14, TX_RV64_TF_F_BASE + 14*8
    .equ TX_RV64_TF_F15, TX_RV64_TF_F_BASE + 15*8
    .equ TX_RV64_TF_F16, TX_RV64_TF_F_BASE + 16*8
    .equ TX_RV64_TF_F17, TX_RV64_TF_F_BASE + 17*8
    .equ TX_RV64_TF_F18, TX_RV64_TF_F_BASE + 18*8
    .equ TX_RV64_TF_F19, TX_RV64_TF_F_BASE + 19*8
    .equ TX_RV64_TF_F20, TX_RV64_TF_F_BASE + 20*8
    .equ TX_RV64_TF_F21, TX_RV64_TF_F_BASE + 21*8
    .equ TX_RV64_TF_F22, TX_RV64_TF_F_BASE + 22*8
    .equ TX_RV64_TF_F23, TX_RV64_TF_F_BASE + 23*8
    .equ TX_RV64_TF_F24, TX_RV64_TF_F_BASE + 24*8
    .equ TX_RV64_TF_F25, TX_RV64_TF_F_BASE + 25*8
    .equ TX_RV64_TF_F26, TX_RV64_TF_F_BASE + 26*8
    .equ TX_RV64_TF_F27, TX_RV64_TF_F_BASE + 27*8
    .equ TX_RV64_TF_F28, TX_RV64_TF_F_BASE + 28*8
    .equ TX_RV64_TF_F29, TX_RV64_TF_F_BASE + 29*8
    .equ TX_RV64_TF_F30, TX_RV64_TF_F_BASE + 30*8
    .equ TX_RV64_TF_F31, TX_RV64_TF_F_BASE + 31*8
    .equ TX_RV64_TF_FCSR, TX_RV64_TF_F_BASE + 256
    # Keep the frame 16-byte aligned at every Rust call boundary.  The
    # register payload occupies 552 bytes; the final 8 bytes are ABI padding.
    .equ TX_RV64_TF_SIZE, 560
    .equ TX_RV64_TF_TMP_T0, TX_RV64_TF_SIZE - 16
    .equ TX_RV64_TF_TMP_SSCRATCH, TX_RV64_TF_SIZE - 8

    # Per-hart KernelResumeCtx field offsets — must match
    # `boards::tx_hal_riscv64_qemu_virt::KernelResumeCtx` in lib.rs.
    .equ TX_RV64_RCTX_SP, 0
    .equ TX_RV64_RCTX_RA, 8
    .equ TX_RV64_RCTX_S0, 16
    # s1..s11 follow at +8 each up to offset 104.
    # Rv64PerCpuArea::trap_stack_top, addressed through kernel tp.
    .equ TX_RV64_PERCPU_TRAP_STACK_TOP, 24

    .globl tx_rv64_qemu_minimal_trap_vector
tx_rv64_qemu_minimal_trap_vector:
    # User traps cannot keep using their user stack, so switch them to
    # the per-hart trap stack through sscratch. A normal kernel trap
    # obtains the same dedicated stack top from the local per-CPU area
    # through kernel tp; unlike sscratch, that value cannot be a stale
    # interrupted sp. A trap already nested on the dedicated trap stack
    # pushes directly below its current sp.
    #
    # This distinction is load-bearing. The old code also swapped
    # kernel-mode traps through sscratch. If sscratch temporarily held an
    # interrupted kernel sp (for example during a nested trap), a later
    # interrupt treated that stale address as a stack top and wrote a
    # TrapFrame into the middle of the live reactor frame.
    #
    # Txv2's user VA is low-half and every kernel/trap stack is high-half,
    # so the sign bit of the trap-time sp is the architecture-level
    # from-user discriminator used before any GPR is available to save.
    bgez sp, 7f

    # From kernel. Preserve the temporaries used to identify the current
    # stack plus the exact old sscratch value before clobbering them.
    addi sp, sp, -32
    sd t0, 0(sp)
    sd t1, 8(sp)
    csrr t0, sscratch
    sd t0, 16(sp)

    # If top-original_sp is in [0, 64 KiB], execution is already on
    # this hart's trap stack and this is a nested trap.
    ld t0, TX_RV64_PERCPU_TRAP_STACK_TOP(tp)
    addi t1, sp, 32
    sub t0, t0, t1
    li t1, 65537
    bltu t0, t1, 6f

    # Ordinary from-kernel trap: switch to the dedicated trap stack
    # obtained from tp. X0=2 uses the same swap-back exit shape as a
    # user trap, with X2 holding the interrupted kernel sp.
    addi t1, sp, 32
    ld t0, TX_RV64_PERCPU_TRAP_STACK_TOP(tp)
    mv sp, t0
    addi sp, sp, -TX_RV64_TF_SIZE
    ld t0, -32(t1)
    sd t0, TX_RV64_TF_X5(sp)
    ld t0, -24(t1)
    sd t0, TX_RV64_TF_X6(sp)
    ld t0, -16(t1)
    sd t0, TX_RV64_TF_TMP_SSCRATCH(sp)
    li t0, 2
    sd t0, TX_RV64_TF_X0(sp)
    sd ra, TX_RV64_TF_X1(sp)
    sd t1, TX_RV64_TF_X2(sp)
    sd gp, TX_RV64_TF_X3(sp)
    sd tp, TX_RV64_TF_X4(sp)
    j 9f

6:
    # Nested trap: finish allocating below the active trap-handler
    # stack. Copy the temporary saves before FP capture reuses their
    # source offsets in the final frame.
    addi sp, sp, -(TX_RV64_TF_SIZE - 32)
    ld t0, (TX_RV64_TF_SIZE - 32)(sp)
    sd t0, TX_RV64_TF_X5(sp)
    ld t0, (TX_RV64_TF_SIZE - 24)(sp)
    sd t0, TX_RV64_TF_X6(sp)
    ld t0, (TX_RV64_TF_SIZE - 16)(sp)
    sd t0, TX_RV64_TF_TMP_SSCRATCH(sp)
    li t0, 1
    sd t0, TX_RV64_TF_X0(sp)
    sd ra, TX_RV64_TF_X1(sp)
    addi t0, sp, TX_RV64_TF_SIZE
    sd t0, TX_RV64_TF_X2(sp)
    sd gp, TX_RV64_TF_X3(sp)
    sd tp, TX_RV64_TF_X4(sp)
    j 9f

7:
    # From user. sscratch is boot-/exit-primed with this hart's
    # trap-stack top. After the swap it retains the interrupted user sp.
    csrrw sp, sscratch, sp

    # Allocate the trap frame on the trap stack.
    addi sp, sp, -TX_RV64_TF_SIZE
    sd t0, TX_RV64_TF_X5(sp)
    sd zero, TX_RV64_TF_X0(sp)
    sd ra, TX_RV64_TF_X1(sp)
    # The trap-time sp is currently in sscratch; save it to the
    # frame's X_SP slot. Subsystems read it via TrapFrameView.
    csrr t0, sscratch
    sd t0, TX_RV64_TF_X2(sp)
8:
    sd gp, TX_RV64_TF_X3(sp)
    sd tp, TX_RV64_TF_X4(sp)
    sd t1, TX_RV64_TF_X6(sp)
9:
    sd t2, TX_RV64_TF_X7(sp)
    sd s0, TX_RV64_TF_X8(sp)
    sd s1, TX_RV64_TF_X9(sp)
    sd a0, TX_RV64_TF_X10(sp)
    sd a1, TX_RV64_TF_X11(sp)
    sd a2, TX_RV64_TF_X12(sp)
    sd a3, TX_RV64_TF_X13(sp)
    sd a4, TX_RV64_TF_X14(sp)
    sd a5, TX_RV64_TF_X15(sp)
    sd a6, TX_RV64_TF_X16(sp)
    sd a7, TX_RV64_TF_X17(sp)
    sd s2, TX_RV64_TF_X18(sp)
    sd s3, TX_RV64_TF_X19(sp)
    sd s4, TX_RV64_TF_X20(sp)
    sd s5, TX_RV64_TF_X21(sp)
    sd s6, TX_RV64_TF_X22(sp)
    sd s7, TX_RV64_TF_X23(sp)
    sd s8, TX_RV64_TF_X24(sp)
    sd s9, TX_RV64_TF_X25(sp)
    sd s10, TX_RV64_TF_X26(sp)
    sd s11, TX_RV64_TF_X27(sp)
    sd t3, TX_RV64_TF_X28(sp)
    sd t4, TX_RV64_TF_X29(sp)
    sd t5, TX_RV64_TF_X30(sp)
    sd t6, TX_RV64_TF_X31(sp)
    csrr t0, scause
    sd t0, TX_RV64_TF_SCAUSE(sp)
    csrr t0, sepc
    sd t0, TX_RV64_TF_SEPC(sp)
    csrr t0, stval
    sd t0, TX_RV64_TF_STVAL(sp)
    csrr t0, sstatus
    sd t0, TX_RV64_TF_SSTATUS(sp)
    # t0 = sstatus; save FP regs only once user FP state is active
    # (FS=Clean/Dirty). FS=Initial means the frame owns the zero state
    # and no user FP instruction has dirtied it yet.
    # Using t0 is safe: it was already saved to TX_RV64_TF_X5(sp) above.
    srli t0, t0, 13
    andi t0, t0, 3
    li t1, 2
    bltu t0, t1, 1f
    fsd f0,  TX_RV64_TF_F0(sp)
    fsd f1,  TX_RV64_TF_F1(sp)
    fsd f2,  TX_RV64_TF_F2(sp)
    fsd f3,  TX_RV64_TF_F3(sp)
    fsd f4,  TX_RV64_TF_F4(sp)
    fsd f5,  TX_RV64_TF_F5(sp)
    fsd f6,  TX_RV64_TF_F6(sp)
    fsd f7,  TX_RV64_TF_F7(sp)
    fsd f8,  TX_RV64_TF_F8(sp)
    fsd f9,  TX_RV64_TF_F9(sp)
    fsd f10, TX_RV64_TF_F10(sp)
    fsd f11, TX_RV64_TF_F11(sp)
    fsd f12, TX_RV64_TF_F12(sp)
    fsd f13, TX_RV64_TF_F13(sp)
    fsd f14, TX_RV64_TF_F14(sp)
    fsd f15, TX_RV64_TF_F15(sp)
    fsd f16, TX_RV64_TF_F16(sp)
    fsd f17, TX_RV64_TF_F17(sp)
    fsd f18, TX_RV64_TF_F18(sp)
    fsd f19, TX_RV64_TF_F19(sp)
    fsd f20, TX_RV64_TF_F20(sp)
    fsd f21, TX_RV64_TF_F21(sp)
    fsd f22, TX_RV64_TF_F22(sp)
    fsd f23, TX_RV64_TF_F23(sp)
    fsd f24, TX_RV64_TF_F24(sp)
    fsd f25, TX_RV64_TF_F25(sp)
    fsd f26, TX_RV64_TF_F26(sp)
    fsd f27, TX_RV64_TF_F27(sp)
    fsd f28, TX_RV64_TF_F28(sp)
    fsd f29, TX_RV64_TF_F29(sp)
    fsd f30, TX_RV64_TF_F30(sp)
    fsd f31, TX_RV64_TF_F31(sp)
    frcsr t0
    sw   t0, TX_RV64_TF_FCSR(sp)
1:

    # From-user traps arrive with gp restored from the user frame.
    # Reinstall the kernel global pointer before calling into Rust:
    # compiler/linker relaxation may address kernel statics relative
    # to gp, so running Rust trap code with a user gp corrupts global
    # accesses in wonderfully cursed ways.
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop

    # From-user traps arrive with tp restored from the user frame, so
    # recover kernel TLS from the per-hart trap-stack top. A direct
    # from-kernel trap already saved the correct kernel tp in X4; its
    # sp+frame-size is an ordinary kernel sp and must not be fed to the
    # trap-stack lookup (doing so aliases every AP to CPU0).
    ld t0, TX_RV64_TF_X0(sp)
    bnez t0, .Ltx_rv64_restore_kernel_tp
    addi a0, sp, TX_RV64_TF_SIZE
    call tx_rv64_kernel_tls_from_trap_stack_top
    mv tp, a0
    j .Ltx_rv64_kernel_tp_ready
.Ltx_rv64_restore_kernel_tp:
    ld tp, TX_RV64_TF_X4(sp)
.Ltx_rv64_kernel_tp_ready:

    mv a0, sp
    call tx_rv64_qemu_kernel_trap_entry
    # The Rust handler returns ONLY for Resume / DeliverSignal. On
    # Reschedule it longjmps via tx_rv64_resume_kernel_after_reschedule
    # (does not return); on Terminate it panics. So after this call
    # we are always on a path that wants pop+sret.

    # Epilogue: pop+sret back to trap-time mode (Resume /
    # DeliverSignal). Reschedule longjmps and never reaches here;
    # Terminate panics.
    #
    # X0 is an internal entry-path marker (real x0 is immutable):
    #   0 = from user, swap back to the saved user sp;
    #   1 = nested trap, remain on the direct trap stack;
    #   2 = ordinary kernel trap, swap back to the saved kernel sp.
    # The direct path also restores the exact pre-trap sscratch value
    # saved in TX_RV64_TF_TMP_SSCRATCH. This makes nested traps
    # compositional instead of leaking an inner interrupted sp into the
    # next unrelated interrupt.
    ld t0, TX_RV64_TF_SEPC(sp)
    csrw sepc, t0
    ld t0, TX_RV64_TF_SSTATUS(sp)
    csrw sstatus, t0
    # t0 = outgoing sstatus; restore FP regs if FS != Off. Ordinary
    # integer-only threads return with FS=Off and avoid this block;
    # the lazy-FP illegal-instruction path uses FS=Initial once to
    # publish the zero FP state before retrying the first FP insn.
    srli t0, t0, 13
    andi t0, t0, 3
    beqz t0, 2f
    lw   t0, TX_RV64_TF_FCSR(sp)
    fscsr t0
    fld f0,  TX_RV64_TF_F0(sp)
    fld f1,  TX_RV64_TF_F1(sp)
    fld f2,  TX_RV64_TF_F2(sp)
    fld f3,  TX_RV64_TF_F3(sp)
    fld f4,  TX_RV64_TF_F4(sp)
    fld f5,  TX_RV64_TF_F5(sp)
    fld f6,  TX_RV64_TF_F6(sp)
    fld f7,  TX_RV64_TF_F7(sp)
    fld f8,  TX_RV64_TF_F8(sp)
    fld f9,  TX_RV64_TF_F9(sp)
    fld f10, TX_RV64_TF_F10(sp)
    fld f11, TX_RV64_TF_F11(sp)
    fld f12, TX_RV64_TF_F12(sp)
    fld f13, TX_RV64_TF_F13(sp)
    fld f14, TX_RV64_TF_F14(sp)
    fld f15, TX_RV64_TF_F15(sp)
    fld f16, TX_RV64_TF_F16(sp)
    fld f17, TX_RV64_TF_F17(sp)
    fld f18, TX_RV64_TF_F18(sp)
    fld f19, TX_RV64_TF_F19(sp)
    fld f20, TX_RV64_TF_F20(sp)
    fld f21, TX_RV64_TF_F21(sp)
    fld f22, TX_RV64_TF_F22(sp)
    fld f23, TX_RV64_TF_F23(sp)
    fld f24, TX_RV64_TF_F24(sp)
    fld f25, TX_RV64_TF_F25(sp)
    fld f26, TX_RV64_TF_F26(sp)
    fld f27, TX_RV64_TF_F27(sp)
    fld f28, TX_RV64_TF_F28(sp)
    fld f29, TX_RV64_TF_F29(sp)
    fld f30, TX_RV64_TF_F30(sp)
    fld f31, TX_RV64_TF_F31(sp)
2:

    ld ra, TX_RV64_TF_X1(sp)
    ld gp, TX_RV64_TF_X3(sp)
    ld tp, TX_RV64_TF_X4(sp)
    ld t1, TX_RV64_TF_X6(sp)
    ld t2, TX_RV64_TF_X7(sp)
    ld s0, TX_RV64_TF_X8(sp)
    ld s1, TX_RV64_TF_X9(sp)
    ld a0, TX_RV64_TF_X10(sp)
    ld a1, TX_RV64_TF_X11(sp)
    ld a2, TX_RV64_TF_X12(sp)
    ld a3, TX_RV64_TF_X13(sp)
    ld a4, TX_RV64_TF_X14(sp)
    ld a5, TX_RV64_TF_X15(sp)
    ld a6, TX_RV64_TF_X16(sp)
    ld a7, TX_RV64_TF_X17(sp)
    ld s2, TX_RV64_TF_X18(sp)
    ld s3, TX_RV64_TF_X19(sp)
    ld s4, TX_RV64_TF_X20(sp)
    ld s5, TX_RV64_TF_X21(sp)
    ld s6, TX_RV64_TF_X22(sp)
    ld s7, TX_RV64_TF_X23(sp)
    ld s8, TX_RV64_TF_X24(sp)
    ld s9, TX_RV64_TF_X25(sp)
    ld s10, TX_RV64_TF_X26(sp)
    ld s11, TX_RV64_TF_X27(sp)
    ld t3, TX_RV64_TF_X28(sp)
    ld t4, TX_RV64_TF_X29(sp)
    ld t5, TX_RV64_TF_X30(sp)
    ld t6, TX_RV64_TF_X31(sp)

    # Keep t0 available until the path split; all other GPRs now carry
    # their trap-time values.
    ld t0, TX_RV64_TF_X0(sp)
    addi t0, t0, -1
    beqz t0, 4f

    # From user or an ordinary kernel stack: stash the saved trap-time
    # sp in sscratch, restore t0, then atomically return to that stack
    # while re-priming sscratch with trap_stack_top.
    ld t0, TX_RV64_TF_X2(sp)
    csrw sscratch, t0
    ld t0, TX_RV64_TF_X5(sp)
    addi sp, sp, TX_RV64_TF_SIZE
    csrrw sp, sscratch, sp
    sret

4:
    # Nested trap: restore the pre-trap sscratch exactly and pop the
    # direct-stack frame without a stack swap.
    ld t0, TX_RV64_TF_TMP_SSCRATCH(sp)
    csrw sscratch, t0
    ld t0, TX_RV64_TF_X5(sp)
    addi sp, sp, TX_RV64_TF_SIZE
    sret

    # ------------------------------------------------------------------
    # tx_rv64_enter_userspace_save_resume:
    #   a0 = *mut KernelResumeCtx       (per-hart save area)
    #   a1 = *const Rv64TrapFrame       (user trap frame to load)
    #   a2 = trap_stack_top             (boot-primed sscratch value)
    #
    # Stash (sp, ra, s0..s11) into *a0 so the trap-shell longjmp
    # helper can unwind back here. Then re-prime sscratch with a2
    # so the upcoming user trap lands on the trap stack. Then load
    # the user frame from a1 and sret.
    #
    # `noreturn` from rustc's POV; control returns via
    # tx_rv64_resume_kernel_after_reschedule, which `ret`s to the
    # ra saved in *a0.
    # ------------------------------------------------------------------
    .globl tx_rv64_enter_userspace_save_resume
    .type tx_rv64_enter_userspace_save_resume, @function
tx_rv64_enter_userspace_save_resume:
    sd sp,   TX_RV64_RCTX_SP(a0)
    sd ra,   TX_RV64_RCTX_RA(a0)
    sd s0,  (TX_RV64_RCTX_S0 +   0)(a0)
    sd s1,  (TX_RV64_RCTX_S0 +   8)(a0)
    sd s2,  (TX_RV64_RCTX_S0 +  16)(a0)
    sd s3,  (TX_RV64_RCTX_S0 +  24)(a0)
    sd s4,  (TX_RV64_RCTX_S0 +  32)(a0)
    sd s5,  (TX_RV64_RCTX_S0 +  40)(a0)
    sd s6,  (TX_RV64_RCTX_S0 +  48)(a0)
    sd s7,  (TX_RV64_RCTX_S0 +  56)(a0)
    sd s8,  (TX_RV64_RCTX_S0 +  64)(a0)
    sd s9,  (TX_RV64_RCTX_S0 +  72)(a0)
    sd s10, (TX_RV64_RCTX_S0 +  80)(a0)
    sd s11, (TX_RV64_RCTX_S0 +  88)(a0)

    # Re-prime sscratch with trap_stack_top so the next user trap
    # lands on the trap stack.
    csrw sscratch, a2

    # Load user frame from a1 and sret. Mirrors `return_to_userspace`.
    mv t6, a1
    ld t0, TX_RV64_TF_SEPC(t6)
    csrw sepc, t0
    ld t0, TX_RV64_TF_SSTATUS(t6)
    csrw sstatus, t0
    # t0 = outgoing sstatus; restore FP regs if FS != Off.
    srli t0, t0, 13
    andi t0, t0, 3
    beqz t0, 3f
    lw   t0, TX_RV64_TF_FCSR(t6)
    fscsr t0
    fld f0,  TX_RV64_TF_F0(t6)
    fld f1,  TX_RV64_TF_F1(t6)
    fld f2,  TX_RV64_TF_F2(t6)
    fld f3,  TX_RV64_TF_F3(t6)
    fld f4,  TX_RV64_TF_F4(t6)
    fld f5,  TX_RV64_TF_F5(t6)
    fld f6,  TX_RV64_TF_F6(t6)
    fld f7,  TX_RV64_TF_F7(t6)
    fld f8,  TX_RV64_TF_F8(t6)
    fld f9,  TX_RV64_TF_F9(t6)
    fld f10, TX_RV64_TF_F10(t6)
    fld f11, TX_RV64_TF_F11(t6)
    fld f12, TX_RV64_TF_F12(t6)
    fld f13, TX_RV64_TF_F13(t6)
    fld f14, TX_RV64_TF_F14(t6)
    fld f15, TX_RV64_TF_F15(t6)
    fld f16, TX_RV64_TF_F16(t6)
    fld f17, TX_RV64_TF_F17(t6)
    fld f18, TX_RV64_TF_F18(t6)
    fld f19, TX_RV64_TF_F19(t6)
    fld f20, TX_RV64_TF_F20(t6)
    fld f21, TX_RV64_TF_F21(t6)
    fld f22, TX_RV64_TF_F22(t6)
    fld f23, TX_RV64_TF_F23(t6)
    fld f24, TX_RV64_TF_F24(t6)
    fld f25, TX_RV64_TF_F25(t6)
    fld f26, TX_RV64_TF_F26(t6)
    fld f27, TX_RV64_TF_F27(t6)
    fld f28, TX_RV64_TF_F28(t6)
    fld f29, TX_RV64_TF_F29(t6)
    fld f30, TX_RV64_TF_F30(t6)
    fld f31, TX_RV64_TF_F31(t6)
3:
    ld ra,  TX_RV64_TF_X1(t6)
    ld gp,  TX_RV64_TF_X3(t6)
    ld tp,  TX_RV64_TF_X4(t6)
    ld t0,  TX_RV64_TF_X5(t6)
    ld t1,  TX_RV64_TF_X6(t6)
    ld t2,  TX_RV64_TF_X7(t6)
    ld s0,  TX_RV64_TF_X8(t6)
    ld s1,  TX_RV64_TF_X9(t6)
    ld a0,  TX_RV64_TF_X10(t6)
    ld a1,  TX_RV64_TF_X11(t6)
    ld a2,  TX_RV64_TF_X12(t6)
    ld a3,  TX_RV64_TF_X13(t6)
    ld a4,  TX_RV64_TF_X14(t6)
    ld a5,  TX_RV64_TF_X15(t6)
    ld a6,  TX_RV64_TF_X16(t6)
    ld a7,  TX_RV64_TF_X17(t6)
    ld s2,  TX_RV64_TF_X18(t6)
    ld s3,  TX_RV64_TF_X19(t6)
    ld s4,  TX_RV64_TF_X20(t6)
    ld s5,  TX_RV64_TF_X21(t6)
    ld s6,  TX_RV64_TF_X22(t6)
    ld s7,  TX_RV64_TF_X23(t6)
    ld s8,  TX_RV64_TF_X24(t6)
    ld s9,  TX_RV64_TF_X25(t6)
    ld s10, TX_RV64_TF_X26(t6)
    ld s11, TX_RV64_TF_X27(t6)
    ld t3,  TX_RV64_TF_X28(t6)
    ld t4,  TX_RV64_TF_X29(t6)
    ld t5,  TX_RV64_TF_X30(t6)
    ld sp,  TX_RV64_TF_X2(t6)
    ld t6,  TX_RV64_TF_X31(t6)
    sret
    .size tx_rv64_enter_userspace_save_resume, . - tx_rv64_enter_userspace_save_resume

    # ------------------------------------------------------------------
    # tx_rv64_resume_kernel_after_reschedule:
    #   a0 = *const KernelResumeCtx
    #
    # Restore (sp, ra, s0..s11) from *a0 and `ret`. The caller in
    # apply_trap_action has already re-primed sscratch with
    # trap_stack_top.
    # ------------------------------------------------------------------
    .globl tx_rv64_resume_kernel_after_reschedule
    .type tx_rv64_resume_kernel_after_reschedule, @function
tx_rv64_resume_kernel_after_reschedule:
    .option push
    .option norelax
    la gp, __global_pointer$
    .option pop
    ld sp,   TX_RV64_RCTX_SP(a0)
    ld ra,   TX_RV64_RCTX_RA(a0)
    ld s0,  (TX_RV64_RCTX_S0 +   0)(a0)
    ld s1,  (TX_RV64_RCTX_S0 +   8)(a0)
    ld s2,  (TX_RV64_RCTX_S0 +  16)(a0)
    ld s3,  (TX_RV64_RCTX_S0 +  24)(a0)
    ld s4,  (TX_RV64_RCTX_S0 +  32)(a0)
    ld s5,  (TX_RV64_RCTX_S0 +  40)(a0)
    ld s6,  (TX_RV64_RCTX_S0 +  48)(a0)
    ld s7,  (TX_RV64_RCTX_S0 +  56)(a0)
    ld s8,  (TX_RV64_RCTX_S0 +  64)(a0)
    ld s9,  (TX_RV64_RCTX_S0 +  72)(a0)
    ld s10, (TX_RV64_RCTX_S0 +  80)(a0)
    ld s11, (TX_RV64_RCTX_S0 +  88)(a0)
    ret
    .size tx_rv64_resume_kernel_after_reschedule, . - tx_rv64_resume_kernel_after_reschedule
    .option pop
"#
);

const RV64_SSTATUS_SPP: usize = 1 << 8;
const RV64_SSTATUS_SPIE: usize = 1 << 5;
/// FS field (bits 14:13): 00=Off 01=Initial 10=Clean 11=Dirty.
/// Must be non-zero before sret so user-space FP/Zd instructions
/// don't trap with Illegal Instruction (scause=2).
const RV64_SSTATUS_FS_MASK: usize = 3 << 13;
const RV64_SSTATUS_FS_OFF: usize = 0 << 13;
const RV64_SSTATUS_FS_INITIAL: usize = 1 << 13;
const RV64_SSTATUS_FS_DIRTY: usize = 3 << 13;
const X_SP: usize = 2;
const X_RA: usize = 1;
const X_TP: usize = 4;
const X_A0: usize = 10;
const X_A1: usize = 11;
const X_A2: usize = 12;
const X_A3: usize = 13;
const X_A4: usize = 14;
const X_A5: usize = 15;
const X_A7: usize = 17;

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rv64TrapFrame {
    pub x: [usize; 32], // offsets   0..255
    pub scause: usize,  // offset  256
    pub sepc: usize,    // offset  264
    pub stval: usize,   // offset  272
    pub sstatus: usize, // offset  280
    pub f: [u64; 32],   // offsets 288..543  (TX_RV64_TF_F_BASE)
    pub fcsr: u32,      // offset  544       (TX_RV64_TF_FCSR)
    pub _pad_fp: u32,   // offset  548       (pad FP payload to 8 bytes)
                        // payload = 552 bytes; repr(align(16)) rounds the
                        // complete frame to 560 bytes (TX_RV64_TF_SIZE)
}

const _: [(); 560] = [(); core::mem::size_of::<Rv64TrapFrame>()];
const _: [(); 16] = [(); core::mem::align_of::<Rv64TrapFrame>()];

impl Rv64TrapFrame {
    pub const fn snapshot(&self) -> TrapFrameSnapshot {
        TrapFrameSnapshot {
            scause: self.scause,
            sepc: self.sepc,
            stval: self.stval,
        }
    }

    pub const fn previous_mode(&self) -> TrapPreviousMode {
        if self.sstatus & RV64_SSTATUS_SPP == 0 {
            TrapPreviousMode::User
        } else {
            TrapPreviousMode::Supervisor
        }
    }

    pub const fn fault_address(&self) -> Option<VirtAddr> {
        match classify_rv64_trap(self.scause) {
            TrapClass::PageFault { .. } | TrapClass::AlignmentFault { .. } => {
                Some(VirtAddr(self.stval))
            }
            _ => None,
        }
    }

    pub const fn faulting_instruction(&self) -> Option<VirtAddr> {
        if rv64_scause_is_interrupt(self.scause) {
            None
        } else {
            Some(VirtAddr(self.sepc))
        }
    }

    pub const fn interrupts_enabled_before(&self) -> bool {
        self.sstatus & RV64_SSTATUS_SPIE != 0
    }

    pub const fn view(&self) -> TrapFrameView {
        TrapFrameView::new(
            VirtAddr(self.sepc),
            VirtAddr(self.x[X_SP]),
            self.x[X_A7] as u64,
            [
                self.x[X_A0] as u64,
                self.x[X_A1] as u64,
                self.x[X_A2] as u64,
                self.x[X_A3] as u64,
                self.x[X_A4] as u64,
                self.x[X_A5] as u64,
            ],
            self.fault_address(),
            self.faulting_instruction(),
            self.previous_mode(),
            self.interrupts_enabled_before(),
            self.x[X_TP] as u64,
        )
    }

    pub fn view_mut(&mut self) -> TrapFrameMut<'_> {
        let view = TrapFrameView::new(
            VirtAddr(self.sepc),
            VirtAddr(self.x[X_SP]),
            self.x[X_A7] as u64,
            [
                self.x[X_A0] as u64,
                self.x[X_A1] as u64,
                self.x[X_A2] as u64,
                self.x[X_A3] as u64,
                self.x[X_A4] as u64,
                self.x[X_A5] as u64,
            ],
            self.fault_address(),
            self.faulting_instruction(),
            self.previous_mode(),
            self.interrupts_enabled_before(),
            self.x[X_TP] as u64,
        );
        let raw = NonNull::from(&mut *self).cast::<()>();
        unsafe { TrapFrameMut::from_raw_parts(view, raw, &RV64_TRAP_FRAME_MUT_VTABLE) }
    }

    fn set_pc(&mut self, pc: VirtAddr) {
        self.sepc = pc.0;
    }

    fn set_sp(&mut self, sp: VirtAddr) {
        self.x[X_SP] = sp.0;
    }

    fn set_syscall_return(&mut self, value: i64) {
        self.x[X_A0] = value as usize;
    }

    fn set_syscall_error(&mut self, errno: i32) {
        self.x[X_A0] = (-(errno as isize)) as usize;
    }

    fn set_user_tls_register(&mut self, value: u64) {
        self.x[X_TP] = value as usize;
    }

    fn capture_user_context(&self) -> UserTrapContext {
        let fs = (self.sstatus >> 13) & 3;
        let fp = if fs >= 2 {
            let mut flags = UserFpContext::FLAG_VALID;
            if fs == 3 {
                flags |= UserFpContext::FLAG_DIRTY;
            }
            UserFpContext {
                regs: self.f,
                fcsr: self.fcsr,
                flags,
                ..UserFpContext::empty()
            }
        } else {
            UserFpContext::empty()
        };
        UserTrapContext {
            regs: self.x,
            pc: self.sepc,
            status: self.sstatus,
            fp,
        }
    }

    fn restore_user_context(&mut self, context: &UserTrapContext) {
        self.x = context.regs;
        self.x[0] = 0;
        self.sepc = context.pc;
        self.sstatus = context.status;
        if context.fp.is_valid() {
            self.f = context.fp.regs;
            self.fcsr = context.fp.fcsr;
        } else {
            self.f = [0u64; 32];
            self.fcsr = 0;
        }
        self.prepare_user_return_with_fp_state(context.fp.is_valid());
    }

    fn set_signal_handler_regs(&mut self, regs: SignalHandlerRegs) {
        self.x[X_RA] = regs.return_pc.0;
        self.x[X_A0] = regs.args[0];
        self.x[X_A1] = regs.args[1];
        self.x[X_A2] = regs.args[2];
    }

    fn rewind_pc(&mut self, bytes: usize) {
        self.sepc = self.sepc.saturating_sub(bytes);
    }

    pub fn prepare_user_return(&mut self) {
        self.prepare_user_return_with_fp_state(false);
    }

    fn prepare_user_return_with_fp_state(&mut self, fp_valid: bool) {
        self.sstatus &= !RV64_SSTATUS_SPP;
        self.sstatus |= RV64_SSTATUS_SPIE;
        let fs = if fp_valid {
            RV64_SSTATUS_FS_DIRTY
        } else {
            RV64_SSTATUS_FS_OFF
        };
        self.sstatus = (self.sstatus & !RV64_SSTATUS_FS_MASK) | fs;
    }

    fn enable_initial_user_fp_state(&mut self) {
        self.f = [0u64; 32];
        self.fcsr = 0;
        self.sstatus = (self.sstatus & !RV64_SSTATUS_FS_MASK) | RV64_SSTATUS_FS_INITIAL;
    }
}

static RV64_TRAP_FRAME_MUT_VTABLE: TrapFrameMutVtable = TrapFrameMutVtable {
    read_view: rv64_read_view,
    set_pc: rv64_set_pc,
    set_sp: rv64_set_sp,
    set_syscall_return: rv64_set_syscall_return,
    set_syscall_error: rv64_set_syscall_error,
    set_user_tls_register: rv64_set_user_tls_register,
    capture_user_context: rv64_capture_user_context,
    restore_user_context: rv64_restore_user_context,
    set_signal_handler_regs: rv64_set_signal_handler_regs,
    rewind_pc: rv64_rewind_pc,
};

fn rv64_frame_ptr(raw: NonNull<()>) -> *mut Rv64TrapFrame {
    raw.cast::<Rv64TrapFrame>().as_ptr()
}

fn rv64_read_view(raw: NonNull<()>) -> TrapFrameView {
    unsafe { (*rv64_frame_ptr(raw)).view() }
}

fn rv64_set_pc(raw: NonNull<()>, pc: VirtAddr) {
    unsafe { (*rv64_frame_ptr(raw)).set_pc(pc) };
}

fn rv64_set_sp(raw: NonNull<()>, sp: VirtAddr) {
    unsafe { (*rv64_frame_ptr(raw)).set_sp(sp) };
}

fn rv64_set_syscall_return(raw: NonNull<()>, value: i64) {
    unsafe { (*rv64_frame_ptr(raw)).set_syscall_return(value) };
}

fn rv64_set_syscall_error(raw: NonNull<()>, errno: i32) {
    unsafe { (*rv64_frame_ptr(raw)).set_syscall_error(errno) };
}

fn rv64_set_user_tls_register(raw: NonNull<()>, value: u64) {
    unsafe { (*rv64_frame_ptr(raw)).set_user_tls_register(value) };
}

fn rv64_capture_user_context(raw: NonNull<()>) -> UserTrapContext {
    unsafe { (*rv64_frame_ptr(raw)).capture_user_context() }
}

fn rv64_restore_user_context(raw: NonNull<()>, context: &UserTrapContext) {
    unsafe { (*rv64_frame_ptr(raw)).restore_user_context(context) };
}

fn rv64_set_signal_handler_regs(raw: NonNull<()>, regs: SignalHandlerRegs) {
    unsafe { (*rv64_frame_ptr(raw)).set_signal_handler_regs(regs) };
}

fn rv64_rewind_pc(raw: NonNull<()>, bytes: usize) {
    unsafe { (*rv64_frame_ptr(raw)).rewind_pc(bytes) };
}

impl TrapIf for Platform {
    fn install_minimal_trap_vector() {
        install_rv64_trap_vector();
    }

    fn install_kernel_trap_vector() {
        install_rv64_trap_vector();
    }

    fn install_user_trap_vector() {
        install_rv64_trap_vector();
    }

    fn classify_trap(snapshot: TrapFrameSnapshot) -> TrapClass {
        classify_rv64_trap(snapshot.scause)
    }

    /// RV64 implementation of the portable userspace-entry hook.
    ///
    /// Materialises a fresh `Rv64TrapFrame` from `ctx`, prepares it
    /// for the user-mode `sret`, and hands it to the existing
    /// `return_to_userspace` low-level primitive. This is the second
    /// site of the two-site discipline pinned by
    /// `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`
    /// (`docs/design/02_execution/THREAD_RUNTIME_v1.md`): the
    /// userspace-entry shim that produces the `UserTrapContext`
    /// (in `tx_subsystems::thread_runtime::execution::
    /// prepare_userspace_entry_payload`) merges any pending syscall
    /// return into the context's `a0` slot before this call; the
    /// platform's writeback is the `restore_user_context` shape
    /// already used by `Rv64TrapFrame::restore_user_context`.
    ///
    /// Materialises a fresh `Rv64TrapFrame` from `ctx`, stashes the
    /// kernel-side caller's `(sp, ra, callee-saved s-regs)` into
    /// the per-hart [`KernelResumeCtx`], re-primes `sscratch` with
    /// the per-CPU trap-stack top, and `sret`s into user mode.
    /// Returns when the trap shell chooses `TrapAction::Reschedule`
    /// and longjmps back via [`tx_rv64_resume_kernel_after_reschedule`].
    fn enter_userspace_with_context(ctx: &UserTrapContext, root: &PmapRoot) {
        assert_eq!(
            <Platform as PercpuIf>::cpu_pin_depth(),
            0,
            "RV64 CPU pin escaped across a reactor/userspace boundary"
        );
        crate::debug_trace::record_entry(ctx);

        let mut frame = Rv64TrapFrame {
            x: [0; 32],
            scause: 0,
            sepc: 0,
            stval: 0,
            sstatus: 0,
            f: [0u64; 32],
            fcsr: 0,
            _pad_fp: 0,
        };
        frame.restore_user_context(ctx);
        // `restore_user_context` already calls `prepare_user_return`,
        // which clears SPP and sets SPIE so `sret` lands in user mode
        // with interrupts enabled.
        <Platform as PmapIf>::activate_user_pmap(root);

        #[cfg(target_arch = "riscv64")]
        unsafe {
            let cpu = <Platform as SmpIf>::current_cpu_id();
            let resume_ctx = current_kernel_resume_ctx_ptr();
            let stack_top = trap_stack_top_for_cpu(cpu);
            tx_rv64_enter_userspace_save_resume(resume_ctx, &frame, stack_top);
        }
        #[cfg(not(target_arch = "riscv64"))]
        {
            // Host build: no userspace to enter; fall through to
            // return_to_userspace which spins per the host stub.
            unsafe { return_to_userspace(&frame) }
        }
    }
}

pub fn dispatch_trap_frame<K>(frame: &mut Rv64TrapFrame) -> TrapAction
where
    K: KernelTrapSink<Platform>,
{
    let class = classify_rv64_trap(frame.scause);
    let from_user = frame.previous_mode() == TrapPreviousMode::User;
    if from_user {
        crate::debug_trace::record_trap((frame.scause & 0xff) as u8, frame);
    }

    match class {
        TrapClass::PageFault { write, instruction } => {
            if !from_user {
                if let Some(recovery_pc) = user_access::fixup_lookup(frame.sepc) {
                    frame.sepc = recovery_pc;
                    frame.x[X_A0] = frame.stval;
                    return TrapAction::Resume;
                }
            }

            let fault = FaultInfo {
                address: VirtAddr(frame.stval),
                write,
                instruction,
                from_user,
            };
            K::on_page_fault(frame.view_mut(), fault)
        }
        TrapClass::Syscall => K::on_syscall(frame.view_mut()),
        TrapClass::TimerInterrupt => {
            let _irq_context = crate::enter_irq_context();
            K::on_timer_interrupt(
                <Platform as tx_hal::SmpIf>::current_cpu_id(),
                frame.view_mut(),
            )
        }
        TrapClass::ExternalInterrupt => {
            let _irq_context = crate::enter_irq_context();
            K::on_external_irq(
                <Platform as tx_hal::SmpIf>::current_cpu_id(),
                frame.view_mut(),
            )
        }
        TrapClass::InterprocessorInterrupt => {
            let _irq_context = crate::enter_irq_context();
            // Clear the hardware SSIP latch before consulting the software
            // pending bitmap. This makes stale/spurious software interrupts
            // one-shot instead of trapping forever when no IPI kind is
            // recorded for the current hart. A concurrently sent IPI will
            // set the latch again after publishing its pending bit.
            crate::clear_supervisor_software_interrupt();
            K::on_ipi(
                <Platform as tx_hal::SmpIf>::current_cpu_id(),
                frame.view_mut(),
            )
        }
        TrapClass::IllegalInstruction => {
            if from_user && try_enable_lazy_user_fp(frame) {
                return TrapAction::Resume;
            }
            let fault = FaultInfo {
                address: VirtAddr(frame.sepc),
                write: false,
                instruction: true,
                from_user,
            };
            K::on_illegal_or_sync_fault(frame.view_mut(), fault)
        }
        TrapClass::Breakpoint
        | TrapClass::AlignmentFault { .. }
        | TrapClass::UnknownSync
        | TrapClass::UnknownInterrupt => {
            let fault = FaultInfo {
                address: VirtAddr(frame.sepc),
                write: false,
                instruction: true,
                from_user,
            };
            K::on_illegal_or_sync_fault(frame.view_mut(), fault)
        }
    }
}

fn try_enable_lazy_user_fp(frame: &mut Rv64TrapFrame) -> bool {
    if frame.sstatus & RV64_SSTATUS_FS_MASK != RV64_SSTATUS_FS_OFF {
        return false;
    }
    frame.enable_initial_user_fp_state();
    true
}

pub(crate) const fn classify_rv64_trap(scause: usize) -> TrapClass {
    let is_interrupt = rv64_scause_is_interrupt(scause);
    let code = scause & !rv64_scause_interrupt_bit();

    match (is_interrupt, code) {
        (false, 0) => TrapClass::AlignmentFault {
            write: false,
            instruction: true,
        },
        (false, 2) => TrapClass::IllegalInstruction,
        (false, 3) => TrapClass::Breakpoint,
        (false, 4) => TrapClass::AlignmentFault {
            write: false,
            instruction: false,
        },
        (false, 6) => TrapClass::AlignmentFault {
            write: true,
            instruction: false,
        },
        (false, 8) => TrapClass::Syscall,
        (false, 12) => TrapClass::PageFault {
            write: false,
            instruction: true,
        },
        (false, 13) => TrapClass::PageFault {
            write: false,
            instruction: false,
        },
        (false, 15) => TrapClass::PageFault {
            write: true,
            instruction: false,
        },
        (true, 1) => TrapClass::InterprocessorInterrupt,
        (true, 5) => TrapClass::TimerInterrupt,
        (true, 9) => TrapClass::ExternalInterrupt,
        (false, _) => TrapClass::UnknownSync,
        (true, _) => TrapClass::UnknownInterrupt,
    }
}

const fn rv64_scause_interrupt_bit() -> usize {
    1usize << (usize::BITS as usize - 1)
}

const fn rv64_scause_is_interrupt(scause: usize) -> bool {
    scause & rv64_scause_interrupt_bit() != 0
}

fn install_rv64_trap_vector() {
    let vector = boot_static::current_trap_vector_kernel_alias();

    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!(
            "csrw stvec, {vector}",
            vector = in(reg) vector.0,
            options(nostack)
        );
    }

    #[cfg(not(target_arch = "riscv64"))]
    let _ = vector;
}

#[cfg(target_arch = "riscv64")]
extern "C" {
    fn tx_kernel_riscv64_qemu_trap_dispatch(frame: *mut Rv64TrapFrame) -> TrapAction;

    /// Stash kernel-side `(sp, ra, s0..s11)` into `*resume_ctx`,
    /// re-prime sscratch with `trap_stack_top`, load the user
    /// frame from `*frame`, and `sret`. Returns only via the
    /// reschedule longjmp helper.
    fn tx_rv64_enter_userspace_save_resume(
        resume_ctx: *mut KernelResumeCtx,
        frame: *const Rv64TrapFrame,
        trap_stack_top: usize,
    );

    /// Restore `(sp, ra, s0..s11)` from `*resume_ctx` and `ret`.
    /// Caller must have already re-primed sscratch.
    fn tx_rv64_resume_kernel_after_reschedule(resume_ctx: *const KernelResumeCtx) -> !;
}

#[cfg(target_arch = "riscv64")]
#[no_mangle]
extern "C" fn tx_rv64_qemu_kernel_trap_entry(frame: &mut Rv64TrapFrame) {
    let action = unsafe { tx_kernel_riscv64_qemu_trap_dispatch(frame) };
    apply_trap_action(frame, action);
}

#[cfg(target_arch = "riscv64")]
fn apply_trap_action(frame: &Rv64TrapFrame, action: TrapAction) {
    let from_user = frame.previous_mode() == TrapPreviousMode::User;
    match action {
        // Resume / DeliverSignal: fall through to the trap-vector
        // epilogue, which pop+sret's back to the trap-time mode.
        // The asm prologue's sscratch swap is reversed in the
        // epilogue's final `csrrw sp, sscratch, sp` which also
        // re-primes sscratch with trap_stack_top.
        TrapAction::Resume | TrapAction::DeliverSignal => {}

        // Reschedule: longjmp back to the kernel-side caller of
        // `enter_userspace_with_context`. Re-prime sscratch first
        // (the longjmp doesn't go through the trap-vector epilogue,
        // so sscratch would otherwise still hold the user's sp from
        // the entry-side swap, leaving subsequent traps to land on
        // the user stack again). Then call the asm helper which
        // restores (sp, ra, s-regs) from the per-hart KernelResumeCtx
        // and `ret`s — control unwinds back through
        // `enter_userspace_with_context` and into the future's
        // `run_thread` body.
        //
        // **Only valid for from-user traps.** The KernelResumeCtx is
        // written exclusively by `enter_userspace_with_context`'s
        // asm helper. After the most recent userspace round-trip
        // unwinds back through the longjmp, the resume context
        // still holds the (sp, ra, s-regs) snapshot from that
        // entry — re-using it for an unrelated kernel-mode trap
        // (e.g. an IRQ that fires while the BSP loop is in WFI)
        // would time-warp execution back into a stale frame.
        // For from-kernel traps that ask for Reschedule (typically
        // an IRQ that woke another task), fall through to the
        // pop+sret epilogue — the woken task will be picked up on
        // the next reactor poll without a longjmp.
        TrapAction::Reschedule => {
            if from_user {
                let cpu = <Platform as SmpIf>::current_cpu_id();
                let stack_top = trap_stack_top_for_cpu(cpu);
                crate::deactivate_current_user_pmap();
                unsafe {
                    core::arch::asm!("csrw sscratch, {top}", top = in(reg) stack_top);
                    let ctx = current_kernel_resume_ctx_ptr();
                    tx_rv64_resume_kernel_after_reschedule(ctx);
                }
                // Asm helper diverges; this path is unreachable.
            }
            // From-kernel: no-op; trap-vector epilogue sret's back
            // to S-mode at the trap-time PC.
        }

        TrapAction::Terminate => tx_rv64_qemu_trap_panic(frame),
    }
}

#[cfg(target_arch = "riscv64")]
/// Restore a prepared RV64 trap frame and enter user mode with `sret`.
///
/// # Safety
///
/// `frame` must describe a valid user context for the currently installed page
/// table, including a user PC, user SP, user-mode `sstatus` bits prepared by
/// `Rv64TrapFrame::prepare_user_return`, and registers that are safe to expose
/// to user mode. This function does not return and does not validate that user
/// memory, VM state, signal state, or ThreadRuntime ownership are coherent.
pub unsafe fn return_to_userspace(frame: &Rv64TrapFrame) -> ! {
    unsafe {
        core::arch::asm!(
            "ld t0, 264(t6)",
            "csrw sepc, t0",
            "ld t0, 280(t6)",
            "csrw sstatus, t0",
            "ld ra, 8(t6)",
            "ld gp, 24(t6)",
            "ld tp, 32(t6)",
            "ld t0, 40(t6)",
            "ld t1, 48(t6)",
            "ld t2, 56(t6)",
            "ld s0, 64(t6)",
            "ld s1, 72(t6)",
            "ld a0, 80(t6)",
            "ld a1, 88(t6)",
            "ld a2, 96(t6)",
            "ld a3, 104(t6)",
            "ld a4, 112(t6)",
            "ld a5, 120(t6)",
            "ld a6, 128(t6)",
            "ld a7, 136(t6)",
            "ld s2, 144(t6)",
            "ld s3, 152(t6)",
            "ld s4, 160(t6)",
            "ld s5, 168(t6)",
            "ld s6, 176(t6)",
            "ld s7, 184(t6)",
            "ld s8, 192(t6)",
            "ld s9, 200(t6)",
            "ld s10, 208(t6)",
            "ld s11, 216(t6)",
            "ld t3, 224(t6)",
            "ld t4, 232(t6)",
            "ld t5, 240(t6)",
            "ld sp, 16(t6)",
            "ld t6, 248(t6)",
            "sret",
            in("t6") frame as *const Rv64TrapFrame,
            options(noreturn)
        )
    }
}

#[cfg(not(target_arch = "riscv64"))]
/// Host-build placeholder for the RV64 user-mode restore primitive.
///
/// # Safety
///
/// This function never returns and exists only so host builds typecheck the
/// platform surface. It does not enter user mode.
pub unsafe fn return_to_userspace(_frame: &Rv64TrapFrame) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "riscv64")]
fn tx_rv64_qemu_trap_panic(frame: &Rv64TrapFrame) -> ! {
    console_write_literal(b"txkernel:qemu-riscv64-virt:trap\nreason=trap-action-terminate\n");
    console_write_trap_summary(frame);
    console_write_trapframe(frame);
    console_write_fp_chain(frame.x[8]);
    console_write_stack_dump(frame.x[2], 32);

    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "riscv64")]
fn console_write_fp_chain(initial_fp: usize) {
    // Kernel canonical addresses in SV39 have bits [63:39] all set.
    const KERNEL_ADDR_MIN: usize = 0xffff_0000_0000_0000;

    let mut fp = initial_fp;
    if fp == 0 || fp < KERNEL_ADDR_MIN || fp & 7 != 0 {
        return;
    }

    console_write_literal(b"fp chain:\n");

    for _ in 0..64 {
        let ra_ptr = fp.wrapping_sub(8);
        let fp_ptr = fp.wrapping_sub(16);
        if ra_ptr < KERNEL_ADDR_MIN || fp_ptr < KERNEL_ADDR_MIN {
            break;
        }
        // SAFETY: both addresses are kernel-canonical, aligned, and within
        // the frame we received from the trap handler's own stack walk. Any
        // fault here would be a kernel bug, not a user-triggerable path.
        let saved_ra = unsafe { *(ra_ptr as *const usize) };
        let saved_fp = unsafe { *(fp_ptr as *const usize) };

        console_write_literal(b"  fp=0x");
        console_write_hex(fp);
        console_write_literal(b" ra=0x");
        console_write_hex(saved_ra);
        console_write_literal(b"\n");

        // Stack grows down: next fp must be strictly higher and aligned.
        if saved_fp == 0 || saved_fp <= fp || saved_fp < KERNEL_ADDR_MIN || saved_fp & 7 != 0 {
            break;
        }
        fp = saved_fp;
    }
}

/// Emit a raw dump of up to `word_count` 8-byte words starting at `sp`.
///
/// Format (parsed by `xtask fault-decode`):
/// ```text
/// stack dump: sp=0x{addr}
///   0x{addr}: 0x{w0} 0x{w1} 0x{w2} 0x{w3}
///   ...
/// ```
#[cfg(target_arch = "riscv64")]
fn console_write_stack_dump(sp: usize, word_count: usize) {
    const KERNEL_ADDR_MIN: usize = 0xffff_0000_0000_0000;
    if sp == 0 || sp < KERNEL_ADDR_MIN || sp & 7 != 0 || word_count == 0 {
        return;
    }
    console_write_literal(b"stack dump: sp=0x");
    console_write_hex(sp);
    console_write_literal(b"\n");

    let mut i = 0usize;
    while i < word_count {
        let addr = sp.wrapping_add(i * core::mem::size_of::<usize>());
        if addr < KERNEL_ADDR_MIN {
            break;
        }
        let row_words = (word_count - i).min(4);
        console_write_literal(b"  0x");
        console_write_hex(addr);
        console_write_literal(b":");
        for j in 0..row_words {
            let word_addr = addr.wrapping_add(j * core::mem::size_of::<usize>());
            if word_addr < KERNEL_ADDR_MIN {
                break;
            }
            // SAFETY: word_addr is kernel-canonical, 8-byte aligned, and
            // within the live kernel stack at the time of the trap.
            let word = unsafe { *(word_addr as *const usize) };
            console_write_literal(b" 0x");
            console_write_hex(word);
        }
        console_write_literal(b"\n");
        i += row_words;
    }
}

/// Emit a synthetic scause/sepc/stval summary line and fp-chain for a
/// software panic, formatted identically to a hardware trap so that
/// `xtask fault-decode` can analyze the panic site.
///
/// `ra` is the return address captured at the panic call site (sepc).
/// `fp` is the frame pointer at that point; the fp-chain walk starts from it.
#[cfg(target_arch = "riscv64")]
pub fn emit_panic_location(fp: usize, ra: usize) {
    // scause=3 (breakpoint) is used as a synthetic code for software panics
    // to distinguish them from hardware breakpoints. sepc=ra, stval=0.
    console_write_literal(b"scause=0x0000000000000003 sepc=0x");
    console_write_hex(ra);
    console_write_literal(b" stval=0x0000000000000000\n");
    console_write_fp_chain(fp);
}

#[cfg(not(target_arch = "riscv64"))]
/// Host-build placeholder so the platform surface typechecks on
/// non-riscv64 hosts. No-op; the real implementation is the riscv64
/// version above.
pub fn emit_panic_location(_fp: usize, _ra: usize) {}

#[cfg(target_arch = "riscv64")]
fn console_write_trap_summary(frame: &Rv64TrapFrame) {
    console_write_literal(b"scause=0x");
    console_write_hex(frame.scause);
    console_write_literal(b" sepc=0x");
    console_write_hex(frame.sepc);
    console_write_literal(b" stval=0x");
    console_write_hex(frame.stval);
    console_write_literal(b"\n");
}

#[cfg(target_arch = "riscv64")]
fn console_write_trapframe(frame: &Rv64TrapFrame) {
    console_write_literal(b"trapframe:\n");
    for index in 0..32 {
        if index % 4 == 0 {
            console_write_literal(b"  ");
        } else {
            console_write_literal(b" ");
        }
        console_write_literal(rv64_register_key(index));
        console_write_literal(b"=0x");
        console_write_hex(frame.x[index]);
        if index % 4 == 3 {
            console_write_literal(b"\n");
        }
    }
    console_write_literal(b"  scause=0x");
    console_write_hex(frame.scause);
    console_write_literal(b" sepc=0x");
    console_write_hex(frame.sepc);
    console_write_literal(b" stval=0x");
    console_write_hex(frame.stval);
    console_write_literal(b" sstatus=0x");
    console_write_hex(frame.sstatus);
    console_write_literal(b"\n");
}

#[cfg(target_arch = "riscv64")]
fn rv64_register_key(index: usize) -> &'static [u8] {
    match index {
        0 => b"x0",
        1 => b"x1",
        2 => b"x2",
        3 => b"x3",
        4 => b"x4",
        5 => b"x5",
        6 => b"x6",
        7 => b"x7",
        8 => b"x8",
        9 => b"x9",
        10 => b"x10",
        11 => b"x11",
        12 => b"x12",
        13 => b"x13",
        14 => b"x14",
        15 => b"x15",
        16 => b"x16",
        17 => b"x17",
        18 => b"x18",
        19 => b"x19",
        20 => b"x20",
        21 => b"x21",
        22 => b"x22",
        23 => b"x23",
        24 => b"x24",
        25 => b"x25",
        26 => b"x26",
        27 => b"x27",
        28 => b"x28",
        29 => b"x29",
        30 => b"x30",
        31 => b"x31",
        _ => b"x?",
    }
}

#[cfg(target_arch = "riscv64")]
pub(crate) fn console_write_literal(bytes: &[u8]) {
    for &byte in bytes {
        crate::sbi_console_putchar(byte);
    }
}

#[cfg(target_arch = "riscv64")]
pub(crate) fn console_write_hex(value: usize) {
    // Print 16 hex digits MSB-first. Shifts run 60, 56, ..., 4, 0
    // so each `(value >> shift) & 0xf` captures the correct nibble.
    //
    // (The previous form `(0..64).rev().step_by(4)` yielded
    // 63, 59, ..., 3 — off by 3 bits, which made every printed
    // address look "shifted left by 3" and broke fault triage.)
    for shift in (0..usize::BITS).step_by(4).rev() {
        let digit = ((value >> shift) & 0xf) as u8;
        let byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + (digit - 10)
        };
        crate::sbi_console_putchar(byte);
    }
}
