//! Reactor-owned deadline domain and compatibility registrar bridge.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tx_services::time::{
    DeadlineDomain, DeadlineNs, DeadlineRegistrarHandle, DeviceTimerCallback, TimeError, TimerRole,
    TimerTarget, TimerToken,
};
use tx_substrate::step::{DelegateTokenId, InterestMask, WaitSourceId};
use tx_substrate::sync::SpinMutex;
use tx_substrate::wake::mailbox::TaskMailbox;
use tx_time::{TimerEngine, TimerKey};

pub(crate) enum TimerRoute {
    Task {
        mailbox: alloc::sync::Weak<TaskMailbox>,
    },
    Signal {
        mailbox: alloc::sync::Weak<TaskMailbox>,
    },
    Delegate(DelegateTokenId),
    WaitSource {
        source: WaitSourceId,
        interests: InterestMask,
    },
    Device(DeviceTimerCallback),
}

struct RouteEntry {
    key: TimerKey,
    guard: tx_time::timer::TimerGuard,
    route: TimerRoute,
}

pub(crate) struct ExpiredTimer {
    pub(crate) key: TimerKey,
    pub(crate) route: TimerRoute,
}

pub(crate) struct ReactorTimerDomain {
    engine: TimerEngine,
    routes: SpinMutex<Vec<RouteEntry>>,
    registration_gate: SpinMutex<()>,
    driver_owner: AtomicUsize,
    deadline_changed: AtomicBool,
}

const DRIVER_FREE: usize = usize::MAX;

struct DriverClaim<'a> {
    owner: &'a AtomicUsize,
    token: usize,
}

impl Drop for DriverClaim<'_> {
    fn drop(&mut self) {
        self.owner
            .compare_exchange(
                self.token,
                DRIVER_FREE,
                Ordering::Release,
                Ordering::Relaxed,
            )
            .expect("timer driver claim was lost before release");
    }
}

impl ReactorTimerDomain {
    pub(crate) fn new() -> Self {
        Self {
            engine: TimerEngine::new(),
            routes: SpinMutex::new(Vec::new()),
            registration_gate: SpinMutex::new(()),
            driver_owner: AtomicUsize::new(DRIVER_FREE),
            deadline_changed: AtomicBool::new(false),
        }
    }

    pub(crate) fn registrar_handle(self: &Arc<Self>) -> DeadlineRegistrarHandle {
        DeadlineRegistrarHandle::from_domain(self.clone())
    }

    pub(crate) fn drain_due_for_owner(&self, owner: usize, now_ns: u64) -> Vec<ExpiredTimer> {
        let Some(_claim) = self.try_claim_driver(owner) else {
            return Vec::new();
        };
        let before = self.next_deadline_ns();
        let keys = {
            let _gate = self.registration_gate.lock();
            self.engine.drain_due_batch(now_ns)
        };
        let entries = self.take_routes(keys);
        let after = self.next_deadline_ns();
        self.mark_deadline_changed(before, after);
        entries
            .into_iter()
            .map(|RouteEntry { key, guard, route }| {
                drop(guard);
                ExpiredTimer { key, route }
            })
            .collect()
    }

    pub(crate) fn next_deadline_ns(&self) -> Option<u64> {
        self.engine.next_deadline_ns()
    }

    pub(crate) fn deadline_change_pending(&self) -> bool {
        self.deadline_changed.load(Ordering::Acquire)
    }

    pub(crate) fn consume_deadline_change(&self) -> bool {
        if !self.deadline_change_pending() {
            return false;
        }
        self.deadline_changed.swap(false, Ordering::AcqRel)
    }

    fn try_claim_driver(&self, owner: usize) -> Option<DriverClaim<'_>> {
        assert_ne!(
            owner, DRIVER_FREE,
            "timer driver owner uses the free sentinel"
        );
        self.driver_owner
            .compare_exchange(DRIVER_FREE, owner, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| DriverClaim {
                owner: &self.driver_owner,
                token: owner,
            })
    }

    fn take_routes(&self, keys: Vec<TimerKey>) -> Vec<RouteEntry> {
        let mut routes = self.routes.lock();
        keys.into_iter()
            .filter_map(|key| {
                routes
                    .iter()
                    .position(|entry| entry.key == key)
                    .map(|index| routes.swap_remove(index))
            })
            .collect()
    }

    fn mark_deadline_changed(&self, before: Option<u64>, after: Option<u64>) {
        if before != after {
            self.deadline_changed.store(true, Ordering::Release);
        }
    }
}

impl DeadlineDomain for ReactorTimerDomain {
    fn register_deadline(
        &self,
        deadline_ns: DeadlineNs,
        _role: TimerRole,
        target: TimerTarget,
    ) -> Result<TimerToken, TimeError> {
        let route = match target {
            TimerTarget::TaskMailbox(mailbox) => TimerRoute::Task { mailbox },
            TimerTarget::SignalTarget { mailbox } => TimerRoute::Signal { mailbox },
            TimerTarget::DelegateToken(token) => TimerRoute::Delegate(token),
            TimerTarget::WaitSource { source, interests } => {
                TimerRoute::WaitSource { source, interests }
            }
            TimerTarget::DeviceCallback(callback) => TimerRoute::Device(callback),
        };
        let _gate = self.registration_gate.lock();
        let before = self.next_deadline_ns();
        let guard = self.engine.insert(deadline_ns);
        let key = guard.key();
        self.routes.lock().push(RouteEntry { key, guard, route });
        let after = self.next_deadline_ns();
        self.mark_deadline_changed(before, after);
        Ok(TimerToken::new(key.raw()))
    }

