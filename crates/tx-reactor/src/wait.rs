//! Reactor wait channels and wait-adapt futures.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::adapter::bus_wire::{
    ActiveWait, DeclaredPort, DeclaredPortSubscription, DeclaredQueue, DeclaredQueueSubscription,
    DeclaredWireError, InterestMask, MailboxEvent, MailboxPollAction, RawPort, RawPortSubscription,
    TaskMailbox, WaitSourceId, WireDeclaration, WireDeclarationError, WireEventSet,
};
use crate::interrupt::{InterruptSource, NoInterrupts};
use alloc::sync::Arc;
use tx_services::time::{
    DeadlineNs, DeadlineRegistrar, DeadlineRegistrarHandle, TimerGuard, TimerRole, TimerTarget,
    TimerToken,
};

/// Bit mask naming the wait events a task cares about on a channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Mask(u64);

impl Mask {
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Script-selected wait policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitProtocol {
    Uninterruptible,
    Interruptible,
    Killable,
    /// Interruptible wait with an absolute nanosecond deadline.
    InterruptibleTimeout(u64),
    /// Killable wait with an absolute nanosecond deadline.
    KillableTimeout(u64),
}

impl WaitProtocol {
    pub const fn is_interruptible(self) -> bool {
        match self {
            Self::Interruptible | Self::InterruptibleTimeout(_) => true,
            Self::Uninterruptible | Self::Killable | Self::KillableTimeout(_) => false,
        }
    }

    pub const fn is_killable(self) -> bool {
        match self {
            Self::Killable | Self::KillableTimeout(_) => true,
            Self::Uninterruptible | Self::Interruptible | Self::InterruptibleTimeout(_) => false,
        }
    }

    const fn deadline_ns(self) -> Option<u64> {
        match self {
            Self::InterruptibleTimeout(deadline_ns) | Self::KillableTimeout(deadline_ns) => {
                Some(deadline_ns)
            }
            Self::Uninterruptible | Self::Interruptible | Self::Killable => None,
        }
    }

    fn classify_interrupt<I>(self, interrupts: &I) -> Option<WaitOutcome>
    where
        I: InterruptSource + ?Sized,
    {
        let summary = interrupts.interrupt_summary();
        if self.is_interruptible() && summary.deliverable_signal {
            Some(WaitOutcome::Interrupted)
        } else if self.is_killable() && summary.termination {
            Some(WaitOutcome::Killed)
        } else {
            None
        }
    }
}

/// Classified wait result shape from REACTOR_v0.
///
/// The v0 smoke implementation produces `Ready` and timeout outcomes. The
/// other variants name future interruption boundaries so callers do not grow
/// a boolean-only API that would have to be broken later.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitOutcome {
    Ready,
    Interrupted,
    Killed,
    TimedOut,
}

/// Reactor-owned wait channel.
///
/// Channels are publication surfaces, not truth sources: firing a mask wakes
/// matching waiters, and the resumed task is still responsible for
/// re-observing the semantic condition before committing work.
#[derive(Clone)]
pub struct Channel {
    port: RawPort,
    timer_registrar: Option<DeadlineRegistrarHandle>,
}

/// Reactor wait channel backed by a typed declared bus port.
pub struct DeclaredChannel<E> {
    port: DeclaredPort<E>,
    timer_registrar: Option<DeadlineRegistrarHandle>,
}

/// Reactor wait channel backed by a typed declared bus readiness queue.
pub struct DeclaredReadinessChannel<E> {
    queue: DeclaredQueue<E>,
    timer_registrar: Option<DeadlineRegistrarHandle>,
}

/// Future returned by `Channel::wait`.
pub struct WaitFuture {
    channel: Channel,
    mask: Mask,
    subscription: Option<RawPortSubscription>,
    mailbox: Option<Arc<TaskMailbox>>,
    active: Option<ActiveWait>,
}

/// Future returned by `DeclaredChannel::wait`.
pub struct DeclaredWaitFuture<E> {
    channel: DeclaredChannel<E>,
    interest: E,
    subscription: Option<DeclaredPortSubscription<E>>,
    mailbox: Option<Arc<TaskMailbox>>,
    active: Option<ActiveWait>,
}

