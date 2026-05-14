use core::marker::PhantomData;
use alloc::sync::Weak;
use crate::wake::mailbox::{TaskMailbox, WaitGeneration};
use core::task::Waker;

use super::common::{
    RawSubscriptionError, RawSubscriptionState, RawWireError, WireDeclaration,
    WireDeclarationError, WireEventSet, WireKind,
};
use super::port::{DeclaredPort, RawPort, RawPortSubscription};
use super::queue::{DeclaredQueue, RawQueue, RawQueueSubscription};

/// Generation-checked handle for a long-lived bus subscription graph entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionGraphKey {
    index: usize,
    generation: u64,
}

impl SubscriptionGraphKey {
    pub const fn index(self) -> usize {
        self.index
    }

    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Typed wrapper around a long-lived bus subscription graph key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeclaredSubscriptionGraphKey<E> {
    raw: SubscriptionGraphKey,
    _events: PhantomData<fn() -> E>,
}

impl<E> DeclaredSubscriptionGraphKey<E> {
    const fn new(raw: SubscriptionGraphKey) -> Self {
        Self {
            raw,
            _events: PhantomData,
        }
    }

    pub const fn raw(self) -> SubscriptionGraphKey {
        self.raw
    }

    pub const fn index(self) -> usize {
        self.raw.index()
    }

    pub const fn generation(self) -> u64 {
        self.raw.generation()
    }
}

/// One ready or terminal entry discovered during an epoll-style graph scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionGraphReady {
    key: SubscriptionGraphKey,
    kind: WireKind,
    state: RawSubscriptionState,
}

impl SubscriptionGraphReady {
    pub const fn key(self) -> SubscriptionGraphKey {
        self.key
    }

    pub const fn kind(self) -> WireKind {
        self.kind
    }

    pub const fn state(self) -> RawSubscriptionState {
        self.state
    }
}

impl Default for SubscriptionGraphReady {
    fn default() -> Self {
        Self {
            key: SubscriptionGraphKey {
                index: usize::MAX,
                generation: 0,
            },
            kind: WireKind::Queue,
            state: RawSubscriptionState::Unsubscribed,
        }
    }
}

/// Error returned by long-lived subscription graph operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionGraphError {
    Full,
    EmptyInterest,
    StaleKey,
    KindMismatch,
    Declaration(WireDeclarationError),
    RawWire(RawWireError),
    RawSubscription(RawSubscriptionError),
}

impl From<WireDeclarationError> for SubscriptionGraphError {
    fn from(value: WireDeclarationError) -> Self {
        Self::Declaration(value)
    }
}

impl From<RawWireError> for SubscriptionGraphError {
    fn from(value: RawWireError) -> Self {
        Self::RawWire(value)
    }
}

impl From<RawSubscriptionError> for SubscriptionGraphError {
    fn from(value: RawSubscriptionError) -> Self {
        Self::RawSubscription(value)
    }
}

/// Bounded owner for epoll-style long-lived raw queue/port subscriptions.
///
/// This is a first subscription-graph slice: it owns subscription tokens,
/// generation-checks user handles, and lets the eventual epoll layer update,
/// remove, and consume readiness without open-coding token lifetimes.
pub struct SubscriptionGraph<const N: usize> {
    entries: [Option<GraphEntry>; N],
    next_generation: u64,
    len: usize,
}

struct GraphEntry {
    generation: u64,
    subscription: GraphSubscription,
}

enum GraphSubscription {
    Queue(RawQueueSubscription),
    Port(RawPortSubscription),
}

impl<const N: usize> SubscriptionGraph<N> {
    pub const fn new() -> Self {
        Self {
            entries: [const { None }; N],
            next_generation: 1,
            len: 0,
        }
    }

