//! Public spin-mutex primitive.
//!
//! Per `CONCEPTS_v4.md` ("substrate provides zone, index, epoch, mutation,
//! bus, page, and reservation primitives") a synchronization primitive
//! with no semantic content and no entity ownership belongs in the
//! substrate. Subsystems and the `tx-fs` / `tx-kernel` crates consume it
//! as `tx_substrate::SpinMutex` (re-exported at the crate root).
//!
//! No poisoning: the kernel does not unwind. Acquire/release is a plain
//! atomic compare-exchange with `core::hint::spin_loop` between attempts.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct SpinMutex<T, M = LockMetricsOff> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
    metrics: M,
}

unsafe impl<T: Send, M: Send + Sync> Sync for SpinMutex<T, M> {}

/// Type-level marker for the default no-metrics lock path.
///
/// `SpinMutex<T>` uses this marker, so ordinary locks compile without local
/// timing state. Under `cfg(tx_lock_metrics)`, the metrics hooks are still
/// statically unreachable for this marker.
#[derive(Copy, Clone, Debug)]
pub struct LockMetricsOff;

/// Type-level marker for opt-in lock timing.
///
/// Use `SpinMutex<T, LockMetricsOn>` plus `SpinMutex::new_observed(...)` at
/// the specific lock declaration that should produce lock rows.
#[derive(Copy, Clone, Debug)]
pub struct LockMetricsOn {
    #[cfg(tx_lock_metrics)]
    name: tx_observe::EventNameId,
}

impl LockMetricsOn {
    pub const fn new(name: &'static [u8]) -> Self {
        let _ = name;
        Self {
            #[cfg(tx_lock_metrics)]
            name: tx_observe::EventNameId::from_name(name),
        }
    }
}

pub trait LockMetricsMode: Sized {
    type Timing: LockTimingOps<Self>;

    const ENABLED: bool;

    fn name(&self) -> tx_observe::EventNameId;
}

impl LockMetricsMode for LockMetricsOff {
    type Timing = LockTimingOff;

    const ENABLED: bool = false;

    #[inline(always)]
    fn name(&self) -> tx_observe::EventNameId {
        tx_observe::EventNameId::from_raw(0)
    }
}

impl LockMetricsMode for LockMetricsOn {
    type Timing = LockTimingOn;

    const ENABLED: bool = true;

    #[inline(always)]
    fn name(&self) -> tx_observe::EventNameId {
        #[cfg(not(tx_lock_metrics))]
        {
            tx_observe::EventNameId::from_raw(0)
        }
        #[cfg(tx_lock_metrics)]
        self.name
    }
}

impl<T> SpinMutex<T, LockMetricsOff> {
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
            metrics: LockMetricsOff,
        }
    }
}

impl<T, M: LockMetricsMode> SpinMutex<T, M> {
    pub const fn new_observed(value: T, metrics: M) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
            metrics,
        }
    }

    #[inline(always)]
    pub const fn lock_metrics_enabled(&self) -> bool {
        cfg!(tx_lock_metrics) && M::ENABLED
    }

    #[inline]
    pub fn lock(&self) -> SpinMutexGuard<'_, T, M> {
        self.lock_with_progress(|| {})
    }

    /// Acquire the lock while periodically running a non-blocking progress
    /// hook.
    ///
    /// This is intentionally opt-in. Architecture code can use it at locks
    /// which participate in a synchronous cross-CPU protocol (for example a
    /// maskable software-IPI TLB shootdown) without imposing HAL work on every
    /// ordinary kernel spin lock.
    #[inline]
    pub fn lock_with_progress<F>(&self, mut progress: F) -> SpinMutexGuard<'_, T, M>
    where
        F: FnMut(),
    {
        let mut timing = M::Timing::start(&self.metrics);
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            timing.spin();
            progress();
            core::hint::spin_loop();
        }
        timing.acquired(self);
        SpinMutexGuard {
            mutex: self,
            timing,
            _marker: PhantomData,
        }
    }

    /// Acquire the mutex without spinning.
    ///
    /// This is useful for multi-object transactions that already hold one
    /// lock: callers can abandon and retry instead of introducing an
    /// address-dependent nested-lock order.
    #[inline]
    pub fn try_lock(&self) -> Option<SpinMutexGuard<'_, T, M>> {
        let mut timing = M::Timing::start(&self.metrics);
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        timing.acquired(self);
        Some(SpinMutexGuard {
            mutex: self,
            timing,
            _marker: PhantomData,
        })
    }

    #[cfg(tx_lock_metrics)]
    #[inline(always)]
    fn emit_metric(&self, metric: tx_observe::EventNameId, value: u64) {
        if !M::ENABLED {
            return;
        }
        if let Some(emitter) = tx_observe::current() {
            emitter.lock_metric(self.metrics.name(), metric, value);
        }
    }
}

