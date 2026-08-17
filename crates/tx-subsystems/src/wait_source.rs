//! Registered wait-source resolver.
//!
//! Subsystems that can produce wait-source outcomes register their wake source
//! here and embed the returned source id in the yielded wait shape. Async
//! wrappers convert the source id plus interest mask into an awaitable future
//! via `wait_on_registered_source_id`.
//!
//! Test-only placeholder ids do not register anything, so
//! `wait_on_registered_source_id` returns `None` for them. Production code is
//! expected to publish wait shapes whose source is a registered id.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll};
use tx_substrate::bus::{RawPort, RawPortSubscription, RawQueue, RawQueueSubscription};
use tx_substrate::wake::WaitEndpoint;

use crate::adapter::step_engine::SpinMutex;
use crate::adapter::wait_mailbox::{ActiveWait, InterestMask, TaskMailbox, WaitGeneration};
use crate::adapter::wait_routing::{Mask, WaitOutcome};
use crate::execution::WaitToken;

#[derive(Clone)]
enum RegisteredWaitSource {
    WaitSource {
        source: Arc<tx_substrate::wake::WaitSource>,
        diagnostic_kind: &'static str,
    },
    RawQueue(RawQueue),
    RawPort(RawPort),
}

/// Future returned after resolving a registered wait-source id.
pub enum RegisteredWaitFuture {
    WaitSource(Box<WaitSourceWaitFuture>),
    RawQueue(Box<RawQueueWaitFuture>),
    RawPort(Box<RawPortWaitFuture>),
}

/// Scoped registration of the driver task mailbox on a registered raw wire.
/// Dropping this value removes the wire subscription.
pub enum RegisteredMailboxSubscription {
    RawQueue(RawQueueSubscription),
    RawPort(RawPortSubscription),
}

/// Result of installing a driver mailbox on a registered raw wire.
pub enum RegisteredMailboxWait {
    Ready,
    Pending(RegisteredMailboxSubscription),
}

/// Awaitable wait over a mailbox-backed [`tx_substrate::wake::WaitSource`].
pub struct WaitSourceWaitFuture {
    source: Arc<tx_substrate::wake::WaitSource>,
    mask: Mask,
    mailbox: Arc<TaskMailbox>,
    active_wait: Option<ActiveWait>,
    subscriber: Option<tx_substrate::wake::SubscriberId>,
    still_blocked: Option<Box<dyn Fn() -> bool + Send + Sync>>,
}

/// Awaitable readiness wait over a bus [`RawQueue`].
pub struct RawQueueWaitFuture {
    queue: RawQueue,
    mask: Mask,
    subscription: Option<RawQueueSubscription>,
    mailbox: Arc<TaskMailbox>,
    active_wait: Option<ActiveWait>,
}

/// Awaitable edge-event wait over a bus [`RawPort`].
pub struct RawPortWaitFuture {
    port: RawPort,
    mask: Mask,
    subscription: Option<RawPortSubscription>,
    mailbox: Arc<TaskMailbox>,
    active_wait: Option<ActiveWait>,
}

