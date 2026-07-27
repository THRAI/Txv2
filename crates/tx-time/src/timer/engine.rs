use alloc::sync::Arc;
use alloc::vec::Vec;

use tx_substrate::SpinMutex;

use crate::{DeadlineNs, TimerKey};

use super::min_heap::MinHeap;
use super::queue::TimerQueue;

struct TimerState {
    next_key: Option<u64>,
    mutation: u64,
    queue: MinHeap,
    #[cfg(test)]
    panic_during_growth: bool,
}

enum EngineState {
    Ready(TimerState),
    Growing,
}

/// A standalone timer registry that returns opaque keys on expiry.
#[derive(Clone)]
pub struct TimerEngine {
    state: Arc<SpinMutex<EngineState>>,
}

impl TimerEngine {
    pub fn new() -> Self {
        Self::with_next_key(1)
    }

    fn with_next_key(next_key: u64) -> Self {
        Self {
            state: Arc::new(SpinMutex::new(EngineState::Ready(TimerState {
                next_key: Some(next_key),
                mutation: 0,
                queue: MinHeap::new(),
                #[cfg(test)]
                panic_during_growth: false,
            }))),
        }
    }

    #[cfg(test)]
    pub(super) fn with_next_key_for_test(next_key: u64) -> Self {
        Self::with_next_key(next_key)
    }

    #[cfg(test)]
    pub(super) fn with_growth_panic_for_test() -> Self {
        let engine = Self::new();
        with_ready_state(&engine.state, |state| state.panic_during_growth = true);
        engine
    }

    /// Register a deadline and return its cancellation capability.
    ///
    /// Panics when the non-reused key space is exhausted; allocation never
    /// wraps to a stale or live key.
    pub fn insert(&self, deadline: DeadlineNs) -> TimerGuard {
        loop {
            let state_to_grow = {
                let mut shared = self.state.lock();
                if let EngineState::Ready(state) = &mut *shared {
                    if state.queue.has_insert_capacity() {
                        let raw = state.next_key.expect("timer key space exhausted");
                        state.next_key = raw.checked_add(1);
                        let key = TimerKey::new(raw);
                        state.queue.insert(key, deadline.raw());
                        state.bump_mutation();
                        return TimerGuard {
                            state: Arc::clone(&self.state),
                            key,
                        };
                    }

                    match core::mem::replace(&mut *shared, EngineState::Growing) {
                        EngineState::Ready(state) => Some(state),
                        EngineState::Growing => unreachable!("ready state replaced with itself"),
                    }
                } else {
                    None
                }
            };

            if let Some(state) = state_to_grow {
                // Heap allocation happens after the spin lock is released.
                let mut restoring = GrowingStateRestore::new(&self.state, state);
                #[cfg(test)]
                if restoring.state_mut().take_growth_panic() {
                    panic!("timer test growth panic");
                }
                restoring.state_mut().queue.reserve_for_insert();
                restoring.restore();
            } else {
                core::hint::spin_loop();
            }
        }
    }

    /// Cancel a live key. Repeated cancellation returns `false`.
    pub fn cancel(&self, key: TimerKey) -> bool {
        with_ready_state(&self.state, |state| {
            let removed = state.queue.remove(key);
            if removed {
                state.bump_mutation();
            }
            removed
        })
    }

    /// Move a live key to a new deadline. A former expiry is invalidated.
    pub fn rearm(&self, key: TimerKey, deadline: DeadlineNs) -> bool {
        with_ready_state(&self.state, |state| {
            let rearmed = state.queue.rearm(key, deadline.raw());
            if rearmed {
                state.bump_mutation();
            }
            rearmed
        })
    }

