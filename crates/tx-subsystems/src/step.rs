use tx_substrate::bus::{RawPort, RawQueue};

// POSIX-style spelling matches the canonical names used uniformly across
// `docs/design/02_execution/EXEC_v1.md`,
// `docs/design/03_memory-vm/PAGE_BACKED_v1.md`,
// `docs/design/05_filesystem/VFS_CHECKS_V2.1.md`, and
// `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Errno {
    EACCES,
    EBUSY,
    EINVAL,
    EISDIR,
    ELOOP,
    ENAMETOOLONG,
    ENOENT,
    ENOSYS,
    ENOTDIR,
    ESTALE,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Progress {
    Units(usize),
}

pub enum WakeCarrier {
    Queue(RawQueue),
    Port(RawPort),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterestConditions {
    pub bits: u64,
}

pub enum StepOutcome<T> {
    Advanced(Progress),
    Blocked(WakeCarrier, InterestConditions),
    AdvancedThenBlocked(Progress, WakeCarrier, InterestConditions),
    Done(T),
    Err(Errno),
}
