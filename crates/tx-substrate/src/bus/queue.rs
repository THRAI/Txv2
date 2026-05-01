use alloc::{sync::Arc, vec::Vec};
use core::task::Waker;

use super::common::{
    DeclaredSubscriptionError, DeclaredWireError, RawSubscriptionError, RawSubscriptionState,
    RawWireError, RawWireStorage, SpinLock, Subscriber, SubscriptionId, TerminateOutcome,
    WireDeclaration, WireDeclarationError, WireEventSet, WireKind, WireRetirement,
};

/// Level-triggered readiness wire.
#[derive(Clone)]
pub struct RawQueue {
    state: RawWireStorage<RawQueueState>,
}

struct RawQueueState {
    ready_bits: u64,
    terminal: bool,
    next_subscription: usize,
    subscribers: Vec<Subscriber>,
}

impl RawQueueState {
    const fn new() -> Self {
        Self {
            ready_bits: 0,
            terminal: false,
            next_subscription: 0,
            subscribers: Vec::new(),
        }
    }
}

/// Static backing storage for a [`RawQueue`] embedded in device tables.
///
/// The storage is the long-lived object; [`StaticRawQueue::raw`] creates cheap
/// handles that operate on this storage without allocating an `Arc`.
pub struct StaticRawQueue {
    state: SpinLock<RawQueueState>,
}

impl StaticRawQueue {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(RawQueueState::new()),
        }
    }

    pub const fn raw(&'static self) -> RawQueue {
        RawQueue {
            state: RawWireStorage::Static(&self.state),
        }
    }
}

impl Default for StaticRawQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Subscription token returned by [`RawQueue::subscribe`].
pub struct RawQueueSubscription {
    queue: RawQueue,
    id: Option<SubscriptionId>,
    terminal_snapshot: bool,
}

impl RawQueue {
    pub fn new() -> Self {
        Self {
            state: RawWireStorage::Shared(Arc::new(SpinLock::new(RawQueueState::new()))),
        }
    }

    pub const fn from_static(storage: &'static StaticRawQueue) -> Self {
        storage.raw()
    }

    pub fn subscribe(&self, interest: u64, waker: Waker) -> RawQueueSubscription {
        let mut state = self.state.lock();
        if state.terminal {
            drop(state);
            waker.wake_by_ref();
            return RawQueueSubscription {
                queue: self.clone(),
                id: None,
                terminal_snapshot: true,
            };
        }

        self.subscribe_with_state(&mut state, interest, waker)
    }

    pub fn try_subscribe(
        &self,
        interest: u64,
        waker: Waker,
    ) -> Result<RawQueueSubscription, RawWireError> {
        let mut state = self.state.lock();
        if state.terminal {
            return Err(RawWireError::Terminal);
        }

        Ok(self.subscribe_with_state(&mut state, interest, waker))
    }

    fn subscribe_with_state(
        &self,
        state: &mut RawQueueState,
        interest: u64,
        waker: Waker,
    ) -> RawQueueSubscription {
        let id = SubscriptionId(state.next_subscription);
        state.next_subscription = state.next_subscription.wrapping_add(1);
        state.subscribers.push(Subscriber {
            id,
            interest,
            ready: false,
            waker,
        });
        RawQueueSubscription {
            queue: self.clone(),
            id: Some(id),
            terminal_snapshot: false,
        }
    }

    pub fn fire(&self, bits: u64) -> usize {
        self.try_fire(bits).unwrap_or(0)
    }

    pub fn try_fire(&self, bits: u64) -> Result<usize, RawWireError> {
        if bits == 0 {
            return Ok(0);
        }

        let mut wakers = Vec::new();
        {
            let mut state = self.state.lock();
            if state.terminal {
                return Err(RawWireError::Terminal);
            }

            let new_bits = bits & !state.ready_bits;
            state.ready_bits |= bits;
            if new_bits == 0 {
                return Ok(0);
            }

            for subscriber in &mut state.subscribers {
                if !subscriber.ready && subscriber.interest & new_bits != 0 {
                    subscriber.ready = true;
                    wakers.push(subscriber.waker.clone());
                }
            }
        }

        let woke = wakers.len();
        for waker in wakers {
            waker.wake();
        }
        Ok(woke)
    }

    pub fn clear(&self, bits: u64) {
        let _ = self.try_clear(bits);
    }

    pub fn try_clear(&self, bits: u64) -> Result<(), RawWireError> {
        let mut state = self.state.lock();
        if state.terminal {
            return Err(RawWireError::Terminal);
        }

        state.ready_bits &= !bits;
        Ok(())
    }

    pub fn peek(&self) -> u64 {
        self.state.lock().ready_bits
    }

    pub fn terminate(&self, terminal_bits: u64) -> usize {
        self.terminate_with_status(terminal_bits).woken
    }

    pub fn retire(&self, terminal_bits: u64, guard: &crate::epoch::Guard<'_>) -> WireRetirement {
        WireRetirement::new(
            WireKind::Queue,
            terminal_bits,
            self.terminate_with_status(terminal_bits),
            guard,
        )
    }

