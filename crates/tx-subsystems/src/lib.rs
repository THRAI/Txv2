#![no_std]

extern crate alloc;

pub mod mount;
pub mod page_backed;
pub mod process {}
pub mod step;
pub mod thread_runtime {}
pub mod tty {}
pub mod vfs;
pub mod vm {}

#[cfg(test)]
pub(crate) mod test_support {
    use core::sync::atomic::{AtomicBool, Ordering};

    static EPOCH_TEST_LOCK: AtomicBool = AtomicBool::new(false);

    pub(crate) struct EpochTestGuard;

    impl EpochTestGuard {
        pub(crate) fn acquire() -> Self {
            while EPOCH_TEST_LOCK
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                core::hint::spin_loop();
            }
            Self
        }
    }

    impl Drop for EpochTestGuard {
        fn drop(&mut self) {
            EPOCH_TEST_LOCK.store(false, Ordering::Release);
        }
    }
}
