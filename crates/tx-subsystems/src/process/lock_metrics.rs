#[cfg(not(tx_lock_metrics_process))]
use crate::sync::SpinMutex;

#[cfg(tx_lock_metrics_process)]
pub(crate) type ProcessSpinMutex<T> = crate::sync::ObservedSpinMutex<T>;

#[cfg(not(tx_lock_metrics_process))]
pub(crate) type ProcessSpinMutex<T> = SpinMutex<T>;

#[cfg(tx_lock_metrics_process)]
pub(crate) const fn process_spin_mutex<T>(value: T, name: &'static [u8]) -> ProcessSpinMutex<T> {
    crate::sync::observed_spin_mutex(value, name)
}

#[cfg(not(tx_lock_metrics_process))]
pub(crate) const fn process_spin_mutex<T>(value: T, _name: &'static [u8]) -> ProcessSpinMutex<T> {
    SpinMutex::new(value)
}