static REGISTRY: SpinMutex<BTreeMap<u64, RegisteredWaitSource>> = SpinMutex::new(BTreeMap::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WaitSourceRegistrySummary {
    pub total: usize,
    pub wait_sources: usize,
    pub raw_queues: usize,
    pub raw_ports: usize,
}

/// Register a mailbox-backed [`tx_substrate::wake::WaitSource`] under an
/// explicit carrier id.
pub fn register_wait_source_with_id(id: u64, source: Arc<tx_substrate::wake::WaitSource>) {
    register_wait_source_with_diagnostic_kind(id, source, "unknown");
}

/// Register an object wait source together with a read-only semantic label.
/// The label is consumed only by the explicit kernel stall diagnostic.
pub fn register_wait_source_with_diagnostic_kind(
    id: u64,
    source: Arc<tx_substrate::wake::WaitSource>,
    diagnostic_kind: &'static str,
) {
    tx_substrate::wake::register_source(Arc::clone(&source));
    REGISTRY.lock().insert(
        id,
        RegisteredWaitSource::WaitSource {
            source,
            diagnostic_kind,
        },
    );
}

/// Register a level-triggered readiness queue for wait-source resolution.
pub fn register_wait_queue(queue: RawQueue) -> u64 {
    let mut registry = REGISTRY.lock();
    let existing = queue.source_id().raw();
    if existing != 0 {
        registry.insert(existing, RegisteredWaitSource::RawQueue(queue));
        return existing;
    }

    let id = NEXT_ID.fetch_add(1, Ordering::AcqRel);
    queue.set_source_id(tx_substrate::step::WaitSourceId::new(id));
    registry.insert(id, RegisteredWaitSource::RawQueue(queue));
    id
}

/// Register a level-triggered queue under an object-owned source id.
///
/// Page-backed waits deliberately pair this readiness predicate with a
/// `WaitSource` carrying the same id: the queue closes the subscribe-after-
/// completion race, while the WaitSource preserves owner-aware reactor wakeup.
pub fn register_wait_queue_with_id(id: u64, queue: RawQueue) {
    queue.set_source_id(tx_substrate::step::WaitSourceId::new(id));
    REGISTRY
        .lock()
        .insert(id, RegisteredWaitSource::RawQueue(queue));
}

/// Register an edge-triggered port for wait-source resolution.
pub fn register_wait_port(port: RawPort) -> u64 {
    let mut registry = REGISTRY.lock();
    let existing = port.source_id().raw();
    if existing != 0 {
        registry.insert(existing, RegisteredWaitSource::RawPort(port));
        return existing;
    }

    let id = NEXT_ID.fetch_add(1, Ordering::AcqRel);
    port.set_source_id(tx_substrate::step::WaitSourceId::new(id));
    registry.insert(id, RegisteredWaitSource::RawPort(port));
    id
}

/// Register an edge-triggered port under an object-owned source id.
pub fn register_wait_port_with_id(id: u64, port: RawPort) {
    port.set_source_id(tx_substrate::step::WaitSourceId::new(id));
    REGISTRY
        .lock()
        .insert(id, RegisteredWaitSource::RawPort(port));
}

/// Drop the registry's clone of any wait source registered under `id`.
pub fn release_wait_source(id: u64) {
    REGISTRY.lock().remove(&id);
}

/// Current number of registered wait sources.
pub fn registered_wait_source_count() -> usize {
    REGISTRY.lock().len()
}

pub fn registry_summary() -> WaitSourceRegistrySummary {
    let registry = REGISTRY.lock();
    let mut summary = WaitSourceRegistrySummary {
        total: registry.len(),
        ..WaitSourceRegistrySummary::default()
    };
    for source in registry.values() {
        match source {
            RegisteredWaitSource::WaitSource { .. } => summary.wait_sources += 1,
            RegisteredWaitSource::RawQueue(_) => summary.raw_queues += 1,
            RegisteredWaitSource::RawPort(_) => summary.raw_ports += 1,
        }
    }
    summary
}

/// Return a clone of the queue registered under `id`.
pub fn lookup_wait_queue(id: u64) -> Option<RawQueue> {
    match REGISTRY.lock().get(&id) {
        Some(RegisteredWaitSource::RawQueue(queue)) => Some(queue.clone()),
        _ => None,
    }
}

/// Return a clone of the port registered under `id`.
pub fn lookup_wait_port(id: u64) -> Option<RawPort> {
    match REGISTRY.lock().get(&id) {
        Some(RegisteredWaitSource::RawPort(port)) => Some(port.clone()),
        _ => None,
    }
}

/// Return a clone of the mailbox-backed wait source registered under `id`.
pub fn lookup_wait_source(id: u64) -> Option<Arc<tx_substrate::wake::WaitSource>> {
    match REGISTRY.lock().get(&id) {
        Some(RegisteredWaitSource::WaitSource { source, .. }) => Some(Arc::clone(source)),
        _ => None,
    }
}

/// Return the semantic owner label attached at source construction.
pub fn registered_wait_source_diagnostic_kind(id: u64) -> Option<&'static str> {
    match REGISTRY.lock().get(&id) {
        Some(RegisteredWaitSource::WaitSource {
            diagnostic_kind, ..
        }) => Some(*diagnostic_kind),
        Some(RegisteredWaitSource::RawQueue(_)) => Some("raw-queue"),
        Some(RegisteredWaitSource::RawPort(_)) => Some("raw-port"),
        None => None,
    }
}

