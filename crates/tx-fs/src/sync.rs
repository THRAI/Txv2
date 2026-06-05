//! tx-fs lock facade.

pub(crate) type SpinMutex<T> = tx_substrate::SpinMutex<T>;

#[cfg(tx_lock_metrics_fs)]
pub(crate) type TmpfsSpinMutex<T> = tx_substrate::SpinMutex<T, tx_substrate::LockMetricsOn>;

#[cfg(not(tx_lock_metrics_fs))]
pub(crate) type TmpfsSpinMutex<T> = SpinMutex<T>;

#[cfg(tx_lock_metrics_fs)]
pub(crate) const fn tmpfs_spin_mutex<T>(value: T, name: &'static [u8]) -> TmpfsSpinMutex<T> {
    tx_substrate::SpinMutex::new_observed(value, tx_substrate::LockMetricsOn::new(name))
}

#[cfg(not(tx_lock_metrics_fs))]
pub(crate) const fn tmpfs_spin_mutex<T>(value: T, _name: &'static [u8]) -> TmpfsSpinMutex<T> {
    SpinMutex::new(value)
}
