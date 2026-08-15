//! PR-3B wake-substrate: `WaitSource` — object-owned wait publication.
//!
//! Per [`docs/progress/decisions/2026-05-11-pr-3-wake-substrate-shape.md`]:
//! semantic objects (pipes, futexes, exit channels) own a `WaitSource`.
//! Subscribers register their [`crate::wake::mailbox::TaskMailbox`]
//! (via a `Weak` handle) together with the generation captured at
//! registration time. `notify(mask)` iterates subscribers and posts
//! [`crate::wake::mailbox::MailboxEvent::SourceFired`] events to each
//! live mailbox, scoped by the subscriber's stored generation.
//!
//! ## Layering
//!
//! - **Driver-local** active suspension state lives in
//!   [`crate::wake::mailbox::ActiveWait`] (PR-3A).
//! - **Task-local** wake delivery + generation lives in
//!   [`crate::wake::mailbox::TaskMailbox`] (PR-3A).
//! - **Object-local** wait publication lives here, in `WaitSource`.
//! - The reactor's existing `Channel` continues to drive production
//!   waker plumbing during the PR-3D coexistence window; production
//!   wait sites migrate to `WaitSource.register_prepared` /
//!   `WaitRegistrationGuard`, and the remaining direct `Waker`
//!   registration sites retire as bus migration proceeds.
//!
//! ## Why subscribers hold `Weak<TaskMailbox>`
//!
//! A `WaitSource` may outlive any individual task. Holding strong
//! references would invert the ownership graph (sources retain
//! tasks). `Weak` lets dead-task subscribers be skipped at notify
//! time, with later cleanup compacting the subscriber list.
//!
//! In the eventual zone-allocated shape (PR-3D+), `Weak<TaskMailbox>`
//! becomes `tx_substrate::zone::Weak<TaskMailbox>` (epoch-protected,
//! cap-typed). The shape stays — only the substrate primitive
//! changes.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::step::{InterestMask, WaitSourceId};
use crate::SpinMutex;

use crate::wake::mailbox::{MailboxEvent, MailboxSchedulerHint, TaskMailbox, WaitGeneration};

/// Per-subscriber bookkeeping. The mailbox handle is `Weak` so a
/// `WaitSource` does not retain dead tasks; `notify` skips and
/// later compacts dead subscribers.
struct Subscriber {
    mailbox: Weak<TaskMailbox>,
    generation: WaitGeneration,
    interests: InterestMask,
    /// Opaque per-subscriber id used by [`WaitSource::unregister`] to
    /// remove this row in O(n). The id is monotonic per-source and
    /// is never reused, so an already-unregistered handle becomes a
    /// no-op on second call rather than removing a fresh subscriber
    /// that happens to occupy the same slot.
    id: SubscriberId,
}

/// Opaque registration handle returned by [`WaitSource::register`].
/// Pass it to [`WaitSource::unregister`] to detach the subscription.
///
/// PR-3C upgrades this into a RAII `WaitRegistrationGuard` that
/// unregisters on drop and threads a `PreparedWaitRegistration`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SubscriberId(u64);

impl SubscriberId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// One subscriber row copied for an opt-in kernel stall report.
///
/// This surface is intentionally read-only and allocation-backed: production
/// notify/register paths never call it.  It lets a stalled userspace task be
/// connected to the exact object-owned wait source without exposing the
/// subscriber table itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaitSubscriberDiagnostic {
    pub source: WaitSourceId,
    pub source_pending_mask: u64,
    pub source_subscribers: usize,
    pub task_id_low: u32,
    pub generation: WaitGeneration,
    pub interests: InterestMask,
    pub mailbox_len: usize,
    pub mailbox_has_waker: bool,
}

/// Object-owned wait publication point.
/// Each semantic object that may have blocked operations retrying on
/// state transition owns one `WaitSource`. Subscribers register via
/// [`Self::register`]; state transitions call [`Self::notify`] to
/// post matching `MailboxEvent::SourceFired` events.
pub struct WaitSource {
    id: WaitSourceId,
    subscribers: SpinMutex<Vec<Subscriber>>,
    next_subscriber_id: AtomicU64,
    pending_mask: AtomicU64,
}

impl WaitSource {
    pub fn new(id: WaitSourceId) -> Self {
        Self {
            id,
            subscribers: SpinMutex::new(Vec::new()),
            // 1-based; `SubscriberId(0)` is a never-issued sentinel.
            next_subscriber_id: AtomicU64::new(1),
            pending_mask: AtomicU64::new(0),
        }
    }