/// Install the driver's task mailbox on a registered raw wire.
///
/// A level-triggered queue uses `peek -> subscribe -> peek` so a readiness
/// transition between observation and subscription retries the step instead
/// of being lost. Edge-triggered ports have no level predicate to observe.
pub fn install_registered_mailbox_wait(
    source_id: u64,
    interest: u64,
    mailbox: Weak<TaskMailbox>,
    generation: WaitGeneration,
) -> Option<RegisteredMailboxWait> {
    let source = REGISTRY.lock().get(&source_id).cloned()?;
    match source {
        RegisteredWaitSource::WaitSource { .. } => None,
        RegisteredWaitSource::RawQueue(queue) => {
            if queue.peek() & interest != 0 {
                return Some(RegisteredMailboxWait::Ready);
            }

            let subscription = queue.subscribe(interest, mailbox, generation);
            if queue.peek() & interest != 0 {
                drop(subscription);
                Some(RegisteredMailboxWait::Ready)
            } else {
                Some(RegisteredMailboxWait::Pending(
                    RegisteredMailboxSubscription::RawQueue(subscription),
                ))
            }
        }
        RegisteredWaitSource::RawPort(port) => Some(RegisteredMailboxWait::Pending(
            RegisteredMailboxSubscription::RawPort(port.subscribe(interest, mailbox, generation)),
        )),
    }
}

fn wait_source_future(
    source: Arc<tx_substrate::wake::WaitSource>,
    interest: u64,
    still_blocked: Option<Box<dyn Fn() -> bool + Send + Sync>>,
) -> WaitSourceWaitFuture {
    WaitSourceWaitFuture {
        source,
        mask: Mask::from_bits(interest),
        mailbox: Arc::new(TaskMailbox::new()),
        active_wait: None,
        subscriber: None,
        still_blocked,
    }
}

/// Convert a source id and interest mask into an awaitable wait over a
/// mailbox-backed [`tx_substrate::wake::WaitSource`].
#[cfg(test)]
pub fn wait_on_source_id(source_id: u64, interest: u64) -> Option<WaitSourceWaitFuture> {
    lookup_wait_source(source_id).map(|source| wait_source_future(source, interest, None))
}

/// Convert an object-owned endpoint and interest mask into an awaitable
/// mailbox-backed wait without going through the global id registry.
pub fn wait_on_endpoint(
    endpoint: &(impl WaitEndpoint + ?Sized),
    interest: u64,
) -> WaitSourceWaitFuture {
    wait_source_future(endpoint.source(), interest, None)
}

/// Convert an object-owned endpoint into the same erased wait-future shape used
/// by registered source-id waits.
pub fn wait_on_registered_endpoint(
    endpoint: &(impl WaitEndpoint + ?Sized),
    interest: u64,
) -> RegisteredWaitFuture {
    RegisteredWaitFuture::WaitSource(Box::new(wait_on_endpoint(endpoint, interest)))
}

