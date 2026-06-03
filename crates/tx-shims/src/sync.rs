//! tx-shims lock facade.

pub(crate) type SpinMutex<T> = tx_substrate::SpinMutex<T>;
