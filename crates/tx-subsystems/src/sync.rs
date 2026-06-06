//! Subsystem lock facade.
//!
//! Runtime subsystems use this module instead of importing
//! `tx_substrate::SpinMutex` directly. That keeps lock instrumentation and
//! future lock-policy swaps behind one local API surface.

pub(crate) type SpinMutex<T> = tx_substrate::SpinMutex<T>;

#[cfg(any(tx_lock_metrics_vm, tx_lock_metrics_process))]
pub(crate) type ObservedSpinMutex<T> = tx_substrate::SpinMutex<T, tx_substrate::LockMetricsOn>;

#[cfg(any(tx_lock_metrics_vm, tx_lock_metrics_process))]
pub(crate) const fn observed_spin_mutex<T>(value: T, name: &'static [u8]) -> ObservedSpinMutex<T> {
    tx_substrate::SpinMutex::new_observed(value, tx_substrate::LockMetricsOn::new(name))
}
