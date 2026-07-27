//! Reactor-local synchronous coordination primitives.

use alloc::{sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::adapter::bus_wire::{MailboxEvent, TaskMailbox};
use crate::spin_lock::SpinLock;
use crate::wait::{Channel, Mask, WaitFuture, WaitOutcome};

const COMPLETE_MASK: Mask = Mask::from_bits(0x1);

/// Lightweight reactor token naming a synchronous coordination target.
///
/// This is temporal coordination identity only. It is not a subsystem entity,
/// capability, hart object, or HAL CPU identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct SyncTargetToken(u32);

impl SyncTargetToken {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Result class for an acknowledgment attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AckOutcome {
    /// The target was in the closed set and transitioned to acknowledged.
    Acknowledged,
    /// The target was known, but had already acknowledged this rendezvous.
    Duplicate,
    /// The token was not in the rendezvous target set.
    UnknownTarget,
}

/// Acknowledgment result plus the rendezvous state after the attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AckResult {
    pub outcome: AckOutcome,
    pub completed: bool,
    pub remaining: usize,
}

/// Closed-set synchronous rendezvous for shootdown-style acknowledgments.
///
/// The primitive owns only reactor-local coordination state. Issuing IPIs,
/// performing TLB invalidation, and post-shootdown substrate accounting are
/// intentionally outside this type.
#[derive(Clone)]
pub struct SyncRendezvous {
    state: Arc<SpinLock<RendezvousState>>,
    completion: Channel,
}

struct RendezvousState {
    targets: Vec<TargetAck>,
    remaining: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TargetAck {
    target: SyncTargetToken,
    acked: bool,
}

/// Future that resolves once every target in a rendezvous has acknowledged.
pub struct SyncRendezvousWait {
    rendezvous: SyncRendezvous,
    wait: Option<WaitFuture>,
}

impl SyncRendezvous {
    /// Create a rendezvous over a closed target set.
    ///
    /// Empty target sets are admitted and are complete immediately. Repeated
    /// target tokens in the input collapse to one target.
    pub fn new<I>(targets: I) -> Self
    where
        I: IntoIterator<Item = SyncTargetToken>,
    {
        Self {
            state: Arc::new(SpinLock::new(RendezvousState::new(targets))),
            completion: Channel::new(),
        }
    }

    pub fn target_count(&self) -> usize {
        self.state.lock().targets.len()
    }

    pub fn remaining(&self) -> usize {
        self.state.lock().remaining
    }

    pub fn is_complete(&self) -> bool {
        self.remaining() == 0
    }

    pub fn is_acknowledged(&self, target: SyncTargetToken) -> Option<bool> {
        self.state
            .lock()
            .targets
            .iter()
            .find(|entry| entry.target == target)
            .map(|entry| entry.acked)
    }

    pub fn ack_with_post<F>(&self, target: SyncTargetToken, post: F) -> (AckResult, usize)
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let mut fire_completion = false;
        let result = {
            let mut state = self.state.lock();
            let Some(index) = state
                .targets
                .iter()
                .position(|entry| entry.target == target)
            else {
                return (
                    AckResult {
                        outcome: AckOutcome::UnknownTarget,
                        completed: state.remaining == 0,
                        remaining: state.remaining,
                    },
                    0,
                );
            };

            if state.targets[index].acked {
                AckResult {
                    outcome: AckOutcome::Duplicate,
                    completed: state.remaining == 0,
                    remaining: state.remaining,
                }
            } else {
                state.targets[index].acked = true;
                state.remaining -= 1;
                fire_completion = state.remaining == 0;
                AckResult {
                    outcome: AckOutcome::Acknowledged,
                    completed: state.remaining == 0,
                    remaining: state.remaining,
                }
            }
        };

        let woken = if fire_completion {
            self.completion.fire_with_post(COMPLETE_MASK, post)
        } else {
            0
        };

        (result, woken)
    }

    pub fn wait(&self) -> SyncRendezvousWait {
        SyncRendezvousWait {
            rendezvous: self.clone(),
            wait: None,
        }
    }
}

impl RendezvousState {
    fn new<I>(targets: I) -> Self
    where
        I: IntoIterator<Item = SyncTargetToken>,
    {
        let mut targets: Vec<_> = targets
            .into_iter()
            .map(|target| TargetAck {
                target,
                acked: false,
            })
            .collect();
        targets.sort_unstable_by_key(|entry| entry.target);
        targets.dedup_by_key(|entry| entry.target);
        let remaining = targets.len();

        Self { targets, remaining }
    }
}

impl Future for SyncRendezvousWait {
    type Output = WaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            if this.rendezvous.is_complete() {
                this.wait = None;
                return Poll::Ready(WaitOutcome::Ready);
            }

            let wait = this
                .wait
                .get_or_insert_with(|| this.rendezvous.completion.wait(COMPLETE_MASK));
            match Pin::new(wait).poll(cx) {
                Poll::Ready(WaitOutcome::Ready) => {
                    this.wait = None;
                }
                Poll::Ready(outcome) => {
                    this.wait = None;
                    return Poll::Ready(outcome);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
