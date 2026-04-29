//! Reader-side epoch guard.
//!
//! A guard pins the current CPU and publishes that CPU's current epoch. While it
//! is alive, memory reached through `IdentRef` or other epoch-protected paths
//! must not be physically reclaimed.

use core::marker::PhantomData;

use super::domain::EpochDomain;
use super::local::CpuLocalEpochState;
use tx_hal::{CpuId, CpuPinGuard};

/// RAII epoch guard.
///
/// This value is deliberately `!Send` and `!Sync`: an epoch guard represents
/// the current CPU's read-side critical section and must not migrate as an
/// owned Rust value.
pub struct Guard<'g> {
    /// Domain to notify when the guard leaves.
    domain: &'static EpochDomain,
    /// Current CPU's local epoch slot.
    local: &'static CpuLocalEpochState,
    /// Captured for diagnostics and for callers that need to know where the
    /// read-side critical section was entered.
    cpu_id: CpuId,
    /// Epoch value published into the local CPU state.
    entered_epoch: u64,
    /// Holding this value prevents migration while the guard is alive.
    _cpu_pin: CpuPinGuard,
    _scope: PhantomData<&'g ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'g> Guard<'g> {
    pub(crate) fn new(
        domain: &'static EpochDomain,
        local: &'static CpuLocalEpochState,
        cpu_id: CpuId,
        entered_epoch: u64,
        cpu_pin: CpuPinGuard,
    ) -> Self {
        Self {
            domain,
            local,
            cpu_id,
            entered_epoch,
            _cpu_pin: cpu_pin,
            _scope: PhantomData,
            _not_send_sync: PhantomData,
        }
    }

    pub fn entered_epoch(&self) -> u64 {
        self.entered_epoch
    }

    pub fn cpu_id(&self) -> CpuId {
        self.cpu_id
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.local.leave();
        self.domain.leave_guard();
    }
}
