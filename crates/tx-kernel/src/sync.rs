//! tx-kernel lock facade.
//!
//! Kernel boot/runtime code imports this wrapper instead of the substrate
//! primitive directly, so lock policy and optional metrics stay behind one
//! local API surface.

#[cfg(tx_lock_metrics_kernel)]
pub(crate) type SpinMutex<T> = tx_substrate::SpinMutex<T, tx_substrate::LockMetricsOn>;

#[cfg(not(tx_lock_metrics_kernel))]
pub(crate) type SpinMutex<T> = tx_substrate::SpinMutex<T>;

#[cfg(tx_lock_metrics_kernel)]
pub(crate) const fn spin_mutex<T>(value: T, name: &'static [u8]) -> SpinMutex<T> {
    tx_substrate::SpinMutex::new_observed(value, tx_substrate::LockMetricsOn::new(name))
}

#[cfg(not(tx_lock_metrics_kernel))]
pub(crate) const fn spin_mutex<T>(value: T, _name: &'static [u8]) -> SpinMutex<T> {
    tx_substrate::SpinMutex::new(value)
}
