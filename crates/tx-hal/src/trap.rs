use core::{marker::PhantomData, ptr::NonNull};

use crate::{CpuId, TxPlatform, VirtAddr};

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
pub struct TrapFrameView<'a> {
    pub pc: VirtAddr,
    pub sp: VirtAddr,
    pub syscall_number: u64,
    pub syscall_args: [u64; 6],
    pub fault_address: Option<VirtAddr>,
    pub previous_mode: TrapPreviousMode,
    _frame: PhantomData<&'a ()>,
}

impl<'a> TrapFrameView<'a> {
    pub const fn new(
        pc: VirtAddr,
        sp: VirtAddr,
        syscall_number: u64,
        syscall_args: [u64; 6],
        fault_address: Option<VirtAddr>,
        previous_mode: TrapPreviousMode,
    ) -> Self {
        Self {
            pc,
            sp,
            syscall_number,
            syscall_args,
            fault_address,
            previous_mode,
            _frame: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct TrapFrameMut<'a> {
    view: TrapFrameView<'a>,
    raw: NonNull<()>,
    vtable: &'static TrapFrameMutVtable,
    _frame: PhantomData<&'a mut ()>,
}

#[derive(Debug)]
pub struct TrapFrameMutVtable {
    pub set_pc: fn(NonNull<()>, VirtAddr),
    pub set_sp: fn(NonNull<()>, VirtAddr),
    pub set_syscall_return: fn(NonNull<()>, i64),
    pub set_syscall_error: fn(NonNull<()>, i32),
    pub set_user_tls_register: fn(NonNull<()>, u64),
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
        view: TrapFrameView<'a>,
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

    pub const fn view(&self) -> TrapFrameView<'_> {
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

    fn on_timer_interrupt(cpu: CpuId) -> TrapAction;

    fn on_external_irq(cpu: CpuId) -> TrapAction;

    fn on_ipi(cpu: CpuId) -> TrapAction;

    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction;
}

pub trait TrapIf {
    fn install_minimal_trap_vector() {}

    fn install_kernel_trap_vector() {}

    fn classify_trap(_snapshot: TrapFrameSnapshot) -> TrapClass {
        TrapClass::UnknownSync
    }

    fn snapshot_trap(snapshot: TrapFrameSnapshot) -> TrapSnapshot {
        snapshot.portable(Self::classify_trap(snapshot))
    }
}