    pub fn id(&self) -> WaitSourceId {
        self.id
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().len()
    }

    pub fn subscriber_diagnostics_for_task(
        &self,
        task_id_low: u32,
    ) -> Vec<WaitSubscriberDiagnostic> {
        self.subscriber_diagnostics()
            .into_iter()
            .filter(|subscriber| subscriber.task_id_low == task_id_low)
            .collect()
    }

    pub fn subscriber_diagnostics(&self) -> Vec<WaitSubscriberDiagnostic> {
        let subscribers = self.subscribers.lock();
        let source_subscribers = subscribers.len();
        let source_pending_mask = self.pending_mask.load(Ordering::Acquire);
        subscribers
            .iter()
            .filter_map(|subscriber| {
                let mailbox = subscriber.mailbox.upgrade()?;
                Some(WaitSubscriberDiagnostic {
                    source: self.id,
                    source_pending_mask,
                    source_subscribers,
                    task_id_low: mailbox.task_id_low(),
                    generation: subscriber.generation,
                    interests: subscriber.interests,
                    mailbox_len: mailbox.len(),
                    mailbox_has_waker: mailbox.has_waker(),
                })
            })
            .collect()
    }

    /// Snapshot latched readiness bits for opt-in kernel diagnostics.
    pub fn pending_mask_snapshot(&self) -> u64 {
        self.pending_mask.load(Ordering::Acquire)
    }

    /// Register `mailbox` as a subscriber. The caller must pass the
    /// `generation` it captured via
    /// [`TaskMailbox::next_generation`](crate::wake::mailbox::TaskMailbox::next_generation)
    /// at the moment of wait installation, plus the bitmask of
    /// interests it cares about.
    ///
    /// Returns a [`SubscriberId`] handle the caller passes to
    /// [`Self::unregister`] when the wait completes (timeout,
    /// interrupt, semantic predicate passes, …).
    pub fn register(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
        interests: InterestMask,
    ) -> SubscriberId {
        self.register_if(mailbox, generation, interests, || true)
            .expect("unconditional wait-source registration")
    }

    /// Recheck the blocked predicate and publish the subscriber row under the
    /// same source lock used by `notify`.
    ///
    /// This is the linearization point required by WAIT-2/SCHED-SMP-8.  The
    /// previous `install_if` implementation evaluated the predicate first and
    /// acquired `subscribers` only afterwards, leaving a producer notification
    /// able to land in between and be lost.
    fn register_if<F>(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
        interests: InterestMask,
        predicate: F,
    ) -> Option<SubscriberId>
    where
        F: FnOnce() -> bool,
    {
        let id = SubscriberId(self.next_subscriber_id.fetch_add(1, Ordering::AcqRel));
        let mailbox_for_pending = mailbox.clone();
        {
            let mut subscribers = self.subscribers.lock();
            if !predicate() {
                return None;
            }
            subscribers.push(Subscriber {
                mailbox,
                generation,
                interests,
                id,
            });
        }
        self.deliver_pending_to(mailbox_for_pending, generation, interests);
        Some(id)
    }

    /// Detach a subscription previously created by [`Self::register`].
    /// Idempotent: passing a handle whose row has already been
    /// removed (or never existed) is a no-op.
    pub fn unregister(&self, id: SubscriberId) {
        self.subscribers.lock().retain(|s| s.id != id);
    }

