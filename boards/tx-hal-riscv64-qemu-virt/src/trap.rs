use core::ptr::NonNull;

use crate::{boot_static, user_access, Platform};
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

    .globl tx_rv64_qemu_minimal_trap_vector
tx_rv64_qemu_minimal_trap_vector:
    addi sp, sp, -TX_RV64_TF_SIZE
    sd t0, TX_RV64_TF_X5(sp)
    sd zero, TX_RV64_TF_X0(sp)
    sd ra, TX_RV64_TF_X1(sp)
    addi t0, sp, TX_RV64_TF_SIZE
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

    ld t0, TX_RV64_TF_SEPC(sp)
    csrw sepc, t0
    ld t0, TX_RV64_TF_SSTATUS(sp)
    csrw sstatus, t0

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
    ld sp, TX_RV64_TF_X2(sp)
    sret
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
}

pub fn dispatch_trap_frame<K>(frame: &mut Rv64TrapFrame) -> TrapAction
where
    K: KernelTrapSink<Platform>,
{
    let class = classify_rv64_trap(frame.scause);
    let from_user = frame.previous_mode() == TrapPreviousMode::User;

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
}

#[cfg(target_arch = "riscv64")]
#[no_mangle]
extern "C" fn tx_rv64_qemu_kernel_trap_entry(frame: &mut Rv64TrapFrame) {
    let action = unsafe { tx_kernel_riscv64_qemu_trap_dispatch(frame) };
    apply_trap_action(frame, action);
}

#[cfg(target_arch = "riscv64")]
fn apply_trap_action(frame: &Rv64TrapFrame, action: TrapAction) {
    match action {
        TrapAction::Resume | TrapAction::Reschedule | TrapAction::DeliverSignal => {}
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
fn console_write_literal(bytes: &[u8]) {
    for &byte in bytes {
        crate::sbi_console_putchar(byte);
    }
}

#[cfg(target_arch = "riscv64")]
fn console_write_hex(value: usize) {
    for shift in (0..usize::BITS).rev().step_by(4) {
        let digit = ((value >> shift) & 0xf) as u8;
        let byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + (digit - 10)
        };
        crate::sbi_console_putchar(byte);
    }
}
