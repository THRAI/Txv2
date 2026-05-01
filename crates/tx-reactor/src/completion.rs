//! Completion wait middleware.

use core::{
    num::NonZeroU32,
    sync::atomic::{AtomicU32, Ordering},
};

use crate::wait::{Channel, Mask, WaitOutcome, WaitProtocol};

const DONE: Mask = Mask::from_bits(0x1);

/// Counted completion credits over a private wait channel.
///
/// The credit counter is the truth. The channel only wakes waiters so they can
/// re-run the consuming condition through `Channel::wait_event`.
pub struct Completion {
    credits: AtomicU32,
    channel: Channel,
}

impl Completion {
    pub fn new() -> Self {
        Self::with_channel(Channel::new())
    }

    /// Creates a completion over a caller-supplied wake carrier.
    ///
    /// This keeps the completion logic independent from timer ownership: host
    /// tests and reactor-owned callers can pass a timer-backed channel, while
    /// default completions remain event-only.
    pub fn with_channel(channel: Channel) -> Self {
        Self {
            credits: AtomicU32::new(0),
            channel,
        }
    }

    pub fn complete(&self) {
        self.credits
            .fetch_update(Ordering::Release, Ordering::Relaxed, |credits| {
                credits.checked_add(1)
            })
            .expect("completion credit overflow");
        self.channel.fire(DONE);
    }

    pub fn try_consume(&self) -> bool {
        self.credits
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |credits| {
                credits.checked_sub(1)
            })
            .is_ok()
    }

    pub async fn wait(&self, protocol: WaitProtocol) -> WaitOutcome {
        self.channel
            .wait_event(DONE, protocol, || self.try_consume())
            .await
    }
}

impl Default for Completion {
    fn default() -> Self {
        Self::new()
    }
}

/// Closed-set countdown completion.
///
/// Participants are fixed at construction. Each successful `arrive` consumes
/// one participant slot; the `1 -> 0` transition wakes waiters.
pub struct CountdownCompletion {
    remaining: AtomicU32,
    channel: Channel,
}

impl CountdownCompletion {
    pub fn new(count: NonZeroU32) -> Self {
        Self::with_channel(count, Channel::new())
    }

    /// Creates a countdown completion over a caller-supplied wake carrier.
    pub fn with_channel(count: NonZeroU32, channel: Channel) -> Self {
        Self {
            remaining: AtomicU32::new(count.get()),
            channel,
        }
    }

    pub fn arrive(&self) {
        let previous = self
            .remaining
            .fetch_update(Ordering::Release, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .expect("countdown completion underflow");

        if previous == 1 {
            self.channel.fire(DONE);
        }
    }

    pub fn is_complete(&self) -> bool {
        self.remaining.load(Ordering::Acquire) == 0
    }

    pub async fn wait(&self, protocol: WaitProtocol) -> WaitOutcome {
        self.channel
            .wait_event(DONE, protocol, || self.is_complete())
            .await
    }
}