    /// Build a [`PreparedWaitRegistration`] without yet adding the
    /// subscriber row. The caller invokes
    /// [`PreparedWaitRegistration::install_if`] with a predicate
    /// that re-tests the blocked condition; if the predicate
    /// returns `true`, the registration is committed and a
    /// [`WaitRegistrationGuard`] is returned.
    ///
    /// This is the **lost-wake fix** primitive: production callers
    /// observe "blocked", construct a prepared registration, then
    /// re-test the predicate under the source's commit lock so a
    /// firing thread is forced to either (a) see the registration
    /// and post an event, or (b) lose the commit-lock race so the
    /// caller's re-test returns "still blocked." Either way the
    /// task is correctly woken.
    ///
    /// PR-3C provides the type-level surface; PR-3D wires it into
    /// the 92 production waker sites.
    pub fn prepare(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
        interests: InterestMask,
    ) -> PreparedWaitRegistration<'_> {
        PreparedWaitRegistration {
            source: self,
            mailbox,
            generation,
            interests,
        }
    }

    /// Fire `mask` on this source. Iterates the subscriber list and
    /// posts a `MailboxEvent::SourceFired` event to each live mailbox
    /// whose stored interests overlap `mask`. Dead subscribers
    /// (whose `Weak<TaskMailbox>` upgrades fail) are compacted out
    /// of the list as a side effect.
    ///
    /// Returns the number of events successfully posted.
    pub fn notify(&self, mask: InterestMask) -> usize {
        self.notify_with_hint(mask, MailboxSchedulerHint::Normal)
    }

    /// Fire `mask` with an explicit scheduler hint for delivered events.
    pub fn notify_with_hint(&self, mask: InterestMask, hint: MailboxSchedulerHint) -> usize {
        self.pending_mask.fetch_or(mask.raw(), Ordering::AcqRel);
        let mut subs = self.subscribers.lock();
        let mut posted = 0usize;
        // Walk forward and use `retain_mut` semantics: drop dead
        // subscribers, deliver to live ones with overlapping interest.
        subs.retain(|sub| {
            let Some(mailbox) = sub.mailbox.upgrade() else {
                // Subscriber's task is gone; compact out.
                return false;
            };
            let overlap = sub.interests.raw() & mask.raw();
            if overlap != 0 {
                let evt = MailboxEvent::SourceFired {
                    generation: sub.generation,
                    source: self.id,
                    interests: InterestMask::new(overlap),
                };
                if mailbox.post_with_scheduler_hint(evt, hint) {
                    posted += 1;
                }
                // Note: on overflow the mailbox latches its flag;
                // the subscriber stays registered.
            }
            true
        });
        posted
    }

    /// Fire `mask` with an explicit caller-provided mailbox post operation.
    ///
    /// This is the owner-aware routing hook used by reactor contexts that can
    /// both publish `SourceFired` and immediately route the mailbox owner
    /// through scheduler placement. Plain semantic producers should keep using
    /// [`Self::notify`] / [`Self::notify_with_hint`].
    pub fn notify_with_owner_post<F>(
        &self,
        mask: InterestMask,
        hint: MailboxSchedulerHint,
        mut post: F,
    ) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        self.pending_mask.fetch_or(mask.raw(), Ordering::AcqRel);
        let deliveries = {
            let mut subs = self.subscribers.lock();
            let mut deliveries = Vec::new();
            subs.retain(|sub| {
                let Some(mailbox) = sub.mailbox.upgrade() else {
                    return false;
                };
                let overlap = sub.interests.raw() & mask.raw();
                if overlap != 0 {
                    deliveries.push((mailbox, sub.generation, InterestMask::new(overlap)));
                }
                true
            });
            deliveries
        };

        let mut posted = 0usize;
        for (mailbox, generation, interests) in deliveries {
            let evt = MailboxEvent::SourceFired {
                generation,
                source: self.id,
                interests,
            };
            if post(&mailbox, evt, hint) {
                posted += 1;
            }
        }
        posted
    }

    /// Fire `mask` on this source, posting to at most `limit` live
    /// matching subscribers. If no live subscriber receives the event,
    /// keep the mask pending so a waiter that already published the
    /// semantic wait but has not yet committed its mailbox registration
    /// can still observe the wake.
    pub fn notify_limit(&self, mask: InterestMask, limit: usize) -> usize {
        self.notify_limit_with_hint(mask, limit, MailboxSchedulerHint::Normal)
    }

    /// Fire `mask` with an explicit scheduler hint, posting to at most
    /// `limit` live matching subscribers.
    pub fn notify_limit_with_hint(
        &self,
        mask: InterestMask,
        limit: usize,
        hint: MailboxSchedulerHint,
    ) -> usize {
        if limit == 0 || mask.raw() == 0 {
            return 0;
        }
        let mut subs = self.subscribers.lock();
        let mut posted = 0usize;
        subs.retain(|sub| {
            let Some(mailbox) = sub.mailbox.upgrade() else {
                return false;
            };
            let overlap = sub.interests.raw() & mask.raw();
            if overlap != 0 && posted < limit {
                let evt = MailboxEvent::SourceFired {
                    generation: sub.generation,
                    source: self.id,
                    interests: InterestMask::new(overlap),
                };
                if mailbox.post_with_scheduler_hint(evt, hint) {
                    posted += 1;
                }
            }
            true
        });
        if posted == 0 {
            self.pending_mask.fetch_or(mask.raw(), Ordering::AcqRel);
        }
        posted
    }

    /// Fire `mask` on this source, posting to at most `limit` live matching
    /// subscribers, and emit one `WaitSourceNotify` observation record per
    /// delivered mailbox event.
    pub fn notify_limit_emit(&self, mask: InterestMask, limit: usize) -> usize {
        self.notify_limit_emit_with_hint(mask, limit, MailboxSchedulerHint::Normal)
    }

    /// Fire `mask` with an explicit scheduler hint, posting to at most
    /// `limit` live matching subscribers and emitting one observation record
    /// per delivered mailbox event.
    pub fn notify_limit_emit_with_hint(
        &self,
        mask: InterestMask,
        limit: usize,
        hint: MailboxSchedulerHint,
    ) -> usize {
        if limit == 0 || mask.raw() == 0 {
            return 0;
        }
        let mut subs = self.subscribers.lock();
        let mut posted = 0usize;
        subs.retain(|sub| {
            let Some(mailbox) = sub.mailbox.upgrade() else {
                return false;
            };
            let overlap = sub.interests.raw() & mask.raw();
            if overlap != 0 && posted < limit {
                let evt = MailboxEvent::SourceFired {
                    generation: sub.generation,
                    source: self.id,
                    interests: InterestMask::new(overlap),
                };
                if mailbox.post_with_scheduler_hint(evt, hint) {
                    posted += 1;
                    if let Some(em) = tx_observe::current() {
                        em.wait_source_notify(
                            self.id.raw() as u32,
                            overlap as u32,
                            mailbox.task_id_low(),
                            sub.generation.raw() as u32,
                        );
                    }
                }
            }
            true
        });
        if posted == 0 {
            self.pending_mask.fetch_or(mask.raw(), Ordering::AcqRel);
        }
        posted
    }

    /// Fire `mask` with an explicit caller-provided mailbox post operation,
    /// posting to at most `limit` live matching subscribers and emitting one
    /// observation record per delivered mailbox event.
    ///
    /// This is the owner-aware companion to [`Self::notify_limit_emit_with_hint`]:
    /// substrate owns subscriber selection and observation, while callers with
    /// scheduler context can route each already-upgraded mailbox through their
    /// owner-aware post boundary.
    pub fn notify_limit_emit_with_owner_post<F>(
        &self,
        mask: InterestMask,
        limit: usize,
        hint: MailboxSchedulerHint,
        mut post: F,
    ) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent, MailboxSchedulerHint) -> bool,
    {
        if limit == 0 || mask.raw() == 0 {
            return 0;
        }

        let deliveries = {
            let mut subs = self.subscribers.lock();
            let mut deliveries = Vec::new();
            subs.retain(|sub| {
                let Some(mailbox) = sub.mailbox.upgrade() else {
                    return false;
                };
                let overlap = sub.interests.raw() & mask.raw();
                if overlap != 0 {
                    deliveries.push((mailbox, sub.generation, InterestMask::new(overlap)));
                }
                true
            });
            deliveries
        };

        let mut posted = 0usize;
        for (mailbox, generation, interests) in deliveries {
            if posted >= limit {
                break;
            }
            let evt = MailboxEvent::SourceFired {
                generation,
                source: self.id,
                interests,
            };
            if post(&mailbox, evt, hint) {
                posted += 1;
                if let Some(em) = tx_observe::current() {
                    em.wait_source_notify(
                        self.id.raw() as u32,
                        interests.raw() as u32,
                        mailbox.task_id_low(),
                        generation.raw() as u32,
                    );
                }
            }
        }
        if posted == 0 {
            self.pending_mask.fetch_or(mask.raw(), Ordering::AcqRel);
        }
        posted
    }

    /// Fire `mask` on this source and emit one `WaitSourceNotify` observation
    /// record per woken task (OBS-4 convergence-point observation).
    ///
    /// Identical wake-delivery semantics to [`Self::notify`]; additionally,
    /// for each subscriber that receives a `MailboxEvent::SourceFired`, emits
    /// one `TxTraceKind::Instant` / `TxPayloadTag::WaitSourceNotify` record
    /// into the current hart's SPSC ring (if the ring is configured).
    ///
    /// The platform context is implicit: `tx_observe::current()` reads the
    /// cpu-id via the function pointer installed by `tx_observe::init` at
    /// boot (D16 Option C). No type parameter is required at call sites,
    /// which is the fix for Drop barriers and async kthread bodies that
    /// cannot supply a `P: PercpuIf` type parameter (OBS-4 / D16).
    ///
    /// # OBS-A-1 compliance
    ///
    /// This method is a **substrate convergence point** (`notify_emit` is
    /// called from state-transition paths such as `decr_reader`,
    /// `decr_writer`, etc.). It is **never** called from inside a
    /// `StepOp::step()` body.
    pub fn notify_emit(&self, mask: InterestMask) -> usize {
        self.notify_emit_with_hint(mask, MailboxSchedulerHint::Normal)
    }

    /// Fire `mask` with an explicit scheduler hint and emit one
    /// `WaitSourceNotify` observation record per woken task.
    pub fn notify_emit_with_hint(&self, mask: InterestMask, hint: MailboxSchedulerHint) -> usize {
        self.pending_mask.fetch_or(mask.raw(), Ordering::AcqRel);
        let mut subs = self.subscribers.lock();
        let mut posted = 0usize;

        subs.retain(|sub| {
            let Some(mailbox) = sub.mailbox.upgrade() else {
                return false;
            };
            let overlap = sub.interests.raw() & mask.raw();
            if overlap != 0 {
                let evt = MailboxEvent::SourceFired {
                    generation: sub.generation,
                    source: self.id,
                    interests: InterestMask::new(overlap),
                };
                if mailbox.post_with_scheduler_hint(evt, hint) {
                    posted += 1;
                    // Emit one Instant record per woken task (OBS-4 / γ-fix).
                    if let Some(em) = tx_observe::current() {
                        em.wait_source_notify(
                            self.id.raw() as u32,
                            overlap as u32,
                            mailbox.task_id_low(),
                            sub.generation.raw() as u32,
                        );
                    }
                }
            }
            true
        });
        posted
    }

    fn deliver_pending_to(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
        interests: InterestMask,
    ) {
        let Some(mailbox) = mailbox.upgrade() else {
            return;
        };
        loop {
            let current = self.pending_mask.load(Ordering::Acquire);
            let overlap = current & interests.raw();
            if overlap == 0 {
                return;
            }
            let next = current & !overlap;
            match self.pending_mask.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let _ = mailbox.post(MailboxEvent::SourceFired {
                        generation,
                        source: self.id,
                        interests: InterestMask::new(overlap),
                    });
                    return;
                }
                Err(_) => continue,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Global WaitSource registry — bridges object-side sources to driver-side
// yield resolution.  Semantic objects insert their WaitSource at creation
// time; resolve_on_wait_source looks up by WaitSourceId to register the
// task mailbox before parking.
// ---------------------------------------------------------------------------

/// Global registry mapping [`WaitSourceId`] → [`WaitSource`].
///
/// Indexed sparsely by `WaitSourceId::raw()`. Notification source ids are
/// allocated from a high subsystem-owned range, so dense slot indexing would
/// try to materialize billions of empty entries during early boot.
static REGISTRY: SpinMutex<BTreeMap<u64, Arc<WaitSource>>> = SpinMutex::new(BTreeMap::new());

/// Snapshot of the global wait-source registry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RegistrySummary {
    pub slots: usize,
    pub live: usize,
}

