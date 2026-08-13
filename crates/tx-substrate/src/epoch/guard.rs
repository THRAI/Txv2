//! Reader-side epoch guard.
//!
//! A guard pins the current CPU and publishes that CPU's current epoch. While it
//! is alive, memory reached through `IdentRef` or other epoch-protected paths
//! must not be physically reclaimed.

use core::marker::PhantomData;

use super::local::CpuLocalEpochState;
use tx_hal::{CpuId, CpuPinGuard};

/// RAII epoch guard.
///
/// This value is deliberately `!Send` and `!Sync`: an epoch guard represents
/// the current CPU's read-side critical section and must not migrate as an
/// owned Rust value.
///
/// When `owned` is false this is a *borrow* of an existing guard: Drop is a
/// no-op and epoch counters are not modified.  Use `epoch::borrow_current_guard`
/// to obtain one when a caller already holds a guard but needs to satisfy an
/// API that requires `&Guard`.
pub struct Guard<'g> {
    /// Current CPU's local epoch slot.
    local: &'static CpuLocalEpochState,
    /// Captured for diagnostics and for callers that need to know where the
    /// read-side critical section was entered.
    cpu_id: CpuId,
    /// Epoch value published into the local CPU state.
    entered_epoch: u64,
    /// Holding this value prevents migration while the guard is alive.
    _cpu_pin: CpuPinGuard,
    /// When false this guard borrows an existing epoch window; Drop is a no-op.
    owned: bool,
    pin_accounted: bool,
    _scope: PhantomData<&'g ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'g> Guard<'g> {
    pub(crate) fn new(
        local: &'static CpuLocalEpochState,
        cpu_id: CpuId,
        entered_epoch: u64,
        cpu_pin: CpuPinGuard,
    ) -> Self {
        Self {
            local,
            cpu_id,
            entered_epoch,
            _cpu_pin: cpu_pin,
            owned: true,
            pin_accounted: true,
            _scope: PhantomData,
            _not_send_sync: PhantomData,
        }
    }

    /// Create a borrow-mode guard that does not modify epoch counters on
    /// creation or drop.  The caller must already hold a real guard on this CPU.
    pub(crate) fn new_borrowed(
        local: &'static CpuLocalEpochState,
        cpu_id: CpuId,
        entered_epoch: u64,
        cpu_pin: CpuPinGuard,
    ) -> Self {
        Self {
            local,
            cpu_id,
            entered_epoch,
            _cpu_pin: cpu_pin,
            owned: false,
            pin_accounted: false,
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
        if self.owned {
            self.local.leave();
        }
        if self.pin_accounted {
            self.local.unpin();
        }
    }
}
