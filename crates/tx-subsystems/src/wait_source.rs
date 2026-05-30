//! `WaitToken` → `Channel` resolver.
//!
//! Subsystems that can produce `Blocked(WaitToken)` outcomes register their
//! wake `Channel` here and embed the returned carrier id in any
//! `WaitToken` they hand out. Async script wrappers convert a `WaitToken`
//! into an awaitable `WaitFuture` via `wait_on_token`.
//!
//! Test-only `WaitToken` placeholders constructed with arbitrary
//! `WaitToken::new(carrier, interest)` literals do not register anything,
//! so `wait_on_token` returns `None` for them. Production code is expected
//! to construct `WaitToken` values whose carrier is a registered id.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::{Context, Poll};
use tx_substrate::bus::{RawPort, RawPortSubscription, RawQueue, RawQueueSubscription};

use crate::adapter::step_engine::SpinMutex;
use crate::adapter::wait_mailbox::{ActiveWait, InterestMask, TaskMailbox};
use crate::adapter::wait_routing::{Channel, Mask, WaitFuture, WaitOutcome};

use crate::execution::WaitToken;

#[derive(Clone)]
enum RegisteredWaitSource {
    Channel(Channel),
    RawQueue(RawQueue),
    RawPort(RawPort),
}

/// Future returned by [`wait_on_token`] after resolving a wait-source id.
pub enum RegisteredWaitFuture {
    Channel(Box<WaitFuture>),
    RawQueue(Box<RawQueueWaitFuture>),
    RawPort(Box<RawPortWaitFuture>),
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

fn register_wait_source(source: RegisteredWaitSource) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::AcqRel);
    REGISTRY.lock().insert(id, source);
    id
}

fn register_wait_source_with_id(id: u64, source: RegisteredWaitSource) {
    let old = REGISTRY.lock().insert(id, source);
    debug_assert!(old.is_none(), "wait-source id {id} was already registered");
}

/// Register `channel` for carrier-based wait resolution. Returns the
/// carrier id that consumers should embed in their `WaitToken` values. The
/// registry holds an internal clone; the caller's channel handle remains
/// independent. Carrier ids are non-zero and monotonically increasing.
pub fn register_wait_channel(channel: Channel) -> u64 {
    register_wait_source(RegisteredWaitSource::Channel(channel))
}

/// Register `channel` under an externally allocated carrier id.
///
/// Notification adapters that publish both a legacy `WaitToken` channel and
/// a v3 `WaitSource` allocate one shared id up front, then register that id
/// here so both wait paths resolve the same readiness source.
pub(crate) fn register_wait_channel_with_id(id: u64, channel: Channel) {
    register_wait_source_with_id(id, RegisteredWaitSource::Channel(channel));
}

/// Register a level-triggered readiness queue for wait-source resolution.
pub fn register_wait_queue(queue: RawQueue) -> u64 {
    register_wait_source(RegisteredWaitSource::RawQueue(queue))
}

/// Register a level-triggered readiness queue under an externally allocated
/// carrier id.
pub(crate) fn register_wait_queue_with_id(id: u64, queue: RawQueue) {
    register_wait_source_with_id(id, RegisteredWaitSource::RawQueue(queue));
}

/// Register an edge-triggered port for wait-source resolution.
pub fn register_wait_port(port: RawPort) -> u64 {
    register_wait_source(RegisteredWaitSource::RawPort(port))
}

/// Drop the registry's clone of the channel registered under `id`.
/// Subsequent `lookup_wait_channel` and `wait_on_token` calls for `id`
/// return `None`. Idempotent: releasing an unregistered id is a no-op.
pub fn release_wait_channel(id: u64) {
    release_wait_source(id);
}

/// Drop the registry's clone of any wait source registered under `id`.
pub fn release_wait_source(id: u64) {
    REGISTRY.lock().remove(&id);
}

/// Return a clone of the channel registered under `id`, or `None` if no
/// channel is currently registered for that id.
pub fn lookup_wait_channel(id: u64) -> Option<Channel> {
    match REGISTRY.lock().get(&id) {
        Some(RegisteredWaitSource::Channel(channel)) => Some(channel.clone()),
        _ => None,
    }
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

/// Convert `token` into a `WaitFuture` over the registered channel. Returns
/// `None` if `token.source_id()` is not a currently-registered id (typical
/// for test placeholder tokens constructed via raw `WaitToken::new`).
pub fn wait_on_token(token: WaitToken) -> Option<RegisteredWaitFuture> {
    let mask = Mask::from_bits(token.interest());
    let source = REGISTRY.lock().get(&token.source_id()).cloned()?;
    match source {
        RegisteredWaitSource::Channel(channel) => {
            Some(RegisteredWaitFuture::Channel(Box::new(channel.wait(mask))))
        }
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

impl Unpin for RegisteredWaitFuture {}
impl Unpin for RawQueueWaitFuture {}
impl Unpin for RawPortWaitFuture {}

impl Future for RegisteredWaitFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            Self::Channel(future) => Pin::new(future.as_mut()).poll(cx),
            Self::RawQueue(future) => Pin::new(future.as_mut()).poll(cx),
            Self::RawPort(future) => Pin::new(future.as_mut()).poll(cx),
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
            mailbox_ready(&this.mailbox, this.active_wait.as_ref())
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
    fn wait_source_register_returns_nonzero_distinct_ids() {
        let a = register_wait_channel(Channel::new());
        let b = register_wait_channel(Channel::new());
        assert!(a != 0);
        assert!(b != 0);
        assert_ne!(a, b);
        release_wait_channel(a);
        release_wait_channel(b);
    }

    #[test]
    fn wait_source_lookup_returns_some_for_registered_id_and_none_after_release() {
        let id = register_wait_channel(Channel::new());
        assert!(lookup_wait_channel(id).is_some());
        release_wait_channel(id);
        assert!(lookup_wait_channel(id).is_none());
    }

    #[test]
    fn wait_source_lookup_returns_none_for_unregistered_id() {
        assert!(lookup_wait_channel(0xdead_beef_dead_beef).is_none());
    }

    #[test]
    fn wait_on_token_returns_some_for_registered_carrier() {
        let id = register_wait_channel(Channel::new());
        let token = WaitToken::new(id, 0x1);
        assert!(wait_on_token(token).is_some());
        release_wait_channel(id);
    }

    #[test]
    fn wait_on_token_returns_none_for_test_placeholder_token() {
        // Existing test mocks (BlockingFs, LifecycleFs) construct tokens with
        // arbitrary numbers; those tokens must not panic in
        // wait_on_token; they return None so production await sites can
        // treat them as a sentinel.
        //
        // Use an obviously-out-of-range ID — Slice 3 (futex) registers
        // 256 carriers at zone-init time, claiming IDs 1..N for some
        // N that grows with each new wait-carrier producer. A
        // sentinel above the entire u32 space is never collisional.
        let token = WaitToken::new(u64::MAX - 1, 0x55);
        assert!(wait_on_token(token).is_none());
    }

    #[test]
    fn wait_on_token_returns_none_after_release() {
        let id = register_wait_channel(Channel::new());
        let token = WaitToken::new(id, 0x1);
        assert!(wait_on_token(token).is_some());
        release_wait_channel(id);
        let token = WaitToken::new(id, 0x1);
        assert!(wait_on_token(token).is_none());
    }
}