/// Future returned by `DeclaredReadinessChannel::wait`.
pub struct DeclaredReadinessWaitFuture<E> {
    channel: DeclaredReadinessChannel<E>,
    interest: E,
    subscription: Option<DeclaredQueueSubscription<E>>,
    mailbox: Option<Arc<TaskMailbox>>,
    active: Option<ActiveWait>,
}

/// Future returned by `Channel::wait_event`.
pub struct WaitEventFuture<C, I = NoInterrupts> {
    channel: Channel,
    mask: Mask,
    protocol: WaitProtocol,
    interrupts: I,
    condition: C,
    wait: Option<WaitFuture>,
    timer: Option<ProtocolTimer>,
}

/// Future returned by `DeclaredChannel::wait_event`.
pub struct DeclaredWaitEventFuture<E, C, I = NoInterrupts> {
    channel: DeclaredChannel<E>,
    interest: E,
    protocol: WaitProtocol,
    interrupts: I,
    condition: C,
    wait: Option<DeclaredWaitFuture<E>>,
    timer: Option<ProtocolTimer>,
}

/// Future returned by `DeclaredReadinessChannel::wait_event`.
pub struct DeclaredReadinessWaitEventFuture<E, C, I = NoInterrupts> {
    channel: DeclaredReadinessChannel<E>,
    interest: E,
    protocol: WaitProtocol,
    interrupts: I,
    condition: C,
    wait: Option<DeclaredReadinessWaitFuture<E>>,
    timer: Option<ProtocolTimer>,
}

struct ProtocolTimer {
    registrar: DeadlineRegistrarHandle,
    deadline_ns: u64,
    mailbox: Option<Arc<TaskMailbox>>,
    guard: Option<TimerGuard>,
    token: Option<TimerToken>,
}

impl ProtocolTimer {
    fn new(registrar: DeadlineRegistrarHandle, deadline_ns: u64) -> Self {
        Self {
            registrar,
            deadline_ns,
            mailbox: None,
            guard: None,
            token: None,
        }
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<WaitOutcome> {
        let mailbox = ensure_wait_mailbox(&mut self.mailbox, cx);
        if self.guard.is_none() {
            let guard = self
                .registrar
                .register_deadline(
                    DeadlineNs::new(self.deadline_ns),
                    TimerRole::DeadlineAbort,
                    TimerTarget::TaskMailbox(Arc::downgrade(&mailbox)),
                )
                .expect("reactor wait deadline registration failed");
            self.token = Some(guard.token());
            self.guard = Some(guard);
        }

        if mailbox
            .poll_select(|event| match event {
                MailboxEvent::TimerFired { token } if Some(*token) == self.token => {
                    MailboxPollAction::Take
                }
                _ => MailboxPollAction::Keep,
            })
            .is_some()
        {
            self.guard = None;
            self.token = None;
            return Poll::Ready(WaitOutcome::TimedOut);
        }

        Poll::Pending
    }
}

fn ensure_wait_mailbox(
    mailbox: &mut Option<Arc<TaskMailbox>>,
    cx: &mut Context<'_>,
) -> Arc<TaskMailbox> {
    let mailbox = mailbox
        .get_or_insert_with(|| {
            crate::task::current_poll_task_mailbox().unwrap_or_else(|| Arc::new(TaskMailbox::new()))
        })
        .clone();
    mailbox.register_waker(cx.waker().clone());
    mailbox
}

fn active_wait_for(
    source: WaitSourceId,
    generation: crate::WaitGeneration,
    interests: u64,
) -> ActiveWait {
    ActiveWait::new(generation, source, InterestMask::new(interests))
}

fn source_wait_poll_action(active: ActiveWait, event: &MailboxEvent) -> MailboxPollAction {
    match event {
        MailboxEvent::SourceFired {
            source, generation, ..
        } if *source == active.source && *generation != active.generation => {
            MailboxPollAction::Drop
        }
        event if active.matches(event) => MailboxPollAction::Take,
        _ => MailboxPollAction::Keep,
    }
}

impl Channel {
    /// Creates an event-only wait channel.
    ///
    /// Timeout-capable wait channels are produced by `Reactor::channel()`
    /// so they share the reactor's timer registry.
    pub fn new() -> Self {
        Self {
            port: RawPort::new(),
            timer_registrar: None,
        }
    }

