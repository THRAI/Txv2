#![no_std]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod cred;
pub mod device;
pub mod execution;
pub mod mount;
pub mod page_backed;
pub mod process;
pub mod signal;
pub mod thread_runtime;
pub mod tty;
pub mod vfs;
pub mod vm;
pub mod wait_carrier;
pub mod zones;

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

/// Cross-crate test-support shims. Enabled by the `test-support`
/// feature; downstream crates flip the feature on through their
/// `[dev-dependencies]` so their test binaries can reset the
/// process-subsystem static state (init slot, pid counter, tid
/// counter) between tests.
///
/// **Production code must not call any of these.** They mutate
/// process-wide singletons in ways that break running threads. The
/// `cfg(any(test, feature = "test-support"))` gate keeps them out of
/// release builds.
#[cfg(any(test, feature = "test-support"))]
pub mod cross_crate_test_support {
    /// Clear the kernel-global init-process slot. Pairs with
    /// `tx_subsystems::process::bootstrap_init_process` so tests can
    /// re-bootstrap between runs without hitting
    /// `BootstrapError::AlreadyBootstrapped`.
    pub fn reset_init_process() {
        crate::process::execution::reset_init_process_for_test();
    }

    /// Reset the monotonic pid counter to its post-init starting
    /// value (2). Pairs with `bootstrap_init_process` so the first
    /// `step_fork` after reset always allocates pid 2.
    pub fn reset_pid_counter() {
        crate::process::structure::reset_pid_counter_for_test();
    }

    /// Reset the monotonic tid counter to its post-init starting
    /// value (2). Pairs with `bootstrap_init_process` so the first
    /// fresh thread allocation after reset gets tid 2.
    pub fn reset_tid_counter() {
        crate::thread_runtime::structure::reset_tid_counter_for_test();
    }
}
