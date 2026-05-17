use alloc::sync::{Arc, Weak};
use core::{
    cell::UnsafeCell,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::step::{InterestMask, WaitSourceId};
use crate::wake::mailbox::{TaskMailbox, WaitGeneration};
use tx_hal::CpuId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SubscriptionId(pub(super) usize);

/// Error returned by fallible wire operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawWireError {
    /// The wire has already entered its terminal state.
    Terminal,
}

/// Error returned by fallible subscription operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawSubscriptionError {
    /// The subscription is no longer registered on its wire.
    Unsubscribed,
    /// The wire has entered its terminal state.
    Terminal,
}

/// Observable state of a raw bus subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawSubscriptionState {
    /// The subscription is currently registered on an open wire.
    Subscribed,
    /// The subscription was removed or was never registered.
    Unsubscribed,
    /// The wire entered its terminal state.
    Terminal,
}

/// Result of an epoch-fenced wire retirement handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WireRetirement {
    kind: WireKind,
    terminal_bits: u64,
    woken: usize,
    newly_terminal: bool,
    guard_epoch: u64,
    guard_cpu: CpuId,
}

impl WireRetirement {
    pub const fn kind(self) -> WireKind {
        self.kind
    }

    pub const fn terminal_bits(self) -> u64 {
        self.terminal_bits
    }

    pub const fn woken(self) -> usize {
        self.woken
    }

    pub const fn newly_terminal(self) -> bool {
        self.newly_terminal
    }

    pub const fn guard_epoch(self) -> u64 {
        self.guard_epoch
    }

    pub const fn guard_cpu(self) -> CpuId {
        self.guard_cpu
    }

    pub(super) fn new(
        kind: WireKind,
        terminal_bits: u64,
        outcome: TerminateOutcome,
        guard: &crate::epoch::Guard<'_>,
    ) -> Self {
        Self {
            kind,
            terminal_bits,
            woken: outcome.woken,
            newly_terminal: outcome.newly_terminal,
            guard_epoch: guard.entered_epoch(),
            guard_cpu: guard.cpu_id(),
        }
    }
}

/// Static declaration kind for a typed bus wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireKind {
    /// Level-triggered readiness backed by [`RawQueue`].
    Queue,
    /// Edge-triggered event delivery backed by [`RawPort`].
    Port,
}

/// Typed event or readiness-mask set accepted by a declared bus wire.
///
/// Implementations are small copyable values, typically bitflag newtypes or
/// closed enums. `DECLARED_BITS` is the static set published by the owning
/// capability type; each value returned by [`WireEventSet::bits`] must be a
/// subset of that set.
pub trait WireEventSet: Copy {
    const DECLARED_BITS: u64;

    fn bits(self) -> u64;
}

/// Error returned when a typed wire declaration or event mask is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireDeclarationError {
    /// The declaration exposes no event bits.
    EmptyDeclaration,
    /// A fired event or subscription interest names bits outside the
    /// declaration.
    UndeclaredBits,
}

/// Error returned by typed wire operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclaredWireError {
    /// The typed declaration rejected the requested bits.
    Declaration(WireDeclarationError),
    /// The backing raw wire rejected the operation.
    Raw(RawWireError),
}

impl From<WireDeclarationError> for DeclaredWireError {
    fn from(value: WireDeclarationError) -> Self {
        Self::Declaration(value)
    }
}

impl From<RawWireError> for DeclaredWireError {
    fn from(value: RawWireError) -> Self {
        Self::Raw(value)
    }
}

/// Error returned by typed subscription operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclaredSubscriptionError {
    /// The typed declaration rejected the requested bits.
    Declaration(WireDeclarationError),
    /// The backing raw subscription rejected the operation.
    Raw(RawSubscriptionError),
}

impl From<WireDeclarationError> for DeclaredSubscriptionError {
    fn from(value: WireDeclarationError) -> Self {
        Self::Declaration(value)
    }
}

impl From<RawSubscriptionError> for DeclaredSubscriptionError {
    fn from(value: RawSubscriptionError) -> Self {
        Self::Raw(value)
    }
}