    pub fn retire_silently(&self, guard: &crate::epoch::Guard<'_>) -> WireRetirement {
        self.retire(0, guard)
    }

    fn terminate_with_status(&self, terminal_bits: u64) -> TerminateOutcome {
        let mut wakers = Vec::new();
        let newly_terminal;
        {
            let mut state = self.state.lock();
            if state.terminal {
                return TerminateOutcome {
                    woken: 0,
                    newly_terminal: false,
                };
            }

            state.terminal = true;
            newly_terminal = true;
            state.ready_bits |= terminal_bits;
            if terminal_bits == 0 {
                state.subscribers.clear();
            } else {
                wakers.extend(
                    state
                        .subscribers
                        .drain(..)
                        .map(|subscriber| subscriber.waker),
                );
            }
        }

        let woke = wakers.len();
        for waker in wakers {
            waker.wake();
        }
        TerminateOutcome {
            woken: woke,
            newly_terminal,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.state.lock().terminal
    }

    pub fn subscriber_count(&self) -> usize {
        self.state.lock().subscribers.len()
    }

    fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut state = self.state.lock();
        if let Some(index) = state
            .subscribers
            .iter()
            .position(|subscriber| subscriber.id == id)
        {
            state.subscribers.swap_remove(index);
            true
        } else {
            false
        }
    }

    fn update_subscription(
        &self,
        id: SubscriptionId,
        interest: u64,
        waker: Waker,
    ) -> Result<(), RawSubscriptionError> {
        let mut state = self.state.lock();
        if state.terminal {
            return Err(RawSubscriptionError::Terminal);
        }

        if let Some(subscriber) = state
            .subscribers
            .iter_mut()
            .find(|subscriber| subscriber.id == id)
        {
            subscriber.interest = interest;
            subscriber.waker = waker;
            Ok(())
        } else {
            Err(RawSubscriptionError::Unsubscribed)
        }
    }

    fn take_ready(&self, id: SubscriptionId) -> Result<bool, RawSubscriptionError> {
        let mut state = self.state.lock();
        if state.terminal {
            return Err(RawSubscriptionError::Terminal);
        }

        if let Some(subscriber) = state
            .subscribers
            .iter_mut()
            .find(|subscriber| subscriber.id == id)
        {
            let ready = subscriber.ready;
            subscriber.ready = false;
            Ok(ready)
        } else {
            Err(RawSubscriptionError::Unsubscribed)
        }
    }

    fn subscription_state(&self, id: SubscriptionId) -> RawSubscriptionState {
        let state = self.state.lock();
        if state.terminal {
            RawSubscriptionState::Terminal
        } else if state
            .subscribers
            .iter()
            .any(|subscriber| subscriber.id == id)
        {
            RawSubscriptionState::Subscribed
        } else {
            RawSubscriptionState::Unsubscribed
        }
    }
}

impl Default for RawQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed RawQueue instance bound to a static [`WireDeclaration`].
pub struct DeclaredQueue<E> {
    raw: RawQueue,
    declaration: WireDeclaration<E>,
}

/// Subscription token returned by [`DeclaredQueue::try_subscribe`].
pub struct DeclaredQueueSubscription<E> {
    raw: RawQueueSubscription,
    declaration: WireDeclaration<E>,
}

impl<E: WireEventSet> DeclaredQueue<E> {
    pub fn new(declaration: WireDeclaration<E>) -> Result<Self, WireDeclarationError> {
        declaration.validate()?;
        Ok(Self {
            raw: RawQueue::new(),
            declaration,
        })
    }

    pub fn from_raw(
        raw: RawQueue,
        declaration: WireDeclaration<E>,
    ) -> Result<Self, WireDeclarationError> {
        declaration.validate()?;
        Ok(Self { raw, declaration })
    }

    pub fn from_static(
        storage: &'static StaticRawQueue,
        declaration: WireDeclaration<E>,
    ) -> Result<Self, WireDeclarationError> {
        Self::from_raw(RawQueue::from_static(storage), declaration)
    }

    pub fn declaration(&self) -> WireDeclaration<E> {
        self.declaration
    }

    pub fn raw(&self) -> &RawQueue {
        &self.raw
    }

    pub fn try_subscribe(
        &self,
        interest: E,
        waker: Waker,
    ) -> Result<DeclaredQueueSubscription<E>, DeclaredWireError> {
        let bits = self.validated_bits(interest)?;
        Ok(DeclaredQueueSubscription {
            raw: self.raw.try_subscribe(bits, waker)?,
            declaration: self.declaration,
        })
    }

    pub fn subscribe(&self, interest: E, waker: Waker) -> DeclaredQueueSubscription<E> {
        let bits = self
            .validated_bits(interest)
            .expect("typed bus queue subscription must use declared bits");
        DeclaredQueueSubscription {
            raw: self.raw.subscribe(bits, waker),
            declaration: self.declaration,
        }
    }

    pub fn try_fire(&self, bits: E) -> Result<usize, DeclaredWireError> {
        let bits = self.validated_bits(bits)?;
        Ok(self.raw.try_fire(bits)?)
    }