    /// Creates a channel wired to a reactor-owned timer registry.
    pub(crate) fn with_deadline_registrar(timer_registrar: DeadlineRegistrarHandle) -> Self {
        Self {
            port: RawPort::new(),
            timer_registrar: Some(timer_registrar),
        }
    }

    pub fn wait(&self, mask: Mask) -> WaitFuture {
        WaitFuture {
            channel: self.clone(),
            mask,
            subscription: None,
            mailbox: None,
            active: None,
        }
    }

    pub fn wait_event<C>(
        &self,
        mask: Mask,
        protocol: WaitProtocol,
        condition: C,
    ) -> WaitEventFuture<C, NoInterrupts>
    where
        C: FnMut() -> bool,
    {
        self.wait_event_with_interrupts(mask, protocol, NoInterrupts, condition)
    }

    pub fn wait_event_with_interrupts<C, I>(
        &self,
        mask: Mask,
        protocol: WaitProtocol,
        interrupts: I,
        condition: C,
    ) -> WaitEventFuture<C, I>
    where
        C: FnMut() -> bool,
        I: InterruptSource,
    {
        WaitEventFuture {
            channel: self.clone(),
            mask,
            protocol,
            interrupts,
            condition,
            wait: None,
            timer: None,
        }
    }

    pub fn fire(&self, mask: Mask) -> usize {
        if mask.is_empty() {
            return 0;
        }
        self.port.fire(mask.bits())
    }

    pub fn try_fire_with_post<F>(&self, mask: Mask, post: F) -> Result<usize, DeclaredWireError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        if mask.is_empty() {
            return Ok(0);
        }
        self.port
            .try_fire_with_post(mask.bits(), post)
            .map_err(DeclaredWireError::from)
    }

    pub fn fire_with_post<F>(&self, mask: Mask, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.try_fire_with_post(mask, post).unwrap_or(0)
    }
}

/// Fire a legacy wait `Channel` with a raw mask value, returning the
/// number of waiters woken.
///
/// Replaces the per-subsystem adapter pattern
/// `channel.fire(Mask::from_bits(mask_bits))`.
/// Future observation hooks (tracing, metrics) attach here in one place.
pub fn fire_legacy(channel: &Channel, mask_bits: u64) -> usize {
    channel.fire(Mask::from_bits(mask_bits))
}

impl<E> DeclaredChannel<E>
where
    E: WireEventSet + Send + Sync + 'static,
{
    /// Creates an event-only wait channel over a fresh declared bus port.
    ///
    /// Timeout-capable declared channels are produced by
    /// `Reactor::declared_channel` or `Reactor::declared_channel_from_port`.
    pub fn new(declaration: WireDeclaration<E>) -> Result<Self, WireDeclarationError> {
        Ok(Self::from_port(DeclaredPort::new(declaration)?))
    }

    /// Creates an event-only wait channel over an existing declared bus port.
    pub fn from_port(port: DeclaredPort<E>) -> Self {
        Self {
            port,
            timer_registrar: None,
        }
    }

    pub(crate) fn with_deadline_registrar(
        declaration: WireDeclaration<E>,
        timer_registrar: DeadlineRegistrarHandle,
    ) -> Result<Self, WireDeclarationError> {
        Ok(Self::from_port_with_deadline_registrar(
            DeclaredPort::new(declaration)?,
            timer_registrar,
        ))
    }

    pub(crate) fn from_port_with_deadline_registrar(
        port: DeclaredPort<E>,
        timer_registrar: DeadlineRegistrarHandle,
    ) -> Self {
        Self {
            port,
            timer_registrar: Some(timer_registrar),
        }
    }

    pub fn declaration(&self) -> WireDeclaration<E> {
        self.port.declaration()
    }

    pub fn port(&self) -> &DeclaredPort<E> {
        &self.port
    }

    pub fn try_wait(&self, interest: E) -> Result<DeclaredWaitFuture<E>, WireDeclarationError> {
        self.port.declaration().validate_bits(interest.bits())?;
        Ok(DeclaredWaitFuture {
            channel: self.clone(),
            interest,
            subscription: None,
            mailbox: None,
            active: None,
        })
    }

    pub fn wait(&self, interest: E) -> DeclaredWaitFuture<E> {
        self.try_wait(interest)
            .expect("declared reactor wait interest must use declared bits")
    }

    pub fn try_wait_event<C>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        condition: C,
    ) -> Result<DeclaredWaitEventFuture<E, C, NoInterrupts>, WireDeclarationError>
    where
        C: FnMut() -> bool,
    {
        self.try_wait_event_with_interrupts(interest, protocol, NoInterrupts, condition)
    }

    pub fn wait_event<C>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        condition: C,
    ) -> DeclaredWaitEventFuture<E, C, NoInterrupts>
    where
        C: FnMut() -> bool,
    {
        self.try_wait_event(interest, protocol, condition)
            .expect("declared reactor wait_event interest must use declared bits")
    }

    pub fn try_wait_event_with_interrupts<C, I>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        interrupts: I,
        condition: C,
    ) -> Result<DeclaredWaitEventFuture<E, C, I>, WireDeclarationError>
    where
        C: FnMut() -> bool,
        I: InterruptSource,
    {
        self.port.declaration().validate_bits(interest.bits())?;
        Ok(DeclaredWaitEventFuture {
            channel: self.clone(),
            interest,
            protocol,
            interrupts,
            condition,
            wait: None,
            timer: None,
        })
    }

    pub fn wait_event_with_interrupts<C, I>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        interrupts: I,
        condition: C,
    ) -> DeclaredWaitEventFuture<E, C, I>
    where
        C: FnMut() -> bool,
        I: InterruptSource,
    {
        self.try_wait_event_with_interrupts(interest, protocol, interrupts, condition)
            .expect("declared reactor wait_event interest must use declared bits")
    }

    pub fn try_fire(&self, event: E) -> Result<usize, DeclaredWireError> {
        self.port.try_fire(event)
    }

    pub fn try_fire_with_post<F>(&self, event: E, post: F) -> Result<usize, DeclaredWireError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.port.try_fire_with_post(event, post)
    }

    pub fn fire_with_post<F>(&self, event: E, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.try_fire_with_post(event, post).unwrap_or(0)
    }

    pub fn fire(&self, event: E) -> usize {
        self.port.fire(event)
    }
}