/// Object-owned wait endpoint exposed to wait drivers.
///
/// This is the narrow language above mailbox delivery: semantic objects own
/// their [`WaitSource`], while drivers consume only an endpoint reference and
/// an `InterestMask`. Domain-specific readiness names stay on the owning
/// object as accessors such as `read_endpoint()` or `exit_endpoint()`.
pub trait WaitEndpoint {
    /// Stable source id used in `YieldShape::OnWaitSource` and mailbox events.
    fn source_id(&self) -> WaitSourceId;

    /// Strong source handle used to prepare mailbox registrations.
    fn source(&self) -> Arc<WaitSource>;
}

impl WaitEndpoint for Arc<WaitSource> {
    fn source_id(&self) -> WaitSourceId {
        self.id()
    }

    fn source(&self) -> Arc<WaitSource> {
        Arc::clone(self)
    }
}

/// Register a source in the global registry so the driver can find it
/// by [`WaitSourceId`] during yield resolution.
pub fn register_source(source: Arc<WaitSource>) {
    REGISTRY.lock().insert(source.id().raw(), source);
}

/// Remove a source from the global registry.
pub fn unregister_source(id: WaitSourceId) {
    REGISTRY.lock().remove(&id.raw());
}

/// Return the current registry slot count and live source count.
pub fn registry_summary() -> RegistrySummary {
    let reg = REGISTRY.lock();
    RegistrySummary {
        slots: reg.len(),
        live: reg.len(),
    }
}

