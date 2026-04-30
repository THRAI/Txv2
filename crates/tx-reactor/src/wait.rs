//! Reactor wait channels and wait-adapt futures.

use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::interrupt::{InterruptSource, NoInterrupts};
use crate::timer::{DeadlineFuture, TimerQueue};
use tx_substrate::bus::{RawPort, RawPortSubscription};

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

/// Future returned by `Channel::wait`.
pub struct WaitFuture {
    channel: Channel,
    mask: Mask,
    subscription: Option<RawPortSubscription>,
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