impl<E> Clone for DeclaredChannel<E> {
    fn clone(&self) -> Self {
        Self {
            port: self.port.clone(),
            timer_registrar: self.timer_registrar.clone(),
        }
    }
}

impl<E> DeclaredReadinessChannel<E>
where
    E: WireEventSet + Send + Sync + 'static,
{
    /// Creates an event-only wait channel over a fresh declared bus queue.
    ///
    /// Timeout-capable declared readiness channels are produced by
    /// `Reactor::declared_readiness_channel` or
    /// `Reactor::declared_readiness_channel_from_queue`.
    pub fn new(declaration: WireDeclaration<E>) -> Result<Self, WireDeclarationError> {
        Ok(Self::from_queue(DeclaredQueue::new(declaration)?))
    }

    /// Creates an event-only wait channel over an existing declared bus queue.
    pub fn from_queue(queue: DeclaredQueue<E>) -> Self {
        Self {
            queue,
            timer_registrar: None,
        }
    }

    pub(crate) fn with_deadline_registrar(
        declaration: WireDeclaration<E>,
        timer_registrar: DeadlineRegistrarHandle,
    ) -> Result<Self, WireDeclarationError> {
        Ok(Self::from_queue_with_deadline_registrar(
            DeclaredQueue::new(declaration)?,
            timer_registrar,
        ))
    }

    pub(crate) fn from_queue_with_deadline_registrar(
        queue: DeclaredQueue<E>,
        timer_registrar: DeadlineRegistrarHandle,
    ) -> Self {
        Self {
            queue,
            timer_registrar: Some(timer_registrar),
        }
    }

    pub fn declaration(&self) -> WireDeclaration<E> {
        self.queue.declaration()
    }

    pub fn queue(&self) -> &DeclaredQueue<E> {
        &self.queue
    }

    pub fn try_wait(
        &self,
        interest: E,
    ) -> Result<DeclaredReadinessWaitFuture<E>, WireDeclarationError> {
        self.queue.declaration().validate_bits(interest.bits())?;
        Ok(DeclaredReadinessWaitFuture {
            channel: self.clone(),
            interest,
            subscription: None,
            mailbox: None,
            active: None,
        })
    }

    pub fn wait(&self, interest: E) -> DeclaredReadinessWaitFuture<E> {
        self.try_wait(interest)
            .expect("declared reactor readiness interest must use declared bits")
    }

    pub fn try_wait_event<C>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        condition: C,
    ) -> Result<DeclaredReadinessWaitEventFuture<E, C, NoInterrupts>, WireDeclarationError>
    where
        C: FnMut() -> bool,
    {
        self.try_wait_event_with_interrupts(interest, protocol, NoInterrupts, condition)
    }

    pub fn wait_event<C>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        condition: C,
    ) -> DeclaredReadinessWaitEventFuture<E, C, NoInterrupts>
    where
        C: FnMut() -> bool,
    {
        self.try_wait_event(interest, protocol, condition)
            .expect("declared reactor readiness wait_event interest must use declared bits")
    }

    pub fn try_wait_event_with_interrupts<C, I>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        interrupts: I,
        condition: C,
    ) -> Result<DeclaredReadinessWaitEventFuture<E, C, I>, WireDeclarationError>
    where
        C: FnMut() -> bool,
        I: InterruptSource,
    {
        self.queue.declaration().validate_bits(interest.bits())?;
        Ok(DeclaredReadinessWaitEventFuture {
            channel: self.clone(),
            interest,
            protocol,
            interrupts,
            condition,
            wait: None,
            timer: None,
        })
    }

    pub fn wait_event_with_interrupts<C, I>(
        &self,
        interest: E,
        protocol: WaitProtocol,
        interrupts: I,
        condition: C,
    ) -> DeclaredReadinessWaitEventFuture<E, C, I>
    where
        C: FnMut() -> bool,
        I: InterruptSource,
    {
        self.try_wait_event_with_interrupts(interest, protocol, interrupts, condition)
            .expect("declared reactor readiness wait_event interest must use declared bits")
    }

    pub fn try_fire(&self, event: E) -> Result<usize, DeclaredWireError> {
        self.queue.try_fire(event)
    }

    pub fn try_fire_with_post<F>(&self, event: E, post: F) -> Result<usize, DeclaredWireError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.queue.try_fire_with_post(event, post)
    }

    pub fn fire_with_post<F>(&self, event: E, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.try_fire_with_post(event, post).unwrap_or(0)
    }

    pub fn fire(&self, event: E) -> usize {
        self.queue.fire(event)
    }

    pub fn try_clear(&self, event: E) -> Result<(), DeclaredWireError> {
        self.queue.try_clear(event)
    }

    pub fn clear(&self, event: E) {
        self.queue.clear(event);
    }

    pub fn peek_bits(&self) -> u64 {
        self.queue.peek_bits()
    }
}