/// Wait on an object-owned endpoint while atomically rechecking whether the
/// caller is still blocked as the subscription is installed.
///
/// `still_blocked` runs while the source's subscriber lock is held. It must be
/// a short, nonblocking level check (normally one or more atomic loads). When
/// it returns `false`, the future completes immediately so the caller can
/// rescan the object instead of sleeping after a wake raced with registration.
pub fn wait_on_registered_endpoint_if<F>(
    endpoint: &(impl WaitEndpoint + ?Sized),
    interest: u64,
    still_blocked: F,
) -> RegisteredWaitFuture
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    RegisteredWaitFuture::WaitSource(Box::new(wait_source_future(
        endpoint.source(),
        interest,
        Some(Box::new(still_blocked)),
    )))
}

/// Convert a registered source id and interest mask into an awaitable wait over
/// the concrete registered source kind.
pub fn wait_on_registered_source_id(source_id: u64, interest: u64) -> Option<RegisteredWaitFuture> {
    let mask = Mask::from_bits(interest);
    let source = REGISTRY.lock().get(&source_id).cloned()?;
    match source {
        RegisteredWaitSource::WaitSource { source, .. } => Some(RegisteredWaitFuture::WaitSource(
            Box::new(wait_source_future(source, mask.bits(), None)),
        )),
        RegisteredWaitSource::RawQueue(queue) => Some(RegisteredWaitFuture::RawQueue(Box::new(
            RawQueueWaitFuture {
                queue,
                mask,
                subscription: None,
                mailbox: Arc::new(TaskMailbox::new()),
                active_wait: None,
            },
        ))),
        RegisteredWaitSource::RawPort(port) => {
            Some(RegisteredWaitFuture::RawPort(Box::new(RawPortWaitFuture {
                port,
                mask,
                subscription: None,
                mailbox: Arc::new(TaskMailbox::new()),
                active_wait: None,
            })))
        }
    }
}

/// Compatibility bridge for subsystems that still carry a structured
/// [`WaitToken`]. Resolution uses main's unified registered-source registry,
/// so channel, raw-queue, and raw-port waits all preserve the same semantics.
pub fn wait_on_token(token: WaitToken) -> Option<RegisteredWaitFuture> {
    wait_on_registered_source_id(token.source_id(), token.interest())
}

impl Unpin for RegisteredWaitFuture {}
impl Unpin for WaitSourceWaitFuture {}
impl Unpin for RawQueueWaitFuture {}
impl Unpin for RawPortWaitFuture {}

impl Future for RegisteredWaitFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            Self::WaitSource(future) => Pin::new(future.as_mut()).poll(cx),
            Self::RawQueue(future) => Pin::new(future.as_mut()).poll(cx),
            Self::RawPort(future) => Pin::new(future.as_mut()).poll(cx),
        }
    }
}

impl Future for WaitSourceWaitFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.mask.is_empty() {
            return Poll::Ready(WaitOutcome::Ready);
        }
        this.mailbox.register_waker(cx.waker().clone());

        if this.active_wait.is_none() {
            let generation = this.mailbox.next_generation();
            this.active_wait = Some(ActiveWait::new(
                generation,
                this.source.id(),
                InterestMask::new(this.mask.bits()),
            ));
            let subscriber = if let Some(still_blocked) = this.still_blocked.as_ref() {
                let prepared = this.source.prepare(
                    Arc::downgrade(&this.mailbox),
                    generation,
                    InterestMask::new(this.mask.bits()),
                );
                let Some(registration) = prepared.install_if(still_blocked.as_ref()) else {
                    this.active_wait = None;
                    return Poll::Ready(WaitOutcome::Ready);
                };
                registration
                    .forget()
                    .expect("installed wait registration has a subscriber id")
            } else {
                // The legacy WaitToken bridge has no predicate to re-test.
                this.source.register(
                    Arc::downgrade(&this.mailbox),
                    generation,
                    InterestMask::new(this.mask.bits()),
                )
            };
            this.subscriber = Some(subscriber);
        }

        if mailbox_ready(&this.mailbox, this.active_wait.as_ref()) {
            if let Some(subscriber) = this.subscriber.take() {
                this.source.unregister(subscriber);
            }
            Poll::Ready(WaitOutcome::Ready)
        } else {
            Poll::Pending
        }
    }
}