    /// Remove all keys due at or before `now_ns` into caller-owned storage.
    pub fn drain_due(&self, now_ns: u64, out: &mut Vec<TimerKey>) {
        loop {
            let (mutation, due_count) = with_ready_state(&self.state, |state| {
                (state.mutation, state.queue.due_count(now_ns))
            });
            if due_count == 0 {
                return;
            }

            // Reserving may allocate, so it must happen without the spin lock.
            out.reserve(due_count);
            let drained = with_ready_state(&self.state, |state| {
                if state.mutation != mutation {
                    return false;
                }
                assert!(
                    out.capacity() - out.len() >= due_count,
                    "timer due output capacity was not prepared"
                );
                state.queue.drain_due(now_ns, out);
                state.bump_mutation();
                true
            });
            if drained {
                return;
            }
        }
    }

    /// Remove due keys into a fresh batch.
    pub fn drain_due_batch(&self, now_ns: u64) -> Vec<TimerKey> {
        let mut out = Vec::new();
        self.drain_due(now_ns, &mut out);
        out
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        with_ready_state(&self.state, |state| state.queue.next_deadline_ns())
    }
}

impl Default for TimerEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII cancellation capability for one [`TimerEngine`] key.
#[must_use = "drop the guard to cancel the timer"]
pub struct TimerGuard {
    state: Arc<SpinMutex<EngineState>>,
    key: TimerKey,
}

impl TimerGuard {
    pub const fn key(&self) -> TimerKey {
        self.key
    }

    pub fn cancel(&self) -> bool {
        cancel_key(&self.state, self.key)
    }

    pub fn rearm(&self, deadline: DeadlineNs) -> bool {
        rearm_key(&self.state, self.key, deadline)
    }
}

impl Drop for TimerGuard {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

impl TimerState {
    fn bump_mutation(&mut self) {
        self.mutation = self
            .mutation
            .checked_add(1)
            .expect("timer mutation counter exhausted");
    }

    #[cfg(test)]
    fn take_growth_panic(&mut self) -> bool {
        core::mem::take(&mut self.panic_during_growth)
    }
}

struct GrowingStateRestore<'a> {
    shared: &'a Arc<SpinMutex<EngineState>>,
    state: Option<TimerState>,
}

impl<'a> GrowingStateRestore<'a> {
    fn new(shared: &'a Arc<SpinMutex<EngineState>>, state: TimerState) -> Self {
        Self {
            shared,
            state: Some(state),
        }
    }

    fn state_mut(&mut self) -> &mut TimerState {
        self.state
            .as_mut()
            .expect("growing timer state was already restored")
    }

    fn restore(mut self) {
        let state = self
            .state
            .take()
            .expect("growing timer state was already restored");
        restore_ready_state(self.shared, state);
    }
}

impl Drop for GrowingStateRestore<'_> {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            restore_ready_state(self.shared, state);
        }
    }
}

fn restore_ready_state(shared: &Arc<SpinMutex<EngineState>>, state: TimerState) {
    let mut shared = shared.lock();
    if matches!(*shared, EngineState::Growing) {
        *shared = EngineState::Ready(state);
    }
}

fn with_ready_state<R>(
    shared: &Arc<SpinMutex<EngineState>>,
    operation: impl FnOnce(&mut TimerState) -> R,
) -> R {
    let mut operation = Some(operation);
    loop {
        let mut shared = shared.lock();
        if let EngineState::Ready(state) = &mut *shared {
            return operation.take().expect("timer operation retried")(state);
        }
        drop(shared);
        core::hint::spin_loop();
    }
}

fn cancel_key(shared: &Arc<SpinMutex<EngineState>>, key: TimerKey) -> bool {
    with_ready_state(shared, |state| {
        let removed = state.queue.remove(key);
        if removed {
            state.bump_mutation();
        }
        removed
    })
}

fn rearm_key(shared: &Arc<SpinMutex<EngineState>>, key: TimerKey, deadline: DeadlineNs) -> bool {
    with_ready_state(shared, |state| {
        let rearmed = state.queue.rearm(key, deadline.raw());
        if rearmed {
            state.bump_mutation();
        }
        rearmed
    })
}
