//! Tiny spin lock used by early substrate code.
//!
//! This is deliberately minimal: it is enough for BSP/smoke paths and local
//! slab-list protection before the full scheduler-aware locking layer exists.

use core::sync::atomic::{AtomicBool, Ordering};

use tx_hal::LocalExecutionGuard;

pub(crate) struct SpinLock {
    /// False means unlocked, true means held.
    held: AtomicBool,
}

impl SpinLock {
    pub(crate) const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    pub(crate) fn lock(&self) -> SpinGuard<'_> {
        self.lock_with(super::runtime::exclude_local_execution)
    }

    fn lock_with<F>(&self, exclude_local_execution: F) -> SpinGuard<'_>
    where
        F: Fn() -> LocalExecutionGuard,
    {
        self.lock_with_hooks(exclude_local_execution, || {}, crate::SpinWait::tick)
    }

    fn lock_with_hooks<F, B, W>(
        &self,
        exclude_local_execution: F,
        mut before_compare: B,
        mut wait_once: W,
    ) -> SpinGuard<'_>
    where
        F: Fn() -> LocalExecutionGuard,
        B: FnMut(),
        W: FnMut(&mut crate::SpinWait),
    {
        let mut wait = crate::SpinWait::new();
        loop {
            if self.held.load(Ordering::Relaxed) {
                wait_once(&mut wait);
                continue;
            }

            // A successful holder must already exclude same-CPU IRQ reentry.
            // A waiter restores the caller's prior IRQ state while spinning,
            // which keeps IRQs serviceable when the caller entered enabled.
            let local_execution = exclude_local_execution();
            before_compare();
            match self
                .held
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => {
                    return SpinGuard {
                        lock: self,
                        _local_execution: local_execution,
                    };
                }
                Err(_) => {
                    drop(local_execution);
                    wait_once(&mut wait);
                }
            }
        }
    }
}

pub(crate) struct SpinGuard<'a> {
    lock: &'a SpinLock,
    _local_execution: LocalExecutionGuard,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;
    use core::ptr;
    use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

    use super::SpinLock;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static EXCLUSION_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RESTORE_EXPECTS_UNLOCKED: AtomicPtr<SpinLock> = AtomicPtr::new(ptr::null_mut());

    std::thread_local! {
        static INTERRUPTS_ENABLED: Cell<bool> = const { Cell::new(true) };
    }

    fn interrupts_enabled() -> bool {
        INTERRUPTS_ENABLED.with(Cell::get)
    }

    fn set_interrupts_enabled(enabled: bool) {
        INTERRUPTS_ENABLED.with(|state| state.set(enabled));
    }

    unsafe fn restore_test_local_execution(saved_state: usize) {
        let expected_unlocked = RESTORE_EXPECTS_UNLOCKED.load(Ordering::Acquire);
        if !expected_unlocked.is_null() {
            assert!(!unsafe { &*expected_unlocked }.held.load(Ordering::Acquire));
        }
        set_interrupts_enabled(saved_state != 0);
    }

    fn exclude_test_local_execution() -> tx_hal::LocalExecutionGuard {
        let enabled = interrupts_enabled();
        set_interrupts_enabled(false);
        EXCLUSION_CALLS.fetch_add(1, Ordering::AcqRel);
        unsafe { tx_hal::LocalExecutionGuard::new(enabled as usize, restore_test_local_execution) }
    }

    #[test]
    fn contended_wait_keeps_local_interrupts_enabled() {
        let _serial = TEST_LOCK.lock().expect("zone spin test lock");
        let lock = SpinLock::new();
        lock.held.store(true, Ordering::Release);
        set_interrupts_enabled(true);
        EXCLUSION_CALLS.store(0, Ordering::Release);
        let observed_wait = Cell::new(false);

        let guard = lock.lock_with_hooks(
            exclude_test_local_execution,
            || {},
            |wait| {
                assert!(interrupts_enabled());
                observed_wait.set(true);
                lock.held.store(false, Ordering::Release);
                wait.tick_with(|| {});
            },
        );

        assert!(observed_wait.get());
        assert!(!interrupts_enabled());
        assert_eq!(EXCLUSION_CALLS.load(Ordering::Acquire), 1);
        RESTORE_EXPECTS_UNLOCKED
            .store(&lock as *const SpinLock as *mut SpinLock, Ordering::Release);
        drop(guard);
        RESTORE_EXPECTS_UNLOCKED.store(ptr::null_mut(), Ordering::Release);
        assert!(interrupts_enabled());
    }

    fn force_failed_compare(initially_enabled: bool) {
        let lock = SpinLock::new();
        set_interrupts_enabled(initially_enabled);
        EXCLUSION_CALLS.store(0, Ordering::Release);
        let injected = Cell::new(false);
        let observed_failed_wait = Cell::new(false);

        let guard = lock.lock_with_hooks(
            exclude_test_local_execution,
            || {
                if !injected.replace(true) {
                    // Model another CPU winning after our optimistic load but
                    // before this CPU's compare-exchange.
                    lock.held.store(true, Ordering::Release);
                }
            },
            |wait| {
                assert_eq!(interrupts_enabled(), initially_enabled);
                observed_failed_wait.set(true);
                lock.held.store(false, Ordering::Release);
                wait.tick_with(|| {});
            },
        );

        assert!(observed_failed_wait.get());
        assert!(!interrupts_enabled());
        assert!(EXCLUSION_CALLS.load(Ordering::Acquire) >= 2);
        drop(guard);
        assert_eq!(interrupts_enabled(), initially_enabled);
    }

    #[test]
    fn failed_compare_restores_the_callers_prior_irq_state() {
        let _serial = TEST_LOCK.lock().expect("zone spin test lock");
        force_failed_compare(true);
        force_failed_compare(false);
    }

    #[test]
    fn nested_exclusion_is_restored_in_lifo_order() {
        let _serial = TEST_LOCK.lock().expect("zone spin test lock");
        let lock = SpinLock::new();
        set_interrupts_enabled(true);

        let outer = exclude_test_local_execution();
        assert!(!interrupts_enabled());
        let guard = lock.lock_with(exclude_test_local_execution);
        assert!(!interrupts_enabled());
        drop(guard);
        assert!(!interrupts_enabled());
        drop(outer);
        assert!(interrupts_enabled());
    }

    #[test]
    fn lock_drop_preserves_an_initially_disabled_state() {
        let _serial = TEST_LOCK.lock().expect("zone spin test lock");
        let lock = SpinLock::new();
        set_interrupts_enabled(false);
        EXCLUSION_CALLS.store(0, Ordering::Release);

        let guard = lock.lock_with(exclude_test_local_execution);
        assert!(!interrupts_enabled());
        drop(guard);

        assert!(!interrupts_enabled());
        assert_eq!(EXCLUSION_CALLS.load(Ordering::Acquire), 1);
    }
}