impl Drop for WaitSourceWaitFuture {
    fn drop(&mut self) {
        if let Some(subscriber) = self.subscriber.take() {
            self.source.unregister(subscriber);
        }
    }
}

impl Future for RawQueueWaitFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.mask.is_empty() {
            return Poll::Ready(WaitOutcome::Ready);
        }
        this.mailbox.register_waker(cx.waker().clone());

        let ready = if this.subscription.is_some() {
            // RawQueue is level-triggered. A stale mailbox generation may
            // suppress an old wake hint, but it must not suppress readiness
            // that is still asserted by the producer.
            mailbox_ready(&this.mailbox, this.active_wait.as_ref())
                || (this.queue.peek() & this.mask.bits() != 0)
        } else if this.queue.peek() & this.mask.bits() != 0 {
            true
        } else {
            ensure_queue_active_wait(this);
            let generation = this.active_wait.as_ref().expect("active wait").generation;
            this.subscription = Some(this.queue.subscribe(
                this.mask.bits(),
                Arc::downgrade(&this.mailbox),
                generation,
            ));
            if this.queue.peek() & this.mask.bits() != 0 {
                this.subscription = None;
                true
            } else {
                mailbox_ready(&this.mailbox, this.active_wait.as_ref())
            }
        };

        if ready {
            this.subscription = None;
            Poll::Ready(WaitOutcome::Ready)
        } else {
            Poll::Pending
        }
    }
}

impl Future for RawPortWaitFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.mask.is_empty() {
            return Poll::Ready(WaitOutcome::Ready);
        }
        this.mailbox.register_waker(cx.waker().clone());

        let ready = if this.subscription.is_some() {
            mailbox_ready(&this.mailbox, this.active_wait.as_ref())
        } else {
            ensure_port_active_wait(this);
            let generation = this.active_wait.as_ref().expect("active wait").generation;
            this.subscription = Some(this.port.subscribe(
                this.mask.bits(),
                Arc::downgrade(&this.mailbox),
                generation,
            ));
            mailbox_ready(&this.mailbox, this.active_wait.as_ref())
        };

        if ready {
            this.subscription = None;
            Poll::Ready(WaitOutcome::Ready)
        } else {
            Poll::Pending
        }
    }
}

fn ensure_queue_active_wait(future: &mut RawQueueWaitFuture) {
    if future.active_wait.is_none() {
        let generation = future.mailbox.next_generation();
        future.active_wait = Some(ActiveWait::new(
            generation,
            future.queue.source_id(),
            InterestMask::new(future.mask.bits()),
        ));
    }
}

fn ensure_port_active_wait(future: &mut RawPortWaitFuture) {
    if future.active_wait.is_none() {
        let generation = future.mailbox.next_generation();
        future.active_wait = Some(ActiveWait::new(
            generation,
            future.port.source_id(),
            InterestMask::new(future.mask.bits()),
        ));
    }
}

