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

type SpinProgressFn = fn();

static PLATFORM_SPIN_PROGRESS: AtomicUsize = AtomicUsize::new(0);

/// Install the selected platform's lock-free spin progress callback.
///
/// Substrate initialization calls this once before secondary CPUs are brought
/// online. Repeating the installation for the same platform is harmless; a
/// different callback is rejected because changing the selected platform at
/// runtime would violate the static HAL model.
#[doc(hidden)]
pub fn install_platform_spin_progress<P: tx_hal::PmapIf>() {
    let callback = P::service_pending_tlb_shootdown as usize;
    match PLATFORM_SPIN_PROGRESS.compare_exchange(0, callback, Ordering::Release, Ordering::Acquire)
    {
        Ok(_) => {}
        Err(installed) => assert_eq!(
            installed, callback,
            "tx-substrate platform spin progress callback changed after installation"
        ),
    }
}

#[inline(always)]
fn platform_spin_progress() {
    let callback = PLATFORM_SPIN_PROGRESS.load(Ordering::Acquire);
    if callback == 0 {
        return;
    }

    // SAFETY: `install_platform_spin_progress` stores only a `fn()` pointer,
    // and the one-time installation keeps that pointer valid for the kernel's
    // lifetime.
    let callback: SpinProgressFn = unsafe { core::mem::transmute(callback) };
    callback();
}

/// Local state for a contended spin wait.
///
/// Keep one value per acquisition loop. [`Self::tick`] provides the selected
/// platform's lock-free progress point, while [`Self::tick_with`] lets an
/// existing architecture-specific wait use the same bounded cadence.
#[derive(Clone, Copy, Debug, Default)]
pub struct SpinWait {
    wait: tx_hal::TlbProgressSpinWait,
}

impl SpinWait {
    pub const fn new() -> Self {
        Self {
            wait: tx_hal::TlbProgressSpinWait::new(),
        }
    }

    /// Record one failed wait and periodically run platform progress.
    #[inline(always)]
    pub fn tick(&mut self) {
        self.tick_with(platform_spin_progress);
    }

    /// Record one failed wait and periodically run `progress`.
    ///
    /// The callback runs on the first failed wait and then at most once per 64
    /// failures. It must not allocate, block, or acquire the lock being waited
    /// on. The processor hint still runs on every failed attempt.
    #[inline(always)]
    pub fn tick_with<F>(&mut self, mut progress: F)
    where
        F: FnMut(),
    {
        self.wait.spin_with(&mut progress);
    }
}

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
        let mut timing = M::Timing::start(&self.metrics);
        let mut wait = SpinWait::new();
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            timing.spin();
            wait.tick();
        }
        timing.acquired(self);
        SpinMutexGuard {
            mutex: self,
            timing,
            _marker: PhantomData,
        }
    }

    /// Acquire the lock while periodically running a non-blocking progress
    /// hook.
    ///
    /// Architecture code can use this at waits which already own their
    /// progress operation. Ordinary [`Self::lock`] acquisitions use the
    /// platform callback installed during substrate initialization.
    #[inline]
    pub fn lock_with_progress<F>(&self, mut progress: F) -> SpinMutexGuard<'_, T, M>
    where
        F: FnMut(),
    {
        let mut timing = M::Timing::start(&self.metrics);
        let mut wait = SpinWait::new();
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            timing.spin();
            wait.tick_with(&mut progress);
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
    use super::SpinMutex;

    #[test]
    fn try_lock_reports_contention_without_spinning() {
        let mutex = SpinMutex::new(7);
        let guard = mutex.try_lock().expect("first try_lock");
        assert!(mutex.try_lock().is_none());
        drop(guard);
        assert_eq!(*mutex.try_lock().expect("try_lock after release"), 7);
    }
}
