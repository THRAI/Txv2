use core::{marker::PhantomData, ptr::NonNull};

use crate::{CpuId, PmapRoot, TxPlatform, VirtAddr};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrapClass {
    PageFault { write: bool, instruction: bool },
    Syscall,
    TimerInterrupt,
    ExternalInterrupt,
    InterprocessorInterrupt,
    IllegalInstruction,
    Breakpoint,
    AlignmentFault { write: bool, instruction: bool },
    UnknownSync,
    UnknownInterrupt,
}

impl TrapClass {
    #[allow(non_upper_case_globals)]
    pub const InstructionPageFault: Self = Self::PageFault {
        write: false,
        instruction: true,
    };
    #[allow(non_upper_case_globals)]
    pub const LoadPageFault: Self = Self::PageFault {
        write: false,
        instruction: false,
    };
    #[allow(non_upper_case_globals)]
    pub const StorePageFault: Self = Self::PageFault {
        write: true,
        instruction: false,
    };
    #[allow(non_upper_case_globals)]
    pub const UserEnvCall: Self = Self::Syscall;
    #[allow(non_upper_case_globals)]
    pub const SupervisorTimer: Self = Self::TimerInterrupt;
    #[allow(non_upper_case_globals)]
    pub const SupervisorExternal: Self = Self::ExternalInterrupt;
    #[allow(non_upper_case_globals)]
    pub const Unknown: Self = Self::UnknownSync;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrapPreviousMode {
    User,
    Supervisor,
    Machine,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrapSnapshot {
    pub class: TrapClass,
    pub pc: VirtAddr,
    pub fault_address: Option<VirtAddr>,
    pub previous_mode: TrapPreviousMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrapFrameSnapshot {
    pub scause: usize,
    pub sepc: usize,
    pub stval: usize,
}

impl TrapFrameSnapshot {
    pub const fn portable(self, class: TrapClass) -> TrapSnapshot {
        TrapSnapshot {
            class,
            pc: VirtAddr(self.sepc),
            fault_address: match class {
                TrapClass::PageFault { .. } | TrapClass::AlignmentFault { .. } => {
                    Some(VirtAddr(self.stval))
                }
                _ => None,
            },
            previous_mode: TrapPreviousMode::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrapFrameView {
    pub pc: VirtAddr,
    pub sp: VirtAddr,
    pub syscall_number: u64,
    pub syscall_args: [u64; 6],
    pub fault_address: Option<VirtAddr>,
    pub faulting_instruction: Option<VirtAddr>,
    pub previous_mode: TrapPreviousMode,
    pub interrupts_enabled_before: bool,
    pub user_tls_register: u64,
}

impl TrapFrameView {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        pc: VirtAddr,
        sp: VirtAddr,
        syscall_number: u64,
        syscall_args: [u64; 6],
        fault_address: Option<VirtAddr>,
        faulting_instruction: Option<VirtAddr>,
        previous_mode: TrapPreviousMode,
        interrupts_enabled_before: bool,
        user_tls_register: u64,
    ) -> Self {
        Self {
            pc,
            sp,
            syscall_number,
            syscall_args,
            fault_address,
            faulting_instruction,
            previous_mode,
            interrupts_enabled_before,
            user_tls_register,
        }
    }
}

/// Architecture-sized user register image used by signal-frame restore.
///
/// The exact meaning of `regs` and `status` is platform-owned. Portable
/// signal code treats this as an opaque saved context and passes it back to
/// the selected platform through `SignalFrameIf::restore_signal_frame`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserFpContext {
    pub regs: [u64; 32],
    pub fcsr: u32,
    pub fcc: u8,
    pub _reserved0: [u8; 3],
    /// Bit 0: FP state valid. Bit 1: FP state dirty.
    pub flags: u32,
    pub _reserved1: u32,
}

impl UserFpContext {
    pub const FLAG_VALID: u32 = 1 << 0;
    pub const FLAG_DIRTY: u32 = 1 << 1;

    pub const fn empty() -> Self {
        Self {
            regs: [0; 32],
            fcsr: 0,
            fcc: 0,
            _reserved0: [0; 3],
            flags: 0,
            _reserved1: 0,
        }
    }

    pub const fn is_valid(&self) -> bool {
        self.flags & Self::FLAG_VALID != 0
    }
}

impl Default for UserFpContext {
    fn default() -> Self {
        Self::empty()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserTrapContext {
    pub regs: [usize; 32],
    pub pc: usize,
    pub status: usize,
    pub fp: UserFpContext,
}

impl UserTrapContext {
    pub const fn empty() -> Self {
        Self {
            regs: [0; 32],
            pc: 0,
            status: 0,
            fp: UserFpContext::empty(),
        }
    }
}

impl Default for UserTrapContext {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalHandlerRegs {
    pub return_pc: VirtAddr,
    pub args: [usize; 3],
}

#[derive(Debug)]
pub struct TrapFrameMut<'a> {
    view: TrapFrameView,
    raw: NonNull<()>,
    vtable: &'static TrapFrameMutVtable,
    _frame: PhantomData<&'a mut ()>,
}

#[derive(Debug)]
pub struct TrapFrameMutVtable {
    pub read_view: fn(NonNull<()>) -> TrapFrameView,
    pub set_pc: fn(NonNull<()>, VirtAddr),
    pub set_sp: fn(NonNull<()>, VirtAddr),
    pub set_syscall_return: fn(NonNull<()>, i64),
    pub set_syscall_error: fn(NonNull<()>, i32),
    pub set_user_tls_register: fn(NonNull<()>, u64),
    pub capture_user_context: fn(NonNull<()>) -> UserTrapContext,
    pub restore_user_context: fn(NonNull<()>, &UserTrapContext),
    pub set_signal_handler_regs: fn(NonNull<()>, SignalHandlerRegs),
    pub rewind_pc: fn(NonNull<()>, usize),
}

impl<'a> TrapFrameMut<'a> {
    /// Build a mutable trap-frame handle from a platform-owned raw frame.
    ///
    /// # Safety
    ///
    /// `raw` must point to the same live raw trap frame used to construct
    /// `view`; the frame must remain uniquely borrowed for `'a`; and `vtable`
    /// must contain writeback functions for that exact raw frame layout.
    pub unsafe fn from_raw_parts(
        view: TrapFrameView,
        raw: NonNull<()>,
        vtable: &'static TrapFrameMutVtable,
    ) -> Self {
        Self {
            view,
            raw,
            vtable,
            _frame: PhantomData,
        }
    }

    pub const fn view(&self) -> TrapFrameView {
        self.view
    }

    pub fn set_pc(&mut self, pc: VirtAddr) {
        (self.vtable.set_pc)(self.raw, pc);
        self.view.pc = pc;
    }

    pub fn set_sp(&mut self, sp: VirtAddr) {
        (self.vtable.set_sp)(self.raw, sp);
        self.view.sp = sp;
    }

    pub fn set_syscall_return(&mut self, value: i64) {
        (self.vtable.set_syscall_return)(self.raw, value);
        self.view.syscall_args[0] = value as u64;
    }

    pub fn set_syscall_error(&mut self, errno: i32) {
        let encoded = -i64::from(errno);
        (self.vtable.set_syscall_error)(self.raw, errno);
        self.view.syscall_args[0] = encoded as u64;
    }

    pub fn set_user_tls_register(&mut self, value: u64) {
        (self.vtable.set_user_tls_register)(self.raw, value);
        self.view.user_tls_register = value;
    }

    pub fn capture_user_context(&self) -> UserTrapContext {
        (self.vtable.capture_user_context)(self.raw)
    }

    pub fn restore_user_context(&mut self, context: &UserTrapContext) {
        (self.vtable.restore_user_context)(self.raw, context);
        self.view = (self.vtable.read_view)(self.raw);
    }

    pub fn set_signal_handler_regs(&mut self, regs: SignalHandlerRegs) {
        (self.vtable.set_signal_handler_regs)(self.raw, regs);
        self.view.syscall_args[0] = regs.args[0] as u64;
        self.view.syscall_args[1] = regs.args[1] as u64;
        self.view.syscall_args[2] = regs.args[2] as u64;
    }

    pub fn rewind_pc(&mut self, bytes: usize) {
        (self.vtable.rewind_pc)(self.raw, bytes);
        self.view.pc = VirtAddr(self.view.pc.0.saturating_sub(bytes));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FaultInfo {
    pub address: VirtAddr,
    pub write: bool,
    pub instruction: bool,
    pub from_user: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub enum TrapAction {
    Resume,
    Reschedule,
    DeliverSignal,
    Terminate,
}

pub trait KernelTrapSink<P: TxPlatform> {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction;

    fn on_syscall(view: TrapFrameMut<'_>) -> TrapAction;

    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction;

    fn on_external_irq(cpu: CpuId) -> TrapAction;

    fn on_ipi(cpu: CpuId) -> TrapAction;

    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction;
}

pub trait TrapIf {
    fn install_minimal_trap_vector() {}

    fn install_kernel_trap_vector() {}

    fn install_user_trap_vector() {
        Self::install_kernel_trap_vector();
    }

    fn classify_trap(_snapshot: TrapFrameSnapshot) -> TrapClass {
        TrapClass::UnknownSync
    }

    fn snapshot_trap(snapshot: TrapFrameSnapshot) -> TrapSnapshot {
        snapshot.portable(Self::classify_trap(snapshot))
    }

    /// Enter userspace with the given context, applying the optional
    /// pending syscall return into the architecture's `a0`-equivalent
    /// register before the platform's `sret`/`ertn`/equivalent.
    ///
    /// **Returns** when the resulting userspace round-trip is
    /// rescheduled by the trap shell (i.e. the kernel-side trap
    /// handler returned [`TrapAction::Reschedule`] and the platform
    /// longjmped back to the kernel context that issued this call).
    /// Returning here is what makes the stackless-coroutine thread
    /// future model work: control unwinds back into the future's
    /// `poll`, which then awaits the freshly-resolved
    /// userspace-run wait and dispatches the trap.
    ///
    /// Resume / DeliverSignal trap actions do NOT return here — they
    /// re-enter userspace directly through the platform's trap-vector
    /// exit path.
    ///
    /// Platforms whose trap shell does not yet implement the
    /// reschedule longjmp may still implement this method as
    /// divergent (the body never returns at runtime); the type is
    /// `()` so it composes with the future-driven thread runtime.
    ///
    /// This is the single platform-side site that mutates user-visible
    /// registers per the Plan B writeback discipline pinned by
    /// `txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`
    /// (`docs/design/02_execution/THREAD_RUNTIME_v1.md`).
    ///
    /// The default implementation panics; platforms that ship a
    /// production userspace-entry path override it. Host-test
    /// platforms (no real `sret`) can also override with an infinite
    /// loop or a panic spelling the configuration error.
    fn enter_userspace_with_context(_ctx: &UserTrapContext, _root: &PmapRoot) {
        panic!("TrapIf::enter_userspace_with_context: platform has no userspace-entry shim");
    }
}