impl<E> Clone for DeclaredReadinessChannel<E> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            timer_registrar: self.timer_registrar.clone(),
        }
    }
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl Future for WaitFuture {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.mask.is_empty() {
            return Poll::Ready(WaitOutcome::Ready);
        }
        let mailbox = ensure_wait_mailbox(&mut this.mailbox, cx);

        let ready = if let Some(subscription) = this.subscription.as_mut() {
            if this
                .active
                .and_then(|active| {
                    mailbox.poll_select(|event| source_wait_poll_action(active, event))
                })
                .is_some()
                || mailbox.take_overflow()
            {
                true
            } else {
                let generation = mailbox.next_generation();
                let active =
                    active_wait_for(this.channel.port.source_id(), generation, this.mask.bits());
                subscription.update(this.mask.bits(), Arc::downgrade(&mailbox), generation);
                this.active = Some(active);
                false
            }
        } else {
            let generation = mailbox.next_generation();
            let active =
                active_wait_for(this.channel.port.source_id(), generation, this.mask.bits());
            this.subscription = Some(this.channel.port.subscribe(
                this.mask.bits(),
                Arc::downgrade(&mailbox),
                generation,
            ));
            this.active = Some(active);
            false
        };

        if ready {
            this.subscription = None;
            this.active = None;
            return Poll::Ready(WaitOutcome::Ready);
        }

        Poll::Pending
    }
}

impl Drop for WaitFuture {
    fn drop(&mut self) {
        self.subscription = None;
        self.active = None;
    }
}

impl<E> Unpin for DeclaredWaitFuture<E> {}