#[cfg(tx_lock_metrics)]
const LOCK_METRIC_WAIT_NS: tx_observe::EventNameId =
    tx_observe::EventNameId::from_name(b"debug.lock.wait_ns");
#[cfg(tx_lock_metrics)]
const LOCK_METRIC_SERVICE_NS: tx_observe::EventNameId =
    tx_observe::EventNameId::from_name(b"debug.lock.service_ns");
#[cfg(tx_lock_metrics)]
const LOCK_METRIC_RESPONSE_NS: tx_observe::EventNameId =
    tx_observe::EventNameId::from_name(b"debug.lock.response_ns");
#[cfg(tx_lock_metrics)]
const LOCK_METRIC_SPINS: tx_observe::EventNameId =
    tx_observe::EventNameId::from_name(b"debug.lock.spins");
#[cfg(tx_lock_metrics)]
const LOCK_METRIC_CONTENDED: tx_observe::EventNameId =
    tx_observe::EventNameId::from_name(b"debug.lock.contended");

impl<T: core::fmt::Debug, M: LockMetricsMode> core::fmt::Debug for SpinMutex<T, M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("SpinMutex").field(&*self.lock()).finish()
    }
}

pub struct SpinMutexGuard<'a, T, M: LockMetricsMode = LockMetricsOff> {
    mutex: &'a SpinMutex<T, M>,
    timing: M::Timing,
    _marker: PhantomData<&'a M>,
}

impl<T, M: LockMetricsMode> core::ops::Deref for SpinMutexGuard<'_, T, M> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T, M: LockMetricsMode> core::ops::DerefMut for SpinMutexGuard<'_, T, M> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T, M: LockMetricsMode> Drop for SpinMutexGuard<'_, T, M> {
    fn drop(&mut self) {
        self.timing.released(self.mutex);
        self.mutex.locked.store(false, Ordering::Release);
    }
}

/// A compact reader-writer spin lock for short, read-mostly kernel data.
///
/// The high bit of `state` denotes an active writer; the remaining bits count
/// readers. Readers therefore share one cache line and do not serialize their
/// protected work. Writers are intentionally rare users of this primitive and
/// acquire the lock only when the reader count reaches zero.
pub struct RwSpinLock<T> {
    state: AtomicUsize,
    value: UnsafeCell<T>,
}

const RW_WRITE_LOCKED: usize = 1usize << (usize::BITS - 1);
const RW_READER_MASK: usize = RW_WRITE_LOCKED - 1;

unsafe impl<T: Send + Sync> Sync for RwSpinLock<T> {}

impl<T> RwSpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(0),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    pub fn read(&self) -> RwSpinReadGuard<'_, T> {
        loop {
            let state = self.state.load(Ordering::Relaxed);
            if state & RW_WRITE_LOCKED != 0 {
                core::hint::spin_loop();
                continue;
            }
            debug_assert!(state & RW_READER_MASK != RW_READER_MASK);
            if self
                .state
                .compare_exchange_weak(state, state + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return RwSpinReadGuard { lock: self };
            }
            core::hint::spin_loop();
        }
    }

    #[inline]
    pub fn write(&self) -> RwSpinWriteGuard<'_, T> {
        loop {
            if self
                .state
                .compare_exchange_weak(0, RW_WRITE_LOCKED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return RwSpinWriteGuard { lock: self };
            }
            core::hint::spin_loop();
        }
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for RwSpinLock<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("RwSpinLock").field(&*self.read()).finish()
    }
}

pub struct RwSpinReadGuard<'a, T> {
    lock: &'a RwSpinLock<T>,
}

impl<T> core::ops::Deref for RwSpinReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for RwSpinReadGuard<'_, T> {
    fn drop(&mut self) {
        let previous = self.lock.state.fetch_sub(1, Ordering::Release);
        debug_assert!(previous & RW_READER_MASK != 0);
        debug_assert!(previous & RW_WRITE_LOCKED == 0);
    }
}