    fn cancel_deadline(&self, token: TimerToken) -> bool {
        let _gate = self.registration_gate.lock();
        let before = self.next_deadline_ns();
        let key = TimerKey::new(token.raw());
        let removed = {
            let mut routes = self.routes.lock();
            routes
                .iter()
                .position(|entry| entry.key == key)
                .map(|index| routes.swap_remove(index))
        };
        if let Some(entry) = removed {
            let RouteEntry { guard, .. } = entry;
            drop(guard);
            let after = self.next_deadline_ns();
            self.mark_deadline_changed(before, after);
            true
        } else {
            false
        }
    }

    fn rearm_deadline(&self, token: TimerToken, deadline_ns: DeadlineNs) -> bool {
        let _gate = self.registration_gate.lock();
        let before = self.next_deadline_ns();
        let key = TimerKey::new(token.raw());
        let live = self.routes.lock().iter().any(|entry| entry.key == key);
        if !live || !self.engine.rearm(key, deadline_ns) {
            return false;
        }
        let after = self.next_deadline_ns();
        self.mark_deadline_changed(before, after);
        true
    }
}

impl Default for ReactorTimerDomain {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tx_services::time::{DeadlineRegistrar, TimerRole, TimerTarget};

    #[test]
    fn deadline_domain_has_no_legacy_wheel_bridge() {
        const SOURCE: &str = include_str!("deadline_registry.rs");

        assert!(!SOURCE.contains(concat!("Timer", "Wheel")));
        assert!(!SOURCE.contains(concat!("from_domain_with_", "legacy")));
    }

    #[test]
    fn dropping_domain_guard_cancels_route_and_engine_key() {
        let domain = Arc::new(ReactorTimerDomain::new());
        let registrar = domain.registrar_handle();
        let guard = registrar
            .register_deadline(
                DeadlineNs::new(10),
                TimerRole::DelegateTimeout,
                TimerTarget::DelegateToken(DelegateTokenId::new(7)),
            )
            .expect("domain registration");
        drop(guard);

        assert!(domain.drain_due_for_owner(0, 10).is_empty());
        assert_eq!(domain.next_deadline_ns(), None);
    }

    #[test]
    fn expired_key_is_removed_from_route_table_and_cannot_refire() {
        let domain = Arc::new(ReactorTimerDomain::new());
        let registrar = domain.registrar_handle();
        let guard = registrar
            .register_deadline(
                DeadlineNs::new(10),
                TimerRole::DelegateTimeout,
                TimerTarget::DelegateToken(DelegateTokenId::new(8)),
            )
            .expect("domain registration");
        let _token = guard.forget();

        let due = domain.drain_due_for_owner(0, 10);
        assert_eq!(due.len(), 1);
        assert!(
            matches!(due[0].route, TimerRoute::Delegate(token) if token == DelegateTokenId::new(8))
        );
        assert!(domain.drain_due_for_owner(0, 11).is_empty());
    }

    #[test]
    fn single_driver_claim_blocks_competing_owner_until_release() {
        let domain = ReactorTimerDomain::new();
        let first = domain.try_claim_driver(1).expect("first driver claim");
        assert!(domain.try_claim_driver(2).is_none());
        drop(first);
        assert!(domain.try_claim_driver(2).is_some());
    }

    #[test]
    fn earliest_deadline_changes_publish_and_consume_notification() {
        let domain = Arc::new(ReactorTimerDomain::new());
        let registrar = domain.registrar_handle();
        assert!(!domain.deadline_change_pending());
        let guard = registrar
            .register_deadline(
                DeadlineNs::new(20),
                TimerRole::DelegateTimeout,
                TimerTarget::DelegateToken(DelegateTokenId::new(9)),
            )
            .expect("domain registration");
        assert!(domain.deadline_change_pending());
        assert!(domain.consume_deadline_change());
        assert!(!domain.consume_deadline_change());
        drop(guard);
        assert!(domain.deadline_change_pending());
        assert!(domain.consume_deadline_change());
    }

    #[test]
    fn rearm_replaces_deadline_without_stale_delivery() {
        let domain = Arc::new(ReactorTimerDomain::new());
        let registrar = domain.registrar_handle();
        let mut guard = Some(
            registrar
                .register_deadline(
                    DeadlineNs::new(10),
                    TimerRole::DelegateTimeout,
                    TimerTarget::DelegateToken(DelegateTokenId::new(10)),
                )
                .expect("domain registration"),
        );
        registrar
            .rearm_deadline(
                &mut guard,
                DeadlineNs::new(30),
                TimerRole::DelegateTimeout,
                TimerTarget::DelegateToken(DelegateTokenId::new(10)),
            )
            .expect("domain rearm");

        assert!(domain.drain_due_for_owner(0, 10).is_empty());
        assert_eq!(domain.next_deadline_ns(), Some(30));
        assert_eq!(domain.drain_due_for_owner(0, 30).len(), 1);
        assert!(domain.drain_due_for_owner(0, 31).is_empty());
    }
}
