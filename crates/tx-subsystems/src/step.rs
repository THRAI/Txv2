use tx_substrate::bus::{RawPort, RawQueue};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Errno {
    NoEntry,
    NotDirectory,
    IsDirectory,
    Invalid,
    PermissionDenied,
    NameTooLong,
    TooManySymlinks,
    Stale,
    Busy,
    NotImplemented,
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