/// Look up a source by id. Returns `None` if the id is unknown or
/// the source was never registered.
pub fn lookup_source(id: WaitSourceId) -> Option<Arc<WaitSource>> {
    REGISTRY.lock().get(&id.raw()).cloned()
}

/// Locate every live object wait currently subscribed by one task.
/// Used only by explicit stall diagnostics.
pub fn subscriber_diagnostics_for_task(task_id_low: u32) -> Vec<WaitSubscriberDiagnostic> {
    let sources: Vec<Arc<WaitSource>> = REGISTRY.lock().values().cloned().collect();
    sources
        .into_iter()
        .flat_map(|source| source.subscriber_diagnostics_for_task(task_id_low))
        .collect()
}

/// Snapshot every live subscriber in the registry for an explicit stall dump.
pub fn all_subscriber_diagnostics() -> Vec<WaitSubscriberDiagnostic> {
    let sources: Vec<Arc<WaitSource>> = REGISTRY.lock().values().cloned().collect();
    sources
        .into_iter()
        .flat_map(|source| source.subscriber_diagnostics())
        .collect()
}

// ---------------------------------------------------------------------------

/// A registration that has been **prepared** but not yet committed
/// to the [`WaitSource`]'s subscriber list. Construct via
/// [`WaitSource::prepare`]; commit via [`Self::install_if`].
///
/// The prepared form exists to support the lost-wake fix:
///
/// 1. Driver observes the predicate (e.g. pipe empty).
/// 2. Driver builds a `PreparedWaitRegistration`.
/// 3. Driver calls `install_if(|| still_blocked())` to re-test
///    under source lock discipline.
///    - **true**: registration commits; guard returned; task parks.
///    - **false**: registration discarded; driver retries the step.
///
/// Either way the task is never parked on a stale view of the
/// world: the re-test races correctly against any concurrent
/// `WaitSource::notify`.
#[must_use = "PreparedWaitRegistration must be installed via install_if or dropped explicitly"]
pub struct PreparedWaitRegistration<'a> {
    source: &'a WaitSource,
    mailbox: Weak<TaskMailbox>,
    generation: WaitGeneration,
    interests: InterestMask,
}

