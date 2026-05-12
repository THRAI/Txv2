//! Reactor wait channels and wait-adapt futures.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::interrupt::{InterruptSource, NoInterrupts};
use crate::timer::{DeadlineFuture, TimerQueue};
use crate::adapter::bus_wire::{
    DeclaredPort, DeclaredPortSubscription, DeclaredQueue, DeclaredQueueSubscription,
    DeclaredWireError, RawPort, RawPortSubscription, WireDeclaration, WireDeclarationError,
    WireEventSet,
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
    timers: Option<TimerQueue>,
}

/// Reactor wait channel backed by a typed declared bus port.
pub struct DeclaredChannel<E> {
    port: DeclaredPort<E>,
    timers: Option<TimerQueue>,
}

/// Reactor wait channel backed by a typed declared bus readiness queue.
pub struct DeclaredReadinessChannel<E> {
    queue: DeclaredQueue<E>,
    timers: Option<TimerQueue>,
}

/// Future returned by `Channel::wait`.
pub struct WaitFuture {
    channel: Channel,
    mask: Mask,
    subscription: Option<RawPortSubscription>,
}

/// Future returned by `DeclaredChannel::wait`.
pub struct DeclaredWaitFuture<E> {
    channel: DeclaredChannel<E>,
    interest: E,
    subscription: Option<DeclaredPortSubscription<E>>,
}

/// Future returned by `DeclaredReadinessChannel::wait`.
pub struct DeclaredReadinessWaitFuture<E> {
    channel: DeclaredReadinessChannel<E>,
    interest: E,
    subscription: Option<DeclaredQueueSubscription<E>>,
}

/// Future returned by `Channel::wait_event`.
pub struct WaitEventFuture<C, I = NoInterrupts> {
    channel: Channel,
    mask: Mask,
    protocol: WaitProtocol,
    interrupts: I,
    condition: C,
    wait: Option<WaitFuture>,
    timer: Option<DeadlineFuture>,
}

/// Future returned by `DeclaredChannel::wait_event`.
pub struct DeclaredWaitEventFuture<E, C, I = NoInterrupts> {
    channel: DeclaredChannel<E>,
    interest: E,
    protocol: WaitProtocol,
    interrupts: I,
    condition: C,
    wait: Option<DeclaredWaitFuture<E>>,
    timer: Option<DeadlineFuture>,
}

/// Future returned by `DeclaredReadinessChannel::wait_event`.
pub struct DeclaredReadinessWaitEventFuture<E, C, I = NoInterrupts> {
    channel: DeclaredReadinessChannel<E>,
    interest: E,
    protocol: WaitProtocol,
    interrupts: I,
    condition: C,
    wait: Option<DeclaredReadinessWaitFuture<E>>,
    timer: Option<DeadlineFuture>,
}

impl Channel {
    /// Creates an event-only wait channel.
    ///
    /// Timeout-capable wait channels are produced by `Reactor::channel()`
    /// so they share the reactor's deadline queue and clock source.
    pub fn new() -> Self {
        Self {
            port: RawPort::new(),
            timers: None,
        }
    }

    /// Creates a channel wired to a reactor-owned timer queue.
    pub(crate) fn with_timer_queue(timers: TimerQueue) -> Self {
        Self {
            port: RawPort::new(),
            timers: Some(timers),
        }
    }

    pub fn wait(&self, mask: Mask) -> WaitFuture {
        WaitFuture {
            channel: self.clone(),
            mask,
            subscription: None,
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
        Self { port, timers: None }
    }

    pub(crate) fn with_timer_queue(
        declaration: WireDeclaration<E>,
        timers: TimerQueue,
    ) -> Result<Self, WireDeclarationError> {
        Ok(Self::from_port_with_timer_queue(
            DeclaredPort::new(declaration)?,
            timers,
        ))
    }

    pub(crate) fn from_port_with_timer_queue(port: DeclaredPort<E>, timers: TimerQueue) -> Self {
        Self {
            port,
            timers: Some(timers),
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

    pub fn fire(&self, event: E) -> usize {
        self.port.fire(event)
    }
}

impl<E> Clone for DeclaredChannel<E> {
    fn clone(&self) -> Self {
        Self {
            port: self.port.clone(),
            timers: self.timers.clone(),
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
            timers: None,
        }
    }

    pub(crate) fn with_timer_queue(
        declaration: WireDeclaration<E>,
        timers: TimerQueue,
    ) -> Result<Self, WireDeclarationError> {
        Ok(Self::from_queue_with_timer_queue(
            DeclaredQueue::new(declaration)?,
            timers,
        ))
    }

    pub(crate) fn from_queue_with_timer_queue(queue: DeclaredQueue<E>, timers: TimerQueue) -> Self {
        Self {
            queue,
            timers: Some(timers),
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
            timers: self.timers.clone(),
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

        let ready = if let Some(subscription) = this.subscription.as_mut() {
            if subscription.take_ready() {
                true
            } else {
                subscription.update(this.mask.bits(), cx.waker().clone());
                false
            }
        } else {
            this.subscription = Some(
                this.channel
                    .port
                    .subscribe(this.mask.bits(), cx.waker().clone()),
            );
            false
        };

        if ready {
            this.subscription = None;
            return Poll::Ready(WaitOutcome::Ready);
        }

        Poll::Pending
    }
}

impl Drop for WaitFuture {
    fn drop(&mut self) {
        self.subscription = None;
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

        let ready = if let Some(subscription) = this.subscription.as_mut() {
            if subscription.take_ready() {
                true
            } else {
                subscription.update(this.interest, cx.waker().clone());
                false
            }
        } else {
            this.subscription = Some(
                this.channel
                    .port
                    .subscribe(this.interest, cx.waker().clone()),
            );
            false
        };

        if ready {
            this.subscription = None;
            return Poll::Ready(WaitOutcome::Ready);
        }

        Poll::Pending
    }
}

impl<E> Drop for DeclaredWaitFuture<E> {
    fn drop(&mut self) {
        self.subscription = None;
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

        let ready = if let Some(subscription) = this.subscription.as_mut() {
            if subscription.take_ready() {
                true
            } else {
                subscription.update(this.interest, cx.waker().clone());
                false
            }
        } else {
            this.subscription = Some(
                this.channel
                    .queue
                    .subscribe(this.interest, cx.waker().clone()),
            );
            false
        };

        if ready {
            this.subscription = None;
            return Poll::Ready(WaitOutcome::Ready);
        }

        Poll::Pending
    }
}

impl<E> Drop for DeclaredReadinessWaitFuture<E> {
    fn drop(&mut self) {
        self.subscription = None;
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
                if let Some(timers) = this.channel.timers.as_ref() {
                    let timer = this
                        .timer
                        .get_or_insert_with(|| timers.wait_until(deadline_ns));
                    if Pin::new(timer).poll(cx).is_ready() {
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
                if let Some(timers) = this.channel.timers.as_ref() {
                    let timer = this
                        .timer
                        .get_or_insert_with(|| timers.wait_until(deadline_ns));
                    if Pin::new(timer).poll(cx).is_ready() {
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
                if let Some(timers) = this.channel.timers.as_ref() {
                    let timer = this
                        .timer
                        .get_or_insert_with(|| timers.wait_until(deadline_ns));
                    if Pin::new(timer).poll(cx).is_ready() {
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