fn mailbox_ready(mailbox: &TaskMailbox, active_wait: Option<&ActiveWait>) -> bool {
    while let Some(event) = mailbox.poll() {
        if active_wait.is_some_and(|wait| wait.matches(&event)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_queue_register_returns_nonzero_distinct_ids() {
        let a = register_wait_queue(RawQueue::new());
        let b = register_wait_queue(RawQueue::new());
        assert!(a != 0);
        assert!(b != 0);
        assert_ne!(a, b);
        release_wait_source(a);
        release_wait_source(b);
    }

    #[test]
    fn registering_a_queue_binds_the_producer_handle_to_the_registry_id() {
        let queue = RawQueue::new();
        let id = register_wait_queue(queue.clone());

        assert_eq!(queue.source_id().raw(), id);

        release_wait_source(id);
    }

    #[test]
    fn registering_a_port_binds_the_producer_handle_to_the_registry_id() {
        let port = RawPort::new();
        let id = register_wait_port(port.clone());

        assert_eq!(port.source_id().raw(), id);

        release_wait_source(id);
    }

    #[test]
    fn wait_source_can_register_already_minted_notification_id() {
        let id = u64::from(u32::MAX) + 17;
        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(id),
        ));
        register_wait_source_with_id(id, Arc::clone(&source));
        assert!(lookup_wait_source(id).is_some());
        release_wait_source(id);
        assert!(lookup_wait_source(id).is_none());
    }

    #[test]
    fn wait_source_lookup_returns_none_for_unregistered_id() {
        assert!(lookup_wait_source(0xdead_beef_dead_beef).is_none());
    }

    #[test]
    fn wait_on_registered_source_id_resolves_mailbox_wait_source() {
        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(0x44),
        ));
        register_wait_source_with_id(0x44, Arc::clone(&source));
        let mut wait = wait_on_registered_source_id(0x44, 0x1).expect("registered wait source");

        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        assert!(matches!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending));

        assert_eq!(source.notify(InterestMask::new(0x1)), 1);
        assert!(matches!(
            Pin::new(&mut wait).poll(&mut cx),
            Poll::Ready(WaitOutcome::Ready)
        ));

        release_wait_source(0x44);
    }

    #[test]
    fn wait_on_source_id_resolves_mailbox_wait_source_without_token() {
        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(0x45),
        ));
        register_wait_source_with_id(0x45, Arc::clone(&source));
        let mut wait = wait_on_source_id(0x45, 0x1).expect("registered wait source");

        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        assert!(matches!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending));

        assert_eq!(source.notify(InterestMask::new(0x1)), 1);
        assert!(matches!(
            Pin::new(&mut wait).poll(&mut cx),
            Poll::Ready(WaitOutcome::Ready)
        ));

        release_wait_source(0x45);
    }

    #[test]
    fn wait_on_endpoint_subscribes_to_object_owned_source() {
        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(0x47),
        ));
        let mut wait = wait_on_endpoint(&source, 0x2);

        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        assert!(matches!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending));

        assert_eq!(source.notify(InterestMask::new(0x2)), 1);
        assert!(matches!(
            Pin::new(&mut wait).poll(&mut cx),
            Poll::Ready(WaitOutcome::Ready)
        ));
    }

    #[test]
    fn conditional_endpoint_wait_rechecks_level_before_subscribing() {
        use core::sync::atomic::{AtomicBool, Ordering};

        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(0x48),
        ));
        let blocked = Arc::new(AtomicBool::new(false));
        let blocked_for_wait = Arc::clone(&blocked);
        let mut wait = wait_on_registered_endpoint_if(&source, 0x1, move || {
            blocked_for_wait.load(Ordering::Acquire)
        });

        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        assert!(matches!(
            Pin::new(&mut wait).poll(&mut cx),
            Poll::Ready(WaitOutcome::Ready)
        ));
        assert_eq!(source.subscriber_count(), 0);
    }

    #[test]
    fn conditional_endpoint_wait_subscribes_while_level_remains_blocked() {
        use core::sync::atomic::{AtomicBool, Ordering};

        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(0x49),
        ));
        let blocked = Arc::new(AtomicBool::new(true));
        let blocked_for_wait = Arc::clone(&blocked);
        let mut wait = wait_on_registered_endpoint_if(&source, 0x1, move || {
            blocked_for_wait.load(Ordering::Acquire)
        });

        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        assert!(matches!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending));
        assert_eq!(source.subscriber_count(), 1);

        blocked.store(false, Ordering::Release);
        assert_eq!(source.notify(InterestMask::new(0x1)), 1);
        assert!(matches!(
            Pin::new(&mut wait).poll(&mut cx),
            Poll::Ready(WaitOutcome::Ready)
        ));
        assert_eq!(source.subscriber_count(), 0);
    }

    #[test]
    fn conditional_endpoint_wait_survives_notify_registration_race_stress() {
        use core::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Barrier;

        const ITERATIONS: usize = 20_000;

        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(0x4a),
        ));
        let blocked = Arc::new(AtomicBool::new(true));
        let start = Arc::new(Barrier::new(2));
        let fired = Arc::new(Barrier::new(2));

        std::thread::scope(|scope| {
            let producer_source = Arc::clone(&source);
            let producer_blocked = Arc::clone(&blocked);
            let producer_start = Arc::clone(&start);
            let producer_fired = Arc::clone(&fired);
            scope.spawn(move || {
                for iteration in 0..ITERATIONS {
                    producer_start.wait();
                    if iteration & 1 == 0 {
                        std::thread::yield_now();
                    }
                    producer_blocked.store(false, Ordering::Release);
                    let _ = producer_source.notify(InterestMask::new(0x1));
                    producer_fired.wait();
                }
            });

            let waker = core::task::Waker::noop();
            let mut cx = core::task::Context::from_waker(waker);
            for iteration in 0..ITERATIONS {
                blocked.store(true, Ordering::Release);
                let blocked_for_wait = Arc::clone(&blocked);
                let mut wait = wait_on_registered_endpoint_if(&source, 0x1, move || {
                    blocked_for_wait.load(Ordering::Acquire)
                });

                start.wait();
                if iteration & 1 != 0 {
                    std::thread::yield_now();
                }
                let first = Pin::new(&mut wait).poll(&mut cx);
                fired.wait();
                let outcome = match first {
                    Poll::Ready(outcome) => Poll::Ready(outcome),
                    Poll::Pending => Pin::new(&mut wait).poll(&mut cx),
                };
                assert!(matches!(outcome, Poll::Ready(WaitOutcome::Ready)));
                drop(wait);
                assert_eq!(source.subscriber_count(), 0);
            }
        });
    }

    #[test]
    fn wait_on_registered_source_id_returns_none_for_test_placeholder_source() {
        // Existing test mocks (BlockingFs, LifecycleFs) construct tokens with
        // arbitrary numbers; those source ids must not panic in the registered
        // resolver; they return None so production await sites can treat them
        // as a sentinel.
        //
        // Use an obviously-out-of-range ID — Slice 3 (futex) registers
        // 256 carriers at zone-init time, claiming IDs 1..N for some
        // N that grows with each new wait-carrier producer. A
        // sentinel above the entire u32 space is never collisional.
        assert!(wait_on_registered_source_id(u64::MAX - 1, 0x55).is_none());
    }

    #[test]
    fn wait_on_registered_source_id_returns_none_after_release() {
        let id = 0x46;
        let source = Arc::new(tx_substrate::wake::WaitSource::new(
            tx_substrate::step::WaitSourceId::new(id),
        ));
        register_wait_source_with_id(id, source);
        assert!(wait_on_registered_source_id(id, 0x1).is_some());
        release_wait_source(id);
        assert!(wait_on_registered_source_id(id, 0x1).is_none());
    }

    #[test]
    fn subscribed_raw_queue_rechecks_level_after_stale_mailbox_event() {
        let queue = RawQueue::new();
        let mut wait = RawQueueWaitFuture {
            queue: queue.clone(),
            mask: Mask::from_bits(0x4),
            subscription: None,
            mailbox: Arc::new(TaskMailbox::new()),
            active_wait: None,
        };
        let waker = core::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);

        assert!(matches!(Pin::new(&mut wait).poll(&mut cx), Poll::Pending));
        assert!(wait.subscription.is_some());

        // Keep the old queue subscription but advance the driver's active
        // generation. The resulting SourceFired event is intentionally stale;
        // the queue's asserted level remains authoritative.
        let generation = wait.mailbox.next_generation();
        wait.active_wait = Some(ActiveWait::new(
            generation,
            queue.source_id(),
            InterestMask::new(0x4),
        ));
        queue.fire(0x4);

        assert!(matches!(
            Pin::new(&mut wait).poll(&mut cx),
            Poll::Ready(WaitOutcome::Ready)
        ));
    }
}