impl<'a> PreparedWaitRegistration<'a> {
    /// Re-test the blocked condition and commit the registration
    /// iff the predicate returns `true`.
    ///
    /// Returns `Some(guard)` on commit. The guard auto-deregisters
    /// on drop, so the typical use is `let _g = prep.install_if(...);`
    /// where the guard's lifetime spans the suspension.
    ///
    /// Returns `None` when the predicate returned `false` — the
    /// driver should retry the step rather than park.
    pub fn install_if<F>(self, predicate: F) -> Option<WaitRegistrationGuard<'a>>
    where
        F: FnOnce() -> bool,
    {
        let id =
            self.source
                .register_if(self.mailbox, self.generation, self.interests, predicate)?;
        Some(WaitRegistrationGuard {
            source: self.source,
            id: Some(id),
        })
    }

    /// Commit unconditionally. Used in tests and when the caller
    /// has already evaluated the predicate via an external lock.
    pub fn install(self) -> WaitRegistrationGuard<'a> {
        let id = self
            .source
            .register(self.mailbox, self.generation, self.interests);
        WaitRegistrationGuard {
            source: self.source,
            id: Some(id),
        }
    }
}

/// RAII handle for a committed wait registration. Auto-deregisters
/// the subscriber from the [`WaitSource`] on drop.
#[must_use = "drop the guard to deregister; binding to _ deregisters immediately"]
pub struct WaitRegistrationGuard<'a> {
    source: &'a WaitSource,
    /// `None` after `forget()` is called; the destructor becomes a
    /// no-op in that case. Set by [`Self::forget`] when the caller
    /// wants ownership transfer (e.g. moving the subscription into
    /// a longer-lived state machine).
    id: Option<SubscriberId>,
}

impl WaitRegistrationGuard<'_> {
    /// The subscriber id for diagnostic / equality checks.
    pub fn id(&self) -> Option<SubscriberId> {
        self.id
    }

    /// Suppress the auto-deregister on drop and return the raw id.
    /// The caller is then responsible for calling
    /// [`WaitSource::unregister`] manually.
    pub fn forget(mut self) -> Option<SubscriberId> {
        self.id.take()
    }
}