impl<E> Future for DeclaredWaitFuture<E>
where
    E: WireEventSet + Send + Sync + 'static,
{
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.interest.bits() == 0 {
            return Poll::Ready(WaitOutcome::Ready);
        }
        let mailbox = ensure_wait_mailbox(&mut this.mailbox, cx);

        let ready = if let Some(subscription) = this.subscription.as_mut() {
            if this
                .active
                .and_then(|active| {
                    mailbox.poll_select(|event| source_wait_poll_action(active, event))
                })
                .is_some()
                || mailbox.take_overflow()
            {
                true
            } else {
                let generation = mailbox.next_generation();
                let active = active_wait_for(
                    this.channel.port.raw().source_id(),
                    generation,
                    this.interest.bits(),
                );
                subscription.update(this.interest, Arc::downgrade(&mailbox), generation);
                this.active = Some(active);
                false
            }
        } else {
            let generation = mailbox.next_generation();
            let active = active_wait_for(
                this.channel.port.raw().source_id(),
                generation,
                this.interest.bits(),
            );
            this.subscription = Some(this.channel.port.subscribe(
                this.interest,
                Arc::downgrade(&mailbox),
                generation,
            ));
            this.active = Some(active);
            false
        };

        if ready {
            this.subscription = None;
            this.active = None;
            return Poll::Ready(WaitOutcome::Ready);
        }

        Poll::Pending
    }
}

impl<E> Drop for DeclaredWaitFuture<E> {
    fn drop(&mut self) {
        self.subscription = None;
        self.active = None;
    }
}

impl<E> Unpin for DeclaredReadinessWaitFuture<E> {}

impl<E> Future for DeclaredReadinessWaitFuture<E>
where
    E: WireEventSet + Send + Sync + 'static,
{
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.interest.bits() == 0 {
            return Poll::Ready(WaitOutcome::Ready);
        }
        let mailbox = ensure_wait_mailbox(&mut this.mailbox, cx);

        let ready = if let Some(subscription) = this.subscription.as_mut() {
            if this
                .active
                .and_then(|active| {
                    mailbox.poll_select(|event| source_wait_poll_action(active, event))
                })
                .is_some()
                || mailbox.take_overflow()
            {
                true
            } else {
                let generation = mailbox.next_generation();
                let active = active_wait_for(
                    this.channel.queue.raw().source_id(),
                    generation,
                    this.interest.bits(),
                );
                subscription.update(this.interest, Arc::downgrade(&mailbox), generation);
                this.active = Some(active);
                false
            }
        } else {
            let generation = mailbox.next_generation();
            let active = active_wait_for(
                this.channel.queue.raw().source_id(),
                generation,
                this.interest.bits(),
            );
            this.subscription = Some(this.channel.queue.subscribe(
                this.interest,
                Arc::downgrade(&mailbox),
                generation,
            ));
            this.active = Some(active);
            false
        };

        if ready {
            this.subscription = None;
            this.active = None;
            return Poll::Ready(WaitOutcome::Ready);
        }

        Poll::Pending
    }
}

impl<E> Drop for DeclaredReadinessWaitFuture<E> {
    fn drop(&mut self) {
        self.subscription = None;
        self.active = None;
    }
}

