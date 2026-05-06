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
pub mod reactor_submit;
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

    /// Reset the mount-table registry. Pairs with the
    /// `mount::register_mount` / `mount::mount_for` table populated
    /// by `init.rs::mount_devfs_at_dev`; tests that re-drive boot
    /// wiring (e.g. `tx-kernel/src/init/tests.rs`) must clear the
    /// table between runs.
    pub fn reset_mount_table() {
        crate::mount::reset_mount_table_for_test();
    }

    /// Reset the monotonic mount-id counter to its post-boot starting
    /// value (1). Pairs with `mount::allocate_mount_id`; tests that
    /// re-drive boot wiring rely on the deterministic sequence
    /// `MountId(1)` (rootfs) → `MountId(2)` (devfs).
    pub fn reset_mount_id_counter() {
        crate::mount::reset_mount_id_counter_for_test();
    }

    /// Reset the monotonic dev-id counter to its post-boot starting
    /// value (1). Pairs with `mount::allocate_dev_id`.
    pub fn reset_dev_id_counter() {
        crate::mount::reset_dev_id_counter_for_test();
    }

    /// Clear the reactor-submission seam slot so a subsequent
    /// `install_submit_child_thread` call sees an empty slot. Used
    /// by `tx-kernel`'s init tests to verify the install seam end-
    /// to-end (Wave 1 of the fork/clone/wait4 slice).
    pub fn reset_reactor_submit_seam() {
        crate::reactor_submit::reset_for_test();
    }
}