impl Drop for WaitRegistrationGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.source.unregister(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;

    fn mb() -> Arc<TaskMailbox> {
        Arc::new(TaskMailbox::new())
    }

    #[test]
    fn arc_wait_source_is_wait_endpoint() {
        let src = Arc::new(WaitSource::new(WaitSourceId::new(77)));
        register_source(Arc::clone(&src));

        let endpoint: &dyn WaitEndpoint = &src;
        assert_eq!(endpoint.source_id(), WaitSourceId::new(77));
        assert_eq!(endpoint.source().id(), WaitSourceId::new(77));

        unregister_source(WaitSourceId::new(77));
    }

    #[test]
    fn register_increments_subscriber_count() {
        let src = WaitSource::new(WaitSourceId::new(7));
        let m1 = mb();
        let m2 = mb();
        let _ = src.register(
            Arc::downgrade(&m1),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        assert_eq!(src.subscriber_count(), 1);
        let _ = src.register(
            Arc::downgrade(&m2),
            WaitGeneration::new(1),
            InterestMask::new(0b10),
        );
        assert_eq!(src.subscriber_count(), 2);
    }

    #[test]
    fn unregister_is_idempotent_and_removes_only_named_subscriber() {
        let src = WaitSource::new(WaitSourceId::new(7));
        let m = mb();
        let a = src.register(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        let b = src.register(
            Arc::downgrade(&m),
            WaitGeneration::new(2),
            InterestMask::new(0b10),
        );
        src.unregister(a);
        assert_eq!(src.subscriber_count(), 1);
        src.unregister(a); // Idempotent.
        assert_eq!(src.subscriber_count(), 1);
        src.unregister(b);
        assert_eq!(src.subscriber_count(), 0);
    }

    #[test]
    fn notify_posts_source_fired_to_matching_subscribers() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m1 = mb();
        let m2 = mb();
        let gen1 = WaitGeneration::new(5);
        let gen2 = WaitGeneration::new(9);
        let _ = src.register(Arc::downgrade(&m1), gen1, InterestMask::new(0b0011));
        let _ = src.register(Arc::downgrade(&m2), gen2, InterestMask::new(0b0010));

        let posted = src.notify(InterestMask::new(0b0010));
        assert_eq!(posted, 2);

        let e1 = m1.poll().expect("m1 got an event");
        match e1 {
            MailboxEvent::SourceFired {
                generation,
                source,
                interests,
            } => {
                assert_eq!(generation, gen1);
                assert_eq!(source, WaitSourceId::new(42));
                assert_eq!(interests, InterestMask::new(0b0010));
            }
            other => panic!("expected SourceFired, got {other:?}"),
        }

        let e2 = m2.poll().expect("m2 got an event");
        match e2 {
            MailboxEvent::SourceFired {
                generation,
                source,
                interests,
            } => {
                assert_eq!(generation, gen2);
                assert_eq!(source, WaitSourceId::new(42));
                assert_eq!(interests, InterestMask::new(0b0010));
            }
            other => panic!("expected SourceFired, got {other:?}"),
        }
    }

    #[test]
    fn default_notify_latches_normal_scheduler_hint() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let _ = src.register(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );

        assert_eq!(src.notify(InterestMask::new(0b1)), 1);
        assert_eq!(m.take_scheduler_hint(), MailboxSchedulerHint::Normal);
    }

    #[test]
    fn hinted_notify_limit_emit_latches_wake_handoff() {
        let src = WaitSource::new(WaitSourceId::new(43));
        let m = mb();
        let _ = src.register(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );

        assert_eq!(
            src.notify_limit_emit_with_hint(
                InterestMask::new(0b1),
                1,
                MailboxSchedulerHint::WakeHandoff,
            ),
            1
        );
        assert_eq!(m.take_scheduler_hint(), MailboxSchedulerHint::WakeHandoff);
    }

    #[test]
    fn notify_limit_posts_at_most_requested_matching_subscribers() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m1 = mb();
        let m2 = mb();
        let m3 = mb();
        let _ = src.register(
            Arc::downgrade(&m1),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        let _ = src.register(
            Arc::downgrade(&m2),
            WaitGeneration::new(2),
            InterestMask::new(0b1),
        );
        let _ = src.register(
            Arc::downgrade(&m3),
            WaitGeneration::new(3),
            InterestMask::new(0b1),
        );

        let posted = src.notify_limit(InterestMask::new(0b1), 1);
        assert_eq!(posted, 1);

        let delivered = [!m1.is_empty(), !m2.is_empty(), !m3.is_empty()]
            .into_iter()
            .filter(|ready| *ready)
            .count();
        assert_eq!(delivered, 1, "only one matching waiter should wake");
    }

    #[test]
    fn notify_limit_emit_posts_at_most_requested_matching_subscribers() {
        let src = WaitSource::new(WaitSourceId::new(45));
        let m1 = mb();
        let m2 = mb();
        let _ = src.register(
            Arc::downgrade(&m1),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        let _ = src.register(
            Arc::downgrade(&m2),
            WaitGeneration::new(2),
            InterestMask::new(0b1),
        );

        let posted = src.notify_limit_emit(InterestMask::new(0b1), 1);

        assert_eq!(posted, 1);
        let delivered = [!m1.is_empty(), !m2.is_empty()]
            .into_iter()
            .filter(|ready| *ready)
            .count();
        assert_eq!(delivered, 1, "only one matching waiter should wake");
    }

    #[test]
    fn notify_skips_subscribers_with_disjoint_interest_mask() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let _ = src.register(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b0100),
        );
        let posted = src.notify(InterestMask::new(0b1010));
        assert_eq!(posted, 0);
        assert!(m.is_empty());
    }

    #[test]
    fn notify_compacts_dead_subscribers() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let live = mb();
        // Create a mailbox, register, then drop the strong reference.
        {
            let temp = mb();
            let _ = src.register(
                Arc::downgrade(&temp),
                WaitGeneration::new(1),
                InterestMask::new(0b1),
            );
        }
        let _ = src.register(
            Arc::downgrade(&live),
            WaitGeneration::new(2),
            InterestMask::new(0b1),
        );
        assert_eq!(src.subscriber_count(), 2);

        let posted = src.notify(InterestMask::new(0b1));
        // Dead subscriber compacted out; only the live one received.
        assert_eq!(posted, 1);
        assert_eq!(src.subscriber_count(), 1);
        assert!(!live.is_empty());
    }

    #[test]
    fn prepared_registration_installs_on_true_predicate() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let prep = src.prepare(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        let guard = prep.install_if(|| true).expect("installed on true");
        assert_eq!(src.subscriber_count(), 1);
        assert!(guard.id().is_some());
        // Drop guard.
        drop(guard);
        assert_eq!(src.subscriber_count(), 0);
    }

    #[test]
    fn prepared_registration_skips_install_on_false_predicate() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let prep = src.prepare(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        let result = prep.install_if(|| false);
        assert!(result.is_none());
        assert_eq!(src.subscriber_count(), 0);
    }

    #[test]
    fn wait_registration_guard_auto_deregisters_on_drop() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let prep = src.prepare(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        {
            let _g = prep.install();
            assert_eq!(src.subscriber_count(), 1);
        }
        assert_eq!(src.subscriber_count(), 0);
    }

    #[test]
    fn wait_registration_guard_forget_suppresses_drop_deregister() {
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let prep = src.prepare(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1),
        );
        let guard = prep.install();
        let id = guard.forget().expect("guard had an id");
        assert_eq!(src.subscriber_count(), 1, "forget suppressed drop");
        src.unregister(id);
        assert_eq!(src.subscriber_count(), 0);
    }

    #[test]
    fn lost_wake_fix_pattern_round_trip() {
        // Sketch of the PR-3D production pattern:
        //
        //   1. Observer reads predicate -> "blocked".
        //   2. Driver prepares registration.
        //   3. install_if re-tests under predicate lock.
        //   4. Concurrent notify still works because we registered
        //      before parking.
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let gen = WaitGeneration::new(1);
        let prep = src.prepare(Arc::downgrade(&m), gen, InterestMask::new(0b1));
        let _guard = prep.install_if(|| true).expect("still blocked");

        // Concurrent notify fires.
        let posted = src.notify(InterestMask::new(0b1));
        assert_eq!(posted, 1);

        // Task wakes; mailbox has the event with the right generation.
        let evt = m.poll().expect("event arrived");
        match evt {
            MailboxEvent::SourceFired { generation, .. } => {
                assert_eq!(generation, gen);
            }
            other => panic!("expected SourceFired, got {other:?}"),
        }
    }

    #[test]
    fn notify_carries_overlap_not_full_mask() {
        // Subscriber interested in 0b1010; fire 0b1110 → posted
        // interests should be 0b1010 (the overlap), not 0b1110.
        let src = WaitSource::new(WaitSourceId::new(42));
        let m = mb();
        let _ = src.register(
            Arc::downgrade(&m),
            WaitGeneration::new(1),
            InterestMask::new(0b1010),
        );
        let _ = src.notify(InterestMask::new(0b1110));
        let evt = m.poll().expect("event posted");
        match evt {
            MailboxEvent::SourceFired { interests, .. } => {
                assert_eq!(interests, InterestMask::new(0b1010));
            }
            other => panic!("expected SourceFired, got {other:?}"),
        }
    }
}