    pub const fn capacity(&self) -> usize {
        N
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn subscribe_queue(
        &mut self,
        queue: &RawQueue,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<SubscriptionGraphKey, SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        let index = self.free_index().ok_or(SubscriptionGraphError::Full)?;
        let subscription = queue.try_subscribe(interest, mailbox, generation)?;
        Ok(self.insert_at(index, GraphSubscription::Queue(subscription)))
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn subscribe_queue_with_waker(
        &mut self,
        queue: &RawQueue,
        interest: u64,
        waker: Waker,
    ) -> Result<SubscriptionGraphKey, SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        let index = self.free_index().ok_or(SubscriptionGraphError::Full)?;
        let subscription = queue.try_subscribe_with_waker(interest, waker)?;
        Ok(self.insert_at(index, GraphSubscription::Queue(subscription)))
    }

    pub fn subscribe_port(
        &mut self,
        port: &RawPort,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<SubscriptionGraphKey, SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        let index = self.free_index().ok_or(SubscriptionGraphError::Full)?;
        let subscription = port.try_subscribe(interest, mailbox, generation)?;
        Ok(self.insert_at(index, GraphSubscription::Port(subscription)))
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn subscribe_port_with_waker(
        &mut self,
        port: &RawPort,
        interest: u64,
        waker: Waker,
    ) -> Result<SubscriptionGraphKey, SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        let index = self.free_index().ok_or(SubscriptionGraphError::Full)?;
        let subscription = port.try_subscribe_with_waker(interest, waker)?;
        Ok(self.insert_at(index, GraphSubscription::Port(subscription)))
    }

    pub fn subscribe_declared_queue<E>(
        &mut self,
        queue: &DeclaredQueue<E>,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<DeclaredSubscriptionGraphKey<E>, SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits(queue.declaration(), interest)?;
        Ok(DeclaredSubscriptionGraphKey::new(self.subscribe_queue(
            queue.raw(),
            interest,
            mailbox,
            generation,
        )?))
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn subscribe_declared_queue_with_waker<E>(
        &mut self,
        queue: &DeclaredQueue<E>,
        interest: E,
        waker: Waker,
    ) -> Result<DeclaredSubscriptionGraphKey<E>, SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits(queue.declaration(), interest)?;
        Ok(DeclaredSubscriptionGraphKey::new(self.subscribe_queue_with_waker(
            queue.raw(),
            interest,
            waker,
        )?))
    }

    pub fn subscribe_declared_port<E>(
        &mut self,
        port: &DeclaredPort<E>,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<DeclaredSubscriptionGraphKey<E>, SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits(port.declaration(), interest)?;
        Ok(DeclaredSubscriptionGraphKey::new(self.subscribe_port(
            port.raw(),
            interest,
            mailbox,
            generation,
        )?))
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn subscribe_declared_port_with_waker<E>(
        &mut self,
        port: &DeclaredPort<E>,
        interest: E,
        waker: Waker,
    ) -> Result<DeclaredSubscriptionGraphKey<E>, SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits(port.declaration(), interest)?;
        Ok(DeclaredSubscriptionGraphKey::new(self.subscribe_port_with_waker(
            port.raw(),
            interest,
            waker,
        )?))
    }

    pub fn remove(&mut self, key: SubscriptionGraphKey) -> Result<(), SubscriptionGraphError> {
        self.take_entry(key)?;
        Ok(())
    }

    pub fn kind(&self, key: SubscriptionGraphKey) -> Result<WireKind, SubscriptionGraphError> {
        Ok(self.entry(key)?.subscription.kind())
    }

    pub fn state(
        &self,
        key: SubscriptionGraphKey,
    ) -> Result<RawSubscriptionState, SubscriptionGraphError> {
        Ok(self.entry(key)?.subscription.state())
    }

    pub fn remove_declared<E>(
        &mut self,
        key: DeclaredSubscriptionGraphKey<E>,
    ) -> Result<(), SubscriptionGraphError> {
        self.remove(key.raw())
    }

    pub fn kind_declared<E>(
        &self,
        key: DeclaredSubscriptionGraphKey<E>,
    ) -> Result<WireKind, SubscriptionGraphError> {
        self.kind(key.raw())
    }

    pub fn state_declared<E>(
        &self,
        key: DeclaredSubscriptionGraphKey<E>,
    ) -> Result<RawSubscriptionState, SubscriptionGraphError> {
        self.state(key.raw())
    }

    pub fn update_queue(
        &mut self,
        key: SubscriptionGraphKey,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        match &mut self.entry_mut(key)?.subscription {
            GraphSubscription::Queue(subscription) => Ok(subscription.try_update(interest, mailbox, generation)?),
            GraphSubscription::Port(_) => Err(SubscriptionGraphError::KindMismatch),
        }
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn update_queue_with_waker(
        &mut self,
        key: SubscriptionGraphKey,
        interest: u64,
        waker: Waker,
    ) -> Result<(), SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        match &mut self.entry_mut(key)?.subscription {
            GraphSubscription::Queue(subscription) => Ok(subscription.try_update_with_waker(interest, waker)?),
            GraphSubscription::Port(_) => Err(SubscriptionGraphError::KindMismatch),
        }
    }

    pub fn update_port(
        &mut self,
        key: SubscriptionGraphKey,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        match &mut self.entry_mut(key)?.subscription {
            GraphSubscription::Port(subscription) => Ok(subscription.try_update(interest, mailbox, generation)?),
            GraphSubscription::Queue(_) => Err(SubscriptionGraphError::KindMismatch),
        }
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn update_port_with_waker(
        &mut self,
        key: SubscriptionGraphKey,
        interest: u64,
        waker: Waker,
    ) -> Result<(), SubscriptionGraphError> {
        if interest == 0 {
            return Err(SubscriptionGraphError::EmptyInterest);
        }

        match &mut self.entry_mut(key)?.subscription {
            GraphSubscription::Port(subscription) => Ok(subscription.try_update_with_waker(interest, waker)?),
            GraphSubscription::Queue(_) => Err(SubscriptionGraphError::KindMismatch),
        }
    }

    pub fn update_declared_queue<E>(
        &mut self,
        key: DeclaredSubscriptionGraphKey<E>,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits_for_event(interest)?;
        self.update_queue(key.raw(), interest, mailbox, generation)
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn update_declared_queue_with_waker<E>(
        &mut self,
        key: DeclaredSubscriptionGraphKey<E>,
        interest: E,
        waker: Waker,
    ) -> Result<(), SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits_for_event(interest)?;
        self.update_queue_with_waker(key.raw(), interest, waker)
    }

    pub fn update_declared_port<E>(
        &mut self,
        key: DeclaredSubscriptionGraphKey<E>,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits_for_event(interest)?;
        self.update_port(key.raw(), interest, mailbox, generation)
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn update_declared_port_with_waker<E>(
        &mut self,
        key: DeclaredSubscriptionGraphKey<E>,
        interest: E,
        waker: Waker,
    ) -> Result<(), SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = Self::validate_declared_bits_for_event(interest)?;
        self.update_port_with_waker(key.raw(), interest, waker)
    }

    pub fn take_ready(
        &mut self,
        key: SubscriptionGraphKey,
    ) -> Result<bool, SubscriptionGraphError> {
        Ok(self.entry_mut(key)?.subscription.try_take_ready()?)
    }

    pub fn take_declared_ready<E>(
        &mut self,
        key: DeclaredSubscriptionGraphKey<E>,
    ) -> Result<bool, SubscriptionGraphError> {
        self.take_ready(key.raw())
    }

    pub fn collect_ready(&mut self, ready: &mut [SubscriptionGraphReady]) -> usize {
        let mut count = 0;
        if ready.is_empty() {
            return count;
        }

        for (index, entry) in self.entries.iter_mut().enumerate() {
            if count == ready.len() {
                break;
            }

            let Some(entry) = entry.as_mut() else {
                continue;
            };

            let kind = entry.subscription.kind();
            let state = match entry.subscription.try_take_ready() {
                Ok(true) => RawSubscriptionState::Subscribed,
                Ok(false) | Err(RawSubscriptionError::Unsubscribed) => continue,
                Err(RawSubscriptionError::Terminal) => RawSubscriptionState::Terminal,
            };

            ready[count] = SubscriptionGraphReady {
                key: SubscriptionGraphKey {
                    index,
                    generation: entry.generation,
                },
                kind,
                state,
            };
            count += 1;
        }

        count
    }

    pub fn clear(&mut self) -> usize {
        let removed = self.len;
        for entry in &mut self.entries {
            *entry = None;
        }
        self.len = 0;
        removed
    }

    fn free_index(&self) -> Option<usize> {
        self.entries.iter().position(Option::is_none)
    }

    fn insert_at(&mut self, index: usize, subscription: GraphSubscription) -> SubscriptionGraphKey {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        self.entries[index] = Some(GraphEntry {
            generation,
            subscription,
        });
        self.len += 1;
        SubscriptionGraphKey { index, generation }
    }

    fn entry(&self, key: SubscriptionGraphKey) -> Result<&GraphEntry, SubscriptionGraphError> {
        let entry = self
            .entries
            .get(key.index)
            .and_then(Option::as_ref)
            .ok_or(SubscriptionGraphError::StaleKey)?;
        if entry.generation == key.generation {
            Ok(entry)
        } else {
            Err(SubscriptionGraphError::StaleKey)
        }
    }

    fn entry_mut(
        &mut self,
        key: SubscriptionGraphKey,
    ) -> Result<&mut GraphEntry, SubscriptionGraphError> {
        let entry = self
            .entries
            .get_mut(key.index)
            .and_then(Option::as_mut)
            .ok_or(SubscriptionGraphError::StaleKey)?;
        if entry.generation == key.generation {
            Ok(entry)
        } else {
            Err(SubscriptionGraphError::StaleKey)
        }
    }

    fn take_entry(
        &mut self,
        key: SubscriptionGraphKey,
    ) -> Result<GraphEntry, SubscriptionGraphError> {
        self.entry(key)?;
        self.len -= 1;
        Ok(self.entries[key.index]
            .take()
            .expect("entry was just validated"))
    }

    fn validate_declared_bits<E>(
        declaration: WireDeclaration<E>,
        interest: E,
    ) -> Result<u64, SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = interest.bits();
        declaration.validate_bits(interest)?;
        Ok(interest)
    }

    fn validate_declared_bits_for_event<E>(interest: E) -> Result<u64, SubscriptionGraphError>
    where
        E: WireEventSet,
    {
        let interest = interest.bits();
        if E::DECLARED_BITS == 0 {
            Err(WireDeclarationError::EmptyDeclaration.into())
        } else if interest & !E::DECLARED_BITS == 0 {
            Ok(interest)
        } else {
            Err(WireDeclarationError::UndeclaredBits.into())
        }
    }
}

impl<const N: usize> Default for SubscriptionGraph<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphSubscription {
    fn kind(&self) -> WireKind {
        match self {
            Self::Queue(_) => WireKind::Queue,
            Self::Port(_) => WireKind::Port,
        }
    }

    fn state(&self) -> RawSubscriptionState {
        match self {
            Self::Queue(subscription) => subscription.state(),
            Self::Port(subscription) => subscription.state(),
        }
    }

    fn try_take_ready(&mut self) -> Result<bool, RawSubscriptionError> {
        match self {
            Self::Queue(subscription) => subscription.try_take_ready(),
            Self::Port(subscription) => subscription.try_take_ready(),
        }
    }
}