/// Static typed declaration shared by all instances of one wire shape.
#[derive(Debug, Eq, PartialEq)]
pub struct WireDeclaration<E> {
    name: &'static str,
    kind: WireKind,
    declared_bits: u64,
    _events: PhantomData<fn() -> E>,
}

impl<E> Clone for WireDeclaration<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for WireDeclaration<E> {}

impl<E: WireEventSet> WireDeclaration<E> {
    pub const fn queue(name: &'static str) -> Self {
        Self {
            name,
            kind: WireKind::Queue,
            declared_bits: E::DECLARED_BITS,
            _events: PhantomData,
        }
    }

    pub const fn port(name: &'static str) -> Self {
        Self {
            name,
            kind: WireKind::Port,
            declared_bits: E::DECLARED_BITS,
            _events: PhantomData,
        }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    pub const fn kind(self) -> WireKind {
        self.kind
    }

    pub const fn declared_bits(self) -> u64 {
        self.declared_bits
    }

    pub fn validate(self) -> Result<(), WireDeclarationError> {
        if self.declared_bits == 0 {
            Err(WireDeclarationError::EmptyDeclaration)
        } else {
            Ok(())
        }
    }

    pub fn validate_bits(self, bits: u64) -> Result<(), WireDeclarationError> {
        self.validate()?;
        if bits & !self.declared_bits == 0 {
            Ok(())
        } else {
            Err(WireDeclarationError::UndeclaredBits)
        }
    }
}

/// Subscriber entry stored in bus queue/port subscriber lists.
///
/// Each subscriber represents a task waiting on this wire. When the wire
/// fires matching bits, the subscriber's [`TaskMailbox`] receives a
/// [`MailboxEvent::SourceFired`] carrying the wire's [`WaitSourceId`],
/// the matching interest mask, and the subscriber's captured generation.
/// The task's driver compares the generation against its `ActiveWait`
/// to detect stale deliveries (wrap-around-safe monotonic).
pub(super) struct Subscriber {
    pub(super) id: SubscriptionId,
    pub(super) interest: u64,
    /// Weak handle to the subscriber task's mailbox. `fire()` upgrades
    /// and posts to live mailboxes; dead subscribers are cleaned up
    /// lazily (mailbox dropped = subscriber silent-noop).
    pub(super) mailbox: Weak<TaskMailbox>,
    /// Generation captured when the subscriber registered or last
    /// updated. The mailbox event carries this value so the driver
    /// can reject stale deliveries.
    pub(super) generation: WaitGeneration,
}

/// Convenience: post a [`MailboxEvent::SourceFired`] to a subscriber.
///
/// Returns `true` if the subscriber's mailbox was alive and received
/// the post; `false` if the subscriber has been dropped (dead mailbox,
/// lazy cleanup candidate).
#[inline]
pub(super) fn post_source_fired(
    subscriber: &Subscriber,
    source: WaitSourceId,
    interests: InterestMask,
) -> bool {
    if let Some(mailbox) = subscriber.mailbox.upgrade() {
        mailbox.post(crate::wake::mailbox::MailboxEvent::SourceFired {
            generation: subscriber.generation,
            source,
            interests,
        })
    } else {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TerminateOutcome {
    pub(super) woken: usize,
    pub(super) newly_terminal: bool,
}

pub(super) enum RawWireStorage<T: 'static> {
    Shared(Arc<SpinLock<T>>),
    Static(&'static SpinLock<T>),
}

impl<T> Clone for RawWireStorage<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Shared(state) => Self::Shared(Arc::clone(state)),
            Self::Static(state) => Self::Static(state),
        }
    }
}

impl<T> RawWireStorage<T> {
    pub(super) fn lock(&self) -> SpinLockGuard<'_, T> {
        match self {
            Self::Shared(state) => state.lock(),
            Self::Static(state) => state.lock(),
        }
    }
}

pub(super) struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

pub(super) struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub(super) const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        SpinLockGuard { lock: self }
    }
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
