use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use super::common::{
    post_source_fired, DeclaredSubscriptionError, DeclaredWireError, RawSubscriptionError,
    RawSubscriptionState, RawWireError, RawWireStorage, SpinLock, Subscriber, SubscriptionId,
    TerminateOutcome, WireDeclaration, WireDeclarationError, WireEventSet, WireKind,
    WireRetirement,
};
use crate::step::{InterestMask, WaitSourceId};
use crate::wake::mailbox::{MailboxEvent, TaskMailbox, WaitGeneration};
use core::task::Waker;

/// Level-triggered readiness wire.
///
/// Each queue carries a [`WaitSourceId`] so `fire()` can post
/// [`MailboxEvent::SourceFired`] events to subscriber mailboxes.
/// The id is assigned via [`Self::set_source_id`] after the queue
/// is registered with the wait-source registry.
#[derive(Clone)]
pub struct RawQueue {
    state: RawWireStorage<RawQueueState>,
}

struct RawQueueState {
    source_id: WaitSourceId,
    ready_bits: u64,
    terminal: bool,
    next_subscription: usize,
    subscribers: Vec<Subscriber>,
}

impl RawQueueState {
    const fn new() -> Self {
        Self {
            source_id: WaitSourceId::ZERO,
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
    /// Retain the mailbox built by `subscribe_with_waker` for legacy
    /// tests so its `Weak` reference stays upgradeable for the lifetime
    /// of the subscription. `None` for callers that pass an external
    /// `Weak<TaskMailbox>` via `subscribe(...)`.
    _waker_mailbox: Option<Arc<TaskMailbox>>,
    /// One-shot terminal edge for the deprecated `take_ready` API.
    terminal_observed: bool,
}

impl RawQueue {
    pub fn new() -> Self {
        Self {
            state: RawWireStorage::Shared(Arc::new(SpinLock::new(RawQueueState::new()))),
        }
    }

    pub fn with_source_id(source_id: WaitSourceId) -> Self {
        let queue = Self::new();
        queue.set_source_id(source_id);
        queue
    }

    pub fn source_id(&self) -> WaitSourceId {
        self.state.lock().source_id
    }

    pub fn set_source_id(&self, id: WaitSourceId) {
        self.state.lock().source_id = id;
    }

    /// Compatibility bridge: create a one-shot `TaskMailbox`, register
    /// `waker`, and subscribe. The mailbox is leaked into an `Arc` so
    /// the bus's `fire()` path can post to it; the waker is registered
    /// so the test harness observes wake counts.
    ///
    /// This method exists so legacy tests can compile against the
    /// migrated bus API. New code should call [`Self::subscribe`]
    /// directly with a long-lived `Weak<TaskMailbox>`.
    pub fn subscribe_with_waker(&self, interest: u64, waker: Waker) -> RawQueueSubscription {
        let mailbox = Arc::new(TaskMailbox::new());
        mailbox.register_waker(waker);
        let gen = mailbox.next_generation();
        let mut sub = self.subscribe(interest, Arc::downgrade(&mailbox), gen);
        sub._waker_mailbox = Some(mailbox);
        sub
    }

    /// Compatibility bridge: see [`Self::subscribe_with_waker`].
    pub fn try_subscribe_with_waker(
        &self,
        interest: u64,
        waker: Waker,
    ) -> Result<RawQueueSubscription, RawWireError> {
        let mailbox = Arc::new(TaskMailbox::new());
        mailbox.register_waker(waker);
        let gen = mailbox.next_generation();
        let mut sub = self.try_subscribe(interest, Arc::downgrade(&mailbox), gen)?;
        sub._waker_mailbox = Some(mailbox);
        Ok(sub)
    }

    pub const fn from_static(storage: &'static StaticRawQueue) -> Self {
        storage.raw()
    }

    pub fn subscribe(
        &self,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> RawQueueSubscription {
        let mut state = self.state.lock();
        if state.terminal {
            drop(state);
            // Post a terminal SourceFired so the caller observes
            // the terminal state immediately.
            if let Some(mb) = mailbox.upgrade() {
                mb.post(MailboxEvent::SourceFired {
                    generation,
                    source: self.source_id(),
                    interests: InterestMask::new(interest),
                });
            }
            return RawQueueSubscription {
                queue: self.clone(),
                id: None,
                terminal_snapshot: true,
                _waker_mailbox: None,
                terminal_observed: false,
            };
        }

        self.subscribe_with_state(&mut state, interest, mailbox, generation)
    }

    pub fn try_subscribe(
        &self,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<RawQueueSubscription, RawWireError> {
        let mut state = self.state.lock();
        if state.terminal {
            return Err(RawWireError::Terminal);
        }

        Ok(self.subscribe_with_state(&mut state, interest, mailbox, generation))
    }

    fn subscribe_with_state(
        &self,
        state: &mut RawQueueState,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> RawQueueSubscription {
        let id = SubscriptionId(state.next_subscription);
        state.next_subscription = state.next_subscription.wrapping_add(1);
        state.subscribers.push(Subscriber {
            id,
            interest,
            mailbox,
            generation,
        });
        RawQueueSubscription {
            queue: self.clone(),
            id: Some(id),
            terminal_snapshot: false,
            _waker_mailbox: None,
            terminal_observed: false,
        }
    }

    pub fn fire(&self, bits: u64) -> usize {
        self.try_fire(bits).unwrap_or(0)
    }

    pub fn try_fire(&self, bits: u64) -> Result<usize, RawWireError> {
        self.try_fire_with_post(bits, |mailbox, event| mailbox.post(event))
    }

    pub fn fire_with_post<F>(&self, bits: u64, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.try_fire_with_post(bits, post).unwrap_or(0)
    }

    pub fn try_fire_with_post<F>(&self, bits: u64, mut post: F) -> Result<usize, RawWireError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        if bits == 0 {
            return Ok(0);
        }

        let source = self.source_id();
        let (interests, deliveries) = {
            let mut state = self.state.lock();
            if state.terminal {
                return Err(RawWireError::Terminal);
            }

            let new_bits = bits & !state.ready_bits;
            state.ready_bits |= bits;
            if new_bits == 0 {
                return Ok(0);
            }

            let interests = InterestMask::new(new_bits);
            let mut deliveries = Vec::new();
            for subscriber in &mut state.subscribers {
                if subscriber.interest & new_bits != 0 {
                    if let Some(mailbox) = subscriber.mailbox.upgrade() {
                        deliveries.push((mailbox, subscriber.generation));
                    }
                }
            }
            (interests, deliveries)
        };

        let mut woke = 0usize;
        for (mailbox, generation) in deliveries {
            if post(
                &mailbox,
                MailboxEvent::SourceFired {
                    generation,
                    source,
                    interests,
                },
            ) {
                woke += 1;
            }
        }
        Ok(woke)
    }

    pub fn clear(&self, bits: u64) {
        let _ = self.try_clear(bits);
    }

    /// Atomically snapshot and consume the requested readiness bits.
    ///
    /// `peek()` followed by `clear()` is not equivalent: a producer can fire
    /// the same bit between those two operations and the later clear then
    /// erases the new event. Event-loop owners use this method when a set bit
    /// means "work was requested" rather than persistent level readiness.
    pub fn take(&self, bits: u64) -> u64 {
        self.try_take(bits).unwrap_or(0)
    }

    pub fn try_take(&self, bits: u64) -> Result<u64, RawWireError> {
        let mut state = self.state.lock();
        if state.terminal {
            return Err(RawWireError::Terminal);
        }

        let taken = state.ready_bits & bits;
        state.ready_bits &= !bits;
        Ok(taken)
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
        let source = self.source_id();
        let mut woke = 0usize;
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
                let interests = InterestMask::new(terminal_bits);
                for subscriber in state.subscribers.drain(..) {
                    if post_source_fired(&subscriber, source, interests) {
                        woke += 1;
                    }
                }
            }
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
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
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
            subscriber.mailbox = mailbox;
            subscriber.generation = generation;
            Ok(())
        } else {
            Err(RawSubscriptionError::Unsubscribed)
        }
    }

    #[allow(dead_code)] // txdoc:vfs-full-bringup-scaffold
    fn take_ready(&self, id: SubscriptionId) -> Result<bool, RawSubscriptionError> {
        let state = self.state.lock();
        if state.terminal {
            return Err(RawSubscriptionError::Terminal);
        }

        // Readiness is now delivered via TaskMailbox events;
        // per-subscriber ready flags are retired. Always return
        // false so callers migrate to mailbox polling.
        if state.subscribers.iter().any(|s| s.id == id) {
            Ok(false)
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
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<DeclaredQueueSubscription<E>, DeclaredWireError> {
        let bits = self.validated_bits(interest)?;
        Ok(DeclaredQueueSubscription {
            raw: self.raw.try_subscribe(bits, mailbox, generation)?,
            declaration: self.declaration,
        })
    }

    pub fn subscribe(
        &self,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> DeclaredQueueSubscription<E> {
        let bits = self
            .validated_bits(interest)
            .expect("typed bus queue subscription must use declared bits");
        DeclaredQueueSubscription {
            raw: self.raw.subscribe(bits, mailbox, generation),
            declaration: self.declaration,
        }
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn subscribe_with_waker(&self, interest: E, waker: Waker) -> DeclaredQueueSubscription<E> {
        let bits = self
            .validated_bits(interest)
            .expect("typed bus queue subscription must use declared bits");
        DeclaredQueueSubscription {
            raw: self.raw.subscribe_with_waker(bits, waker),
            declaration: self.declaration,
        }
    }

    /// Compatibility bridge: see [`RawQueue::try_subscribe_with_waker`].
    pub fn try_subscribe_with_waker(
        &self,
        interest: E,
        waker: Waker,
    ) -> Result<DeclaredQueueSubscription<E>, DeclaredWireError> {
        let bits = self.validated_bits(interest)?;
        Ok(DeclaredQueueSubscription {
            raw: self.raw.try_subscribe_with_waker(bits, waker)?,
            declaration: self.declaration,
        })
    }

    pub fn try_fire(&self, bits: E) -> Result<usize, DeclaredWireError> {
        let bits = self.validated_bits(bits)?;
        Ok(self.raw.try_fire(bits)?)
    }

    pub fn try_fire_with_post<F>(&self, bits: E, post: F) -> Result<usize, DeclaredWireError>
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        let bits = self.validated_bits(bits)?;
        Ok(self.raw.try_fire_with_post(bits, post)?)
    }

    pub fn fire_with_post<F>(&self, bits: E, post: F) -> usize
    where
        F: FnMut(&TaskMailbox, MailboxEvent) -> bool,
    {
        self.try_fire_with_post(bits, post).unwrap_or(0)
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

    pub fn update(&mut self, interest: E, mailbox: Weak<TaskMailbox>, generation: WaitGeneration) {
        let _ = self.try_update(interest, mailbox, generation);
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn update_with_waker(&mut self, interest: E, waker: Waker) {
        let bits = interest.bits();
        let _ = self.declaration.validate_bits(bits);
        self.raw.update_with_waker(bits, waker);
    }

    pub fn try_update(
        &mut self,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), DeclaredSubscriptionError> {
        let bits = interest.bits();
        self.declaration.validate_bits(bits)?;
        Ok(self.raw.try_update(bits, mailbox, generation)?)
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn try_update_with_waker(
        &mut self,
        interest: E,
        waker: Waker,
    ) -> Result<(), DeclaredSubscriptionError> {
        let bits = interest.bits();
        self.declaration.validate_bits(bits)?;
        let mailbox = Arc::new(TaskMailbox::new());
        mailbox.register_waker(waker);
        let gen = mailbox.next_generation();
        Ok(self.raw.try_update(bits, Arc::downgrade(&mailbox), gen)?)
    }

    #[deprecated(note = "use TaskMailbox::poll() + ActiveWait::matches() instead")]
    pub fn take_ready(&mut self) -> bool {
        #[allow(deprecated)]
        self.raw.take_ready()
    }

    #[deprecated(note = "use TaskMailbox::poll() + ActiveWait::matches() instead")]
    pub fn try_take_ready(&mut self) -> Result<bool, DeclaredSubscriptionError> {
        #[allow(deprecated)]
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

    pub fn update(
        &mut self,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) {
        let _ = self.try_update(interest, mailbox, generation);
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn update_with_waker(&mut self, interest: u64, waker: Waker) {
        let _ = self.try_update_with_waker(interest, waker);
    }

    /// Compatibility bridge: see [`RawQueue::try_subscribe_with_waker`].
    pub fn try_update_with_waker(
        &mut self,
        interest: u64,
        waker: Waker,
    ) -> Result<(), RawSubscriptionError> {
        let mailbox = Arc::new(TaskMailbox::new());
        mailbox.register_waker(waker);
        let gen = mailbox.next_generation();
        self.try_update(interest, Arc::downgrade(&mailbox), gen)?;
        self._waker_mailbox = Some(mailbox);
        self.terminal_observed = false;
        Ok(())
    }

    pub fn try_update(
        &mut self,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<(), RawSubscriptionError> {
        match self.id {
            Some(id) => self
                .queue
                .update_subscription(id, interest, mailbox, generation),
            None if self.terminal_snapshot => Err(RawSubscriptionError::Terminal),
            None => Err(RawSubscriptionError::Unsubscribed),
        }
    }

    /// Deprecated: readiness is now delivered via [`TaskMailbox`]
    /// events. The mailbox driver filters events with
    /// [`crate::wake::mailbox::ActiveWait::matches`]. For legacy
    /// callers that constructed the subscription via
    /// `subscribe_with_waker`, this method drains the embedded
    /// mailbox and returns `true` if a `SourceFired` event was
    /// observed. Callers should migrate to polling their mailbox
    /// directly.
    #[deprecated(note = "use TaskMailbox::poll() + ActiveWait::matches() instead")]
    pub fn take_ready(&mut self) -> bool {
        if let Some(mb) = self._waker_mailbox.as_ref() {
            while let Some(event) = mb.poll() {
                if matches!(
                    event,
                    crate::wake::mailbox::MailboxEvent::SourceFired { .. }
                ) {
                    return true;
                }
            }
        }
        if !self.terminal_observed && self.state() == RawSubscriptionState::Terminal {
            self.terminal_observed = true;
            return true;
        }
        false
    }

    /// Deprecated: see [`Self::take_ready`].
    #[deprecated(note = "use TaskMailbox::poll() + ActiveWait::matches() instead")]
    pub fn try_take_ready(&mut self) -> Result<bool, RawSubscriptionError> {
        match self.state() {
            RawSubscriptionState::Subscribed =>
            {
                #[allow(deprecated)]
                Ok(self.take_ready())
            }
            RawSubscriptionState::Terminal => Err(RawSubscriptionError::Terminal),
            RawSubscriptionState::Unsubscribed => Err(RawSubscriptionError::Unsubscribed),
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