    pub fn fire(&self, bits: E) -> usize {
        self.try_fire(bits).unwrap_or(0)
    }

    pub fn try_clear(&self, bits: E) -> Result<(), DeclaredWireError> {
        let bits = self.validated_bits(bits)?;
        Ok(self.raw.try_clear(bits)?)
    }

    pub fn clear(&self, bits: E) {
        let _ = self.try_clear(bits);
    }

    pub fn peek_bits(&self) -> u64 {
        self.raw.peek() & self.declaration.declared_bits()
    }

    pub fn try_terminate(&self, terminal_bits: E) -> Result<usize, DeclaredWireError> {
        let bits = self.validated_bits(terminal_bits)?;
        Ok(self.raw.terminate(bits))
    }

    pub fn terminate(&self, terminal_bits: E) -> usize {
        self.try_terminate(terminal_bits).unwrap_or(0)
    }

    pub fn try_retire(
        &self,
        terminal_bits: E,
        guard: &crate::epoch::Guard<'_>,
    ) -> Result<WireRetirement, DeclaredWireError> {
        let bits = self.validated_bits(terminal_bits)?;
        Ok(self.raw.retire(bits, guard))
    }

    pub fn retire(&self, terminal_bits: E, guard: &crate::epoch::Guard<'_>) -> WireRetirement {
        self.try_retire(terminal_bits, guard)
            .expect("typed bus queue retirement must use declared bits")
    }

    pub fn retire_silently(&self, guard: &crate::epoch::Guard<'_>) -> WireRetirement {
        self.raw.retire_silently(guard)
    }

    pub fn terminate_silently(&self) -> usize {
        self.raw.terminate(0)
    }

    pub fn is_terminal(&self) -> bool {
        self.raw.is_terminal()
    }

    pub fn subscriber_count(&self) -> usize {
        self.raw.subscriber_count()
    }

    fn validated_bits(&self, bits: E) -> Result<u64, WireDeclarationError> {
        let bits = bits.bits();
        self.declaration.validate_bits(bits)?;
        Ok(bits)
    }
}

impl<E> Clone for DeclaredQueue<E> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            declaration: self.declaration,
        }
    }
}

impl<E: WireEventSet> DeclaredQueueSubscription<E> {
    pub fn state(&self) -> RawSubscriptionState {
        self.raw.state()
    }

    pub fn is_subscribed(&self) -> bool {
        self.raw.is_subscribed()
    }

    pub fn unsubscribe(&mut self) -> bool {
        self.raw.unsubscribe()
    }

    pub fn update(&mut self, interest: E, waker: Waker) {
        let _ = self.try_update(interest, waker);
    }

    pub fn try_update(
        &mut self,
        interest: E,
        waker: Waker,
    ) -> Result<(), DeclaredSubscriptionError> {
        let bits = interest.bits();
        self.declaration.validate_bits(bits)?;
        Ok(self.raw.try_update(bits, waker)?)
    }

    pub fn take_ready(&mut self) -> bool {
        self.raw.take_ready()
    }

    pub fn try_take_ready(&mut self) -> Result<bool, DeclaredSubscriptionError> {
        Ok(self.raw.try_take_ready()?)
    }
}

impl RawQueueSubscription {
    pub fn state(&self) -> RawSubscriptionState {
        match self.id {
            Some(id) => self.queue.subscription_state(id),
            None if self.terminal_snapshot => RawSubscriptionState::Terminal,
            None => RawSubscriptionState::Unsubscribed,
        }
    }

    pub fn is_subscribed(&self) -> bool {
        self.state() == RawSubscriptionState::Subscribed
    }

    pub fn unsubscribe(&mut self) -> bool {
        if let Some(id) = self.id.take() {
            self.terminal_snapshot = false;
            self.queue.unsubscribe(id)
        } else {
            false
        }
    }

    pub fn update(&mut self, interest: u64, waker: Waker) {
        let _ = self.try_update(interest, waker);
    }

    pub fn try_update(&mut self, interest: u64, waker: Waker) -> Result<(), RawSubscriptionError> {
        match self.id {
            Some(id) => self.queue.update_subscription(id, interest, waker),
            None if self.terminal_snapshot => Err(RawSubscriptionError::Terminal),
            None => Err(RawSubscriptionError::Unsubscribed),
        }
    }

    pub fn take_ready(&mut self) -> bool {
        match self.try_take_ready() {
            Ok(ready) => ready,
            Err(RawSubscriptionError::Terminal) => true,
            Err(RawSubscriptionError::Unsubscribed) => false,
        }
    }

    pub fn try_take_ready(&mut self) -> Result<bool, RawSubscriptionError> {
        match self.id {
            Some(id) => self.queue.take_ready(id),
            None if self.terminal_snapshot => Err(RawSubscriptionError::Terminal),
            None => Err(RawSubscriptionError::Unsubscribed),
        }
    }
}

impl Drop for RawQueueSubscription {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.queue.unsubscribe(id);
        }
    }
}
