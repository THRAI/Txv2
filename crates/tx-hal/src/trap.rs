use crate::VirtAddr;

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