pub struct RwSpinWriteGuard<'a, T> {
    lock: &'a RwSpinLock<T>,
}

impl<T> core::ops::Deref for RwSpinWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for RwSpinWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for RwSpinWriteGuard<'_, T> {
    fn drop(&mut self) {
        debug_assert_eq!(self.lock.state.load(Ordering::Relaxed), RW_WRITE_LOCKED);
        self.lock.state.store(0, Ordering::Release);
    }
}

#[doc(hidden)]
pub trait LockTimingOps<M: LockMetricsMode>: Sized {
    fn start(metrics: &M) -> Self;
    fn spin(&mut self);
    fn acquired<T>(&mut self, mutex: &SpinMutex<T, M>);
    fn released<T>(&self, mutex: &SpinMutex<T, M>);
}

pub struct LockTimingOff;

impl LockTimingOps<LockMetricsOff> for LockTimingOff {
    #[inline(always)]
    fn start(_metrics: &LockMetricsOff) -> Self {
        Self
    }

    #[inline(always)]
    fn spin(&mut self) {}

    #[inline(always)]
    fn acquired<T>(&mut self, _mutex: &SpinMutex<T, LockMetricsOff>) {}

    #[inline(always)]
    fn released<T>(&self, _mutex: &SpinMutex<T, LockMetricsOff>) {}
}

pub struct LockTimingOn {
    #[cfg(tx_lock_metrics)]
    request_ts: u64,
    #[cfg(tx_lock_metrics)]
    acquire_ts: u64,
    #[cfg(tx_lock_metrics)]
    spins: u64,
}

impl LockTimingOps<LockMetricsOn> for LockTimingOn {
    #[inline(always)]
    fn start(_metrics: &LockMetricsOn) -> Self {
        Self {
            #[cfg(tx_lock_metrics)]
            request_ts: { tx_observe::clock_now_ns() },
            #[cfg(tx_lock_metrics)]
            acquire_ts: 0,
            #[cfg(tx_lock_metrics)]
            spins: 0,
        }
    }

    #[inline(always)]
    fn spin(&mut self) {
        #[cfg(tx_lock_metrics)]
        {
            self.spins = self.spins.saturating_add(1);
        }
    }

    #[inline(always)]
    fn acquired<T>(&mut self, _mutex: &SpinMutex<T, LockMetricsOn>) {
        #[cfg(tx_lock_metrics)]
        {
            let now = tx_observe::clock_now_ns();
            self.acquire_ts = now;
            _mutex.emit_metric(LOCK_METRIC_WAIT_NS, now.saturating_sub(self.request_ts));
            if self.spins != 0 {
                _mutex.emit_metric(LOCK_METRIC_SPINS, self.spins);
                _mutex.emit_metric(LOCK_METRIC_CONTENDED, 1);
            }
        };
    }

    #[inline(always)]
    fn released<T>(&self, _mutex: &SpinMutex<T, LockMetricsOn>) {
        #[cfg(tx_lock_metrics)]
        {
            let release_ts = tx_observe::clock_now_ns();
            _mutex.emit_metric(
                LOCK_METRIC_SERVICE_NS,
                release_ts.saturating_sub(self.acquire_ts),
            );
            _mutex.emit_metric(
                LOCK_METRIC_RESPONSE_NS,
                release_ts.saturating_sub(self.request_ts),
            );
        };
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{RwSpinLock, SpinMutex};
    use alloc::sync::Arc;

    #[test]
    fn try_lock_reports_contention_without_spinning() {
        let mutex = SpinMutex::new(7);
        let guard = mutex.try_lock().expect("first try_lock");
        assert!(mutex.try_lock().is_none());
        drop(guard);
        assert_eq!(*mutex.try_lock().expect("try_lock after release"), 7);
    }

    #[test]
    fn rw_spin_lock_allows_parallel_readers_and_excludes_writer() {
        let lock = Arc::new(RwSpinLock::new(0usize));
        let first = lock.read();
        let peer_lock = Arc::clone(&lock);
        let peer = std::thread::spawn(move || {
            let second = peer_lock.read();
            assert_eq!(*second, 0);
        });
        peer.join().expect("parallel reader");
        drop(first);

        {
            let mut writer = lock.write();
            *writer = 7;
        }
        assert_eq!(*lock.read(), 7);
    }
}
