use core::ptr::NonNull;

use crate::{boot_static, user_access, Platform};
#[cfg(target_arch = "riscv64")]
use crate::{current_kernel_resume_ctx_ptr, trap_stack_top_for_cpu, KernelResumeCtx};
#[cfg(target_arch = "riscv64")]
use tx_hal::SmpIf;
use tx_hal::{
    FaultInfo, KernelTrapSink, SignalHandlerRegs, TrapAction, TrapClass, TrapFrameMut,
    TrapFrameMutVtable, TrapFrameSnapshot, TrapFrameView, TrapIf, TrapPreviousMode,
    UserTrapContext, VirtAddr,
};

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .section .text.trap, "ax"
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
    .equ TX_RV64_TF_SIZE, 288

    # Per-hart KernelResumeCtx field offsets — must match
    # `boards::tx_hal_riscv64_qemu_virt::KernelResumeCtx` in lib.rs.
    .equ TX_RV64_RCTX_SP, 0
    .equ TX_RV64_RCTX_RA, 8
    .equ TX_RV64_RCTX_S0, 16
    # s1..s11 follow at +8 each up to offset 104.

    .globl tx_rv64_qemu_minimal_trap_vector
tx_rv64_qemu_minimal_trap_vector:
    # Slice 2 trap-vector prologue: sscratch swap onto the per-CPU
    # trap-handler stack. sscratch is boot-primed (and re-primed on
    # every clean exit) to point at trap_stack_top for this hart.
    #
    # On entry: sp = trap-time sp (user sp for from-user, kernel sp
    # for from-kernel); sscratch = trap_stack_top.
    # After swap: sp = trap_stack_top; sscratch = trap-time sp.
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
    sd gp, TX_RV64_TF_X3(sp)
    sd tp, TX_RV64_TF_X4(sp)
    sd t1, TX_RV64_TF_X6(sp)
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
    # Discipline: stash the trap-time sp into sscratch FIRST (before
    # restoring user temporaries), so we have it for the final
    # swap. After all user regs are restored, deallocate the frame
    # (sp moves up to trap_stack_top), then `csrrw sp, sscratch, sp`
    # atomically: sp = trap-time sp, sscratch = trap_stack_top
    # (re-primed for the next trap).
    ld t0, TX_RV64_TF_SEPC(sp)
    csrw sepc, t0
    ld t0, TX_RV64_TF_SSTATUS(sp)
    csrw sstatus, t0

    # Move trap-time sp into sscratch via t0. Safe to clobber t0
    # because we'll restore the user's t0 from the frame below
    # before we sret.
    ld t0, TX_RV64_TF_X2(sp)
    csrw sscratch, t0

    ld ra, TX_RV64_TF_X1(sp)
    ld gp, TX_RV64_TF_X3(sp)
    ld tp, TX_RV64_TF_X4(sp)
    ld t0, TX_RV64_TF_X5(sp)
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

    # Deallocate the frame and atomically swap sp ↔ sscratch:
    # sp = trap-time sp, sscratch = trap_stack_top.
    addi sp, sp, TX_RV64_TF_SIZE
    csrrw sp, sscratch, sp
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
"#
);

const RV64_SSTATUS_SPP: usize = 1 << 8;
const RV64_SSTATUS_SPIE: usize = 1 << 5;
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

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rv64TrapFrame {
    pub x: [usize; 32],
    pub scause: usize,
    pub sepc: usize,
    pub stval: usize,
    pub sstatus: usize,
}

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
        UserTrapContext {
            regs: self.x,
            pc: self.sepc,
            status: self.sstatus,
            fp: tx_hal::UserFpContext::empty(),
        }
    }

    fn restore_user_context(&mut self, context: &UserTrapContext) {
        self.x = context.regs;
        self.x[0] = 0;
        self.sepc = context.pc;
        self.sstatus = context.status;
        self.prepare_user_return();
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
        self.sstatus &= !RV64_SSTATUS_SPP;
        self.sstatus |= RV64_SSTATUS_SPIE;
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
    fn enter_userspace_with_context(ctx: UserTrapContext) {
        crate::debug_trace::record_entry(&ctx);

        let mut frame = Rv64TrapFrame {
            x: [0; 32],
            scause: 0,
            sepc: 0,
            stval: 0,
            sstatus: 0,
        };
        frame.restore_user_context(&ctx);
        // `restore_user_context` already calls `prepare_user_return`,
        // which clears SPP and sets SPIE so `sret` lands in user mode
        // with interrupts enabled.

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
            K::on_timer_interrupt(<Platform as tx_hal::SmpIf>::current_cpu_id())
        }
        TrapClass::ExternalInterrupt => {
            let _irq_context = crate::enter_irq_context();
            K::on_external_irq(<Platform as tx_hal::SmpIf>::current_cpu_id())
        }
        TrapClass::InterprocessorInterrupt => {
            let _irq_context = crate::enter_irq_context();
            K::on_ipi(<Platform as tx_hal::SmpIf>::current_cpu_id())
        }
        TrapClass::IllegalInstruction
        | TrapClass::Breakpoint
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

    loop {
        core::hint::spin_loop();
    }
}

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
