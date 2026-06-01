#[cfg(not(tx_lock_metrics_vm))]
use crate::vm::adapter::step_engine::LockMetricsOff;
#[cfg(tx_lock_metrics_vm)]
use crate::vm::adapter::step_engine::LockMetricsOn;
use crate::vm::adapter::step_engine::SpinMutex;

#[cfg(tx_lock_metrics_vm)]
pub(in crate::vm) type VmSpinMutex<T> = SpinMutex<T, LockMetricsOn>;

#[cfg(not(tx_lock_metrics_vm))]
pub(in crate::vm) type VmSpinMutex<T> = SpinMutex<T, LockMetricsOff>;

#[cfg(tx_lock_metrics_vm)]
pub(in crate::vm) const fn vm_spin_mutex<T>(value: T, name: &'static [u8]) -> VmSpinMutex<T> {
    SpinMutex::new_observed(value, LockMetricsOn::new(name))
}

#[cfg(not(tx_lock_metrics_vm))]
pub(in crate::vm) const fn vm_spin_mutex<T>(value: T, _name: &'static [u8]) -> VmSpinMutex<T> {
    SpinMutex::new(value)
}
