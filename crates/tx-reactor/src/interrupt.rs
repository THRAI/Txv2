//! Reactor-local interrupt summary predicates for wait classification.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, Ordering};

const DELIVERABLE_SIGNAL: u8 = 0b001;
const TERMINATION: u8 = 0b010;
const STOP_REQUESTED: u8 = 0b100;

/// Cheap denormalized interrupt state consumed by wait-adapt.
///
/// This is not signal ownership. It intentionally carries only the predicate
/// state THREAD_RUNTIME_v1 says the reactor may consult while classifying a
/// blocked wait.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InterruptSummary {
    pub deliverable_signal: bool,
    pub termination: bool,
    pub stop_requested: bool,
}

impl InterruptSummary {
    pub const fn new(deliverable_signal: bool, termination: bool, stop_requested: bool) -> Self {
        Self {
            deliverable_signal,
            termination,
            stop_requested,
        }
    }

    pub const fn none() -> Self {
        Self::new(false, false, false)
    }

    pub const fn bits(self) -> u8 {
        let mut bits = 0;
        if self.deliverable_signal {
            bits |= DELIVERABLE_SIGNAL;
        }
        if self.termination {
            bits |= TERMINATION;
        }
        if self.stop_requested {
            bits |= STOP_REQUESTED;
        }
        bits
    }

    pub const fn from_bits(bits: u8) -> Self {
        Self {
            deliverable_signal: bits & DELIVERABLE_SIGNAL != 0,
            termination: bits & TERMINATION != 0,
            stop_requested: bits & STOP_REQUESTED != 0,
        }
    }
}

/// Predicate surface wait-adapt needs from thread-runtime-owned state.
pub trait InterruptSource {
    fn deliverable_signal_pending(&self) -> bool;
    fn termination_in_force(&self) -> bool;
    fn stop_requested(&self) -> bool;

    fn interrupt_summary(&self) -> InterruptSummary {
        InterruptSummary {
            deliverable_signal: self.deliverable_signal_pending(),
            termination: self.termination_in_force(),
            stop_requested: self.stop_requested(),
        }
    }
}

impl InterruptSource for InterruptSummary {
    fn deliverable_signal_pending(&self) -> bool {
        self.deliverable_signal
    }

    fn termination_in_force(&self) -> bool {
        self.termination
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested
    }

    fn interrupt_summary(&self) -> InterruptSummary {
        *self
    }
}

/// Atomic host/runtime-friendly summary storage.
pub struct AtomicInterruptSummary {
    bits: AtomicU8,
}

impl AtomicInterruptSummary {
    pub const fn new() -> Self {
        Self::from_summary(InterruptSummary::none())
    }

    pub const fn from_summary(summary: InterruptSummary) -> Self {
        Self {
            bits: AtomicU8::new(summary.bits()),
        }
    }

    pub fn load(&self) -> InterruptSummary {
        InterruptSummary::from_bits(self.bits.load(Ordering::Acquire))
    }

    pub fn store(&self, summary: InterruptSummary) {
        self.bits.store(summary.bits(), Ordering::Release);
    }

    pub fn clear(&self) {
        self.store(InterruptSummary::none());
    }

    pub fn set_deliverable_signal(&self, pending: bool) {
        self.set_bit(DELIVERABLE_SIGNAL, pending);
    }

    pub fn set_termination(&self, in_force: bool) {
        self.set_bit(TERMINATION, in_force);
    }

    pub fn set_stop_requested(&self, requested: bool) {
        self.set_bit(STOP_REQUESTED, requested);
    }

    fn set_bit(&self, bit: u8, enabled: bool) {
        if enabled {
            self.bits.fetch_or(bit, Ordering::AcqRel);
        } else {
            self.bits.fetch_and(!bit, Ordering::AcqRel);
        }
    }
}

impl Default for AtomicInterruptSummary {
    fn default() -> Self {
        Self::new()
    }
}

impl InterruptSource for AtomicInterruptSummary {
    fn deliverable_signal_pending(&self) -> bool {
        self.load().deliverable_signal
    }

    fn termination_in_force(&self) -> bool {
        self.load().termination
    }

    fn stop_requested(&self) -> bool {
        self.load().stop_requested
    }

    fn interrupt_summary(&self) -> InterruptSummary {
        self.load()
    }
}

impl<T> InterruptSource for &T
where
    T: InterruptSource + ?Sized,
{
    fn deliverable_signal_pending(&self) -> bool {
        T::deliverable_signal_pending(self)
    }

    fn termination_in_force(&self) -> bool {
        T::termination_in_force(self)
    }

    fn stop_requested(&self) -> bool {
        T::stop_requested(self)
    }

    fn interrupt_summary(&self) -> InterruptSummary {
        T::interrupt_summary(self)
    }
}

impl<T> InterruptSource for Arc<T>
where
    T: InterruptSource + ?Sized,
{
    fn deliverable_signal_pending(&self) -> bool {
        T::deliverable_signal_pending(self)
    }

    fn termination_in_force(&self) -> bool {
        T::termination_in_force(self)
    }

    fn stop_requested(&self) -> bool {
        T::stop_requested(self)
    }

    fn interrupt_summary(&self) -> InterruptSummary {
        T::interrupt_summary(self)
    }
}

/// Empty source for waits that are not tied to a thread-runtime payload.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoInterrupts;

impl InterruptSource for NoInterrupts {
    fn deliverable_signal_pending(&self) -> bool {
        false
    }

    fn termination_in_force(&self) -> bool {
        false
    }

    fn stop_requested(&self) -> bool {
        false
    }
}
