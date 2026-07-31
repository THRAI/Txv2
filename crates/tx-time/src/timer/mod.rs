//! Standalone opaque-key timer engine.

mod engine;
mod min_heap;
mod queue;

pub use engine::{TimerEngine, TimerGuard};

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::{DeadlineNs, TimerKey};

    use super::TimerEngine;

    #[test]
    fn earliest_order() {
        let engine = TimerEngine::new();
        let late = engine.insert(DeadlineNs::new(30));
        let early = engine.insert(DeadlineNs::new(10));
        let middle = engine.insert(DeadlineNs::new(20));
        let mut due = Vec::new();

        engine.drain_due(20, &mut due);

        assert_eq!(due, vec![early.key(), middle.key()]);
        assert_eq!(engine.next_deadline_ns(), Some(30));
        assert_ne!(late.key(), early.key());
    }

    #[test]
    fn equal_deadline_uses_deterministic_key_order() {
        let engine = TimerEngine::new();
        let first = engine.insert(DeadlineNs::new(10));
        let second = engine.insert(DeadlineNs::new(10));
        let third = engine.insert(DeadlineNs::new(10));
        let mut due = Vec::new();

        engine.drain_due(10, &mut due);

        assert_eq!(due, vec![first.key(), second.key(), third.key()]);
    }

    #[test]
    fn cancel_is_idempotent() {
        let engine = TimerEngine::new();
        let guard = engine.insert(DeadlineNs::new(10));
        let key = guard.key();
        let mut due = Vec::new();

        assert!(engine.cancel(key));
        assert!(!engine.cancel(key));
        engine.drain_due(10, &mut due);

        assert!(due.is_empty());
        assert!(!guard.cancel());
    }

    #[test]
    fn rearm_invalidates_old_expiry() {
        let engine = TimerEngine::new();
        let guard = engine.insert(DeadlineNs::new(10));
        let key = guard.key();
        let mut due = Vec::new();

        assert!(engine.rearm(key, DeadlineNs::new(20)));
        engine.drain_due(10, &mut due);
        assert!(due.is_empty());
        engine.drain_due(20, &mut due);

        assert_eq!(due, vec![key]);
    }

    #[test]
    fn rearm_rejects_cancelled_and_expired_keys() {
        let cancelled_engine = TimerEngine::new();
        let cancelled_guard = cancelled_engine.insert(DeadlineNs::new(10));
        let cancelled_key = cancelled_guard.key();
        let mut due = Vec::new();

        assert!(cancelled_engine.cancel(cancelled_key));
        assert!(!cancelled_engine.rearm(cancelled_key, DeadlineNs::new(20)));
        assert_eq!(cancelled_engine.next_deadline_ns(), None);
        cancelled_engine.drain_due(20, &mut due);
        assert!(due.is_empty());

        let expired_engine = TimerEngine::new();
        let expired_guard = expired_engine.insert(DeadlineNs::new(10));
        let expired_key = expired_guard.key();

        expired_engine.drain_due(10, &mut due);
        assert_eq!(due, vec![expired_key]);
        assert!(!expired_engine.rearm(expired_key, DeadlineNs::new(20)));
        assert_eq!(expired_engine.next_deadline_ns(), None);
        due.clear();
        expired_engine.drain_due(20, &mut due);
        assert!(due.is_empty());
    }

    #[test]
    fn key_exhaustion_does_not_wrap_or_reuse_a_live_key() {
        let engine = TimerEngine::with_next_key_for_test(u64::MAX);
        let live = engine.insert(DeadlineNs::new(10));
        let live_key = live.key();
        let mut due = Vec::new();

        assert_eq!(live_key.raw(), u64::MAX);
        let exhausted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.insert(DeadlineNs::new(20))
        }));
        assert!(exhausted.is_err());

        engine.drain_due(10, &mut due);
        assert_eq!(due, vec![live_key]);
    }

    #[test]
    fn growth_panic_restores_the_ready_state() {
        let engine = TimerEngine::with_growth_panic_for_test();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            engine.insert(DeadlineNs::new(10))
        }));
        assert!(panicked.is_err());

        let guard = engine.insert(DeadlineNs::new(20));
        let mut due = Vec::new();
        engine.drain_due(20, &mut due);

        assert_eq!(guard.key().raw(), 1);
        assert_eq!(due, vec![guard.key()]);
    }

    #[test]
    fn guard_drop_cancels() {
        let engine = TimerEngine::new();
        let key = {
            let guard = engine.insert(DeadlineNs::new(10));
            guard.key()
        };
        let mut due = Vec::new();

        engine.drain_due(10, &mut due);

        assert!(due.is_empty());
        assert!(!engine.cancel(key));
    }

    #[test]
    fn batch_drain_returns_only_keys() {
        let engine = TimerEngine::new();
        let first = engine.insert(DeadlineNs::new(10));
        let second = engine.insert(DeadlineNs::new(10));
        let _third = engine.insert(DeadlineNs::new(20));
        let mut due = Vec::new();

        engine.drain_due(10, &mut due);

        let _: Vec<TimerKey> = due.clone();
        assert_eq!(due, vec![first.key(), second.key()]);
        assert_eq!(engine.next_deadline_ns(), Some(20));
    }

    #[test]
    fn timer_guard_has_no_delivery_payload() {
        let engine = TimerEngine::new();
        let guard = engine.insert(DeadlineNs::new(10));

        let _: TimerKey = guard.key();
    }
}