impl<C, I> Future for WaitEventFuture<C, I>
where
    C: FnMut() -> bool + Unpin,
    I: InterruptSource + Unpin,
{
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let deadline_ns = this.protocol.deadline_ns();
        let mut woke = false;

        loop {
            if (this.condition)() {
                this.wait = None;
                this.timer = None;
                return Poll::Ready(WaitOutcome::Ready);
            }

            if woke {
                if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                    this.wait = None;
                    this.timer = None;
                    return Poll::Ready(outcome);
                }
            }

            if let Some(deadline_ns) = deadline_ns {
                if let Some(timer_registrar) = this.channel.timer_registrar.as_ref() {
                    let timer = this.timer.get_or_insert_with(|| {
                        ProtocolTimer::new(timer_registrar.clone(), deadline_ns)
                    });
                    if timer.poll(cx).is_ready() {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(WaitOutcome::TimedOut);
                    }
                }
            }

            if this.mask.is_empty() {
                if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                    this.wait = None;
                    this.timer = None;
                    return Poll::Ready(outcome);
                }
                return Poll::Pending;
            }

            let wait = this
                .wait
                .get_or_insert_with(|| this.channel.wait(this.mask));
            match Pin::new(wait).poll(cx) {
                Poll::Ready(WaitOutcome::Ready) => {
                    this.wait = None;
                    woke = true;
                }
                Poll::Ready(outcome) => {
                    this.wait = None;
                    return Poll::Ready(outcome);
                }
                Poll::Pending => {
                    if (this.condition)() {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(WaitOutcome::Ready);
                    }
                    if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(outcome);
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<E, C, I> Unpin for DeclaredWaitEventFuture<E, C, I>
where
    C: Unpin,
    I: Unpin,
{
}

impl<E, C, I> Future for DeclaredWaitEventFuture<E, C, I>
where
    E: WireEventSet + Send + Sync + 'static,
    C: FnMut() -> bool + Unpin,
    I: InterruptSource + Unpin,
{
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let deadline_ns = this.protocol.deadline_ns();
        let mut woke = false;

        loop {
            if (this.condition)() {
                this.wait = None;
                this.timer = None;
                return Poll::Ready(WaitOutcome::Ready);
            }

            if woke {
                if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                    this.wait = None;
                    this.timer = None;
                    return Poll::Ready(outcome);
                }
            }

            if let Some(deadline_ns) = deadline_ns {
                if let Some(timer_registrar) = this.channel.timer_registrar.as_ref() {
                    let timer = this.timer.get_or_insert_with(|| {
                        ProtocolTimer::new(timer_registrar.clone(), deadline_ns)
                    });
                    if timer.poll(cx).is_ready() {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(WaitOutcome::TimedOut);
                    }
                }
            }

            if this.interest.bits() == 0 {
                if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                    this.wait = None;
                    this.timer = None;
                    return Poll::Ready(outcome);
                }
                return Poll::Pending;
            }

            let wait = this
                .wait
                .get_or_insert_with(|| this.channel.wait(this.interest));
            match Pin::new(wait).poll(cx) {
                Poll::Ready(WaitOutcome::Ready) => {
                    this.wait = None;
                    woke = true;
                }
                Poll::Ready(outcome) => {
                    this.wait = None;
                    return Poll::Ready(outcome);
                }
                Poll::Pending => {
                    if (this.condition)() {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(WaitOutcome::Ready);
                    }
                    if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(outcome);
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

impl<E, C, I> Unpin for DeclaredReadinessWaitEventFuture<E, C, I>
where
    C: Unpin,
    I: Unpin,
{
}

impl<E, C, I> Future for DeclaredReadinessWaitEventFuture<E, C, I>
where
    E: WireEventSet + Send + Sync + 'static,
    C: FnMut() -> bool + Unpin,
    I: InterruptSource + Unpin,
{
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let deadline_ns = this.protocol.deadline_ns();
        let mut woke = false;

        loop {
            if (this.condition)() {
                this.wait = None;
                this.timer = None;
                return Poll::Ready(WaitOutcome::Ready);
            }

            if woke {
                if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                    this.wait = None;
                    this.timer = None;
                    return Poll::Ready(outcome);
                }
            }

            if let Some(deadline_ns) = deadline_ns {
                if let Some(timer_registrar) = this.channel.timer_registrar.as_ref() {
                    let timer = this.timer.get_or_insert_with(|| {
                        ProtocolTimer::new(timer_registrar.clone(), deadline_ns)
                    });
                    if timer.poll(cx).is_ready() {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(WaitOutcome::TimedOut);
                    }
                }
            }

            if this.interest.bits() == 0 {
                if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                    this.wait = None;
                    this.timer = None;
                    return Poll::Ready(outcome);
                }
                return Poll::Pending;
            }

            let wait = this
                .wait
                .get_or_insert_with(|| this.channel.wait(this.interest));
            match Pin::new(wait).poll(cx) {
                Poll::Ready(WaitOutcome::Ready) => {
                    this.wait = None;
                    woke = true;
                }
                Poll::Ready(outcome) => {
                    this.wait = None;
                    return Poll::Ready(outcome);
                }
                Poll::Pending => {
                    if (this.condition)() {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(WaitOutcome::Ready);
                    }
                    if let Some(outcome) = this.protocol.classify_interrupt(&this.interrupts) {
                        this.wait = None;
                        this.timer = None;
                        return Poll::Ready(outcome);
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}
