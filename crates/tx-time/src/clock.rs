//! Consumer-facing clock read capability.

use crate::types::{ClockId, TimeError};

/// Read-only clock capability for syscall, VFS, procfs, and timer consumers.
pub trait ClockRead {
    fn monotonic_now_ns(&self) -> u64;

    fn realtime_now_ns(&self) -> u64;

    fn now_ns(&self, clock: ClockId) -> Result<u64, TimeError> {
        match clock {
            ClockId::Realtime => Ok(self.realtime_now_ns()),
            ClockId::Monotonic | ClockId::Boottime => Ok(self.monotonic_now_ns()),
            ClockId::Tai | ClockId::ProcessCpuTime | ClockId::ThreadCpuTime => {
                Err(TimeError::Unsupported)
            }
        }
    }
}
