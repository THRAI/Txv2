//! LA64 hardware trap frame: the register/CSR image the trap-entry stub pushes,
//! its portable projections (`snapshot` / `view` / `view_mut`), the syscall /
//! signal / user-context mutators, and the `TrapFrameMut` vtable wiring that
//! exposes those mutators to the arch-neutral reactor.

use core::ptr::NonNull;

use super::la64_irq_trap::classify_la64_trap;
use super::*;

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
        TrapFrameSnapshot {
            cause: self.estat,
            pc: self.era,
            fault_value: self.badv,
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

    pub(crate) fn set_syscall_return(&mut self, value: i64) {
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

    pub(crate) fn restore_user_context(&mut self, context: &UserTrapContext) {
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
