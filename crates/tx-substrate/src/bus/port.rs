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

/// Edge-triggered event wire.
#[derive(Clone)]
pub struct RawPort {
    state: RawWireStorage<RawPortState>,
    source_id: WaitSourceId,
}

struct RawPortState {
    terminal: bool,
    next_subscription: usize,
    subscribers: Vec<Subscriber>,
}

impl RawPortState {
    const fn new() -> Self {
        Self {
            terminal: false,
            next_subscription: 0,
            subscribers: Vec::new(),
        }
    }
}

/// Static backing storage for a [`RawPort`] embedded in device tables.
///
/// The storage is the long-lived object; [`StaticRawPort::raw`] creates cheap
/// handles that operate on this storage without allocating an `Arc`.
pub struct StaticRawPort {
    state: SpinLock<RawPortState>,
    source_id: WaitSourceId,
}

impl StaticRawPort {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(RawPortState::new()),
            source_id: WaitSourceId::new(0),
        }
    }

    pub const fn raw(&'static self) -> RawPort {
        RawPort {
            state: RawWireStorage::Static(&self.state),
            source_id: self.source_id,
        }
    }
}

impl Default for StaticRawPort {
    fn default() -> Self {
        Self::new()
    }
}

/// Subscription token returned by [`RawPort::subscribe`].
pub struct RawPortSubscription {
    port: RawPort,
    id: Option<SubscriptionId>,
    terminal_snapshot: bool,
    /// Retain the mailbox built by `subscribe_with_waker` for legacy
    /// tests so its `Weak` reference stays upgradeable for the lifetime
    /// of the subscription.
    _waker_mailbox: Option<Arc<TaskMailbox>>,
    /// Has the deprecated `take_ready` already consumed the
    /// terminal-state edge? Tracks the one-shot "subscription became
    /// terminal" signal so legacy callers see a single `true` on the
    /// first `take_ready` after the wire retires.
    terminal_observed: bool,
}

impl RawPort {
    pub fn new() -> Self {
        Self {
            state: RawWireStorage::Shared(Arc::new(SpinLock::new(RawPortState::new()))),
            source_id: WaitSourceId::new(0),
        }
    }

    pub fn with_source_id(source_id: WaitSourceId) -> Self {
        Self {
            state: RawWireStorage::Shared(Arc::new(SpinLock::new(RawPortState::new()))),
            source_id,
        }
    }

    pub fn source_id(&self) -> WaitSourceId {
        self.source_id
    }

    pub fn set_source_id(&mut self, id: WaitSourceId) {
        self.source_id = id;
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn subscribe_with_waker(&self, interest: u64, waker: Waker) -> RawPortSubscription {
        let mailbox = Arc::new(TaskMailbox::new());
        mailbox.register_waker(waker);
        let gen = mailbox.next_generation();
        let mut sub = self.subscribe(interest, Arc::downgrade(&mailbox), gen);
        sub._waker_mailbox = Some(mailbox);
        sub
    }

    /// Compatibility bridge: see [`RawQueue::try_subscribe_with_waker`].
    pub fn try_subscribe_with_waker(
        &self,
        interest: u64,
        waker: Waker,
    ) -> Result<RawPortSubscription, RawWireError> {
        let mailbox = Arc::new(TaskMailbox::new());
        mailbox.register_waker(waker);
        let gen = mailbox.next_generation();
        let mut sub = self.try_subscribe(interest, Arc::downgrade(&mailbox), gen)?;
        sub._waker_mailbox = Some(mailbox);
        Ok(sub)
    }

    pub const fn from_static(storage: &'static StaticRawPort) -> Self {
        storage.raw()
    }

    pub fn subscribe(
        &self,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> RawPortSubscription {
        let mut state = self.state.lock();
        if state.terminal {
            drop(state);
            if let Some(mb) = mailbox.upgrade() {
                mb.post(MailboxEvent::SourceFired {
                    generation,
                    source: self.source_id,
                    interests: InterestMask::new(interest),
                });
            }
            return RawPortSubscription {
                port: self.clone(),
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
    ) -> Result<RawPortSubscription, RawWireError> {
        let mut state = self.state.lock();
        if state.terminal {
            return Err(RawWireError::Terminal);
        }

        Ok(self.subscribe_with_state(&mut state, interest, mailbox, generation))
    }

    fn subscribe_with_state(
        &self,
        state: &mut RawPortState,
        interest: u64,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> RawPortSubscription {
        let id = SubscriptionId(state.next_subscription);
        state.next_subscription = state.next_subscription.wrapping_add(1);
        state.subscribers.push(Subscriber {
            id,
            interest,
            mailbox,
            generation,
        });
        RawPortSubscription {
            port: self.clone(),
            id: Some(id),
            terminal_snapshot: false,
            _waker_mailbox: None,
            terminal_observed: false,
        }
    }

    pub fn fire(&self, event: u64) -> usize {
        self.try_fire(event).unwrap_or(0)
    }

    pub fn try_fire(&self, event: u64) -> Result<usize, RawWireError> {
        if event == 0 {
            return Ok(0);
        }

        let source = self.source_id;
        let mut woke = 0usize;
        {
            let mut state = self.state.lock();
            if state.terminal {
                return Err(RawWireError::Terminal);
            }

            let interests = InterestMask::new(event);
            for subscriber in &mut state.subscribers {
                if subscriber.interest & event != 0
                    && post_source_fired(subscriber, source, interests)
                {
                    woke += 1;
                }
            }
        }
        Ok(woke)
    }

    pub fn terminate(&self, gone_event: u64) -> usize {
        self.terminate_with_status(gone_event).woken
    }

    pub fn retire(&self, gone_event: u64, guard: &crate::epoch::Guard<'_>) -> WireRetirement {
        WireRetirement::new(
            WireKind::Port,
            gone_event,
            self.terminate_with_status(gone_event),
            guard,
        )
    }

    pub fn retire_silently(&self, guard: &crate::epoch::Guard<'_>) -> WireRetirement {
        self.retire(0, guard)
    }

    fn terminate_with_status(&self, gone_event: u64) -> TerminateOutcome {
        let source = self.source_id;
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
            if gone_event == 0 {
                state.subscribers.clear();
            } else {
                let interests = InterestMask::new(gone_event);
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
        // per-subscriber ready flags are retired.
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

impl Default for RawPort {
    fn default() -> Self {
        Self::new()
    }
}

/// Typed RawPort instance bound to a static [`WireDeclaration`].
pub struct DeclaredPort<E> {
    raw: RawPort,
    declaration: WireDeclaration<E>,
}

/// Subscription token returned by [`DeclaredPort::try_subscribe`].
pub struct DeclaredPortSubscription<E> {
    raw: RawPortSubscription,
    declaration: WireDeclaration<E>,
}

impl<E: WireEventSet> DeclaredPort<E> {
    pub fn new(declaration: WireDeclaration<E>) -> Result<Self, WireDeclarationError> {
        declaration.validate()?;
        Ok(Self {
            raw: RawPort::new(),
            declaration,
        })
    }

    pub fn from_raw(
        raw: RawPort,
        declaration: WireDeclaration<E>,
    ) -> Result<Self, WireDeclarationError> {
        declaration.validate()?;
        Ok(Self { raw, declaration })
    }

    pub fn from_static(
        storage: &'static StaticRawPort,
        declaration: WireDeclaration<E>,
    ) -> Result<Self, WireDeclarationError> {
        Self::from_raw(RawPort::from_static(storage), declaration)
    }

    pub fn declaration(&self) -> WireDeclaration<E> {
        self.declaration
    }

    pub fn raw(&self) -> &RawPort {
        &self.raw
    }

    pub fn try_subscribe(
        &self,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> Result<DeclaredPortSubscription<E>, DeclaredWireError> {
        let bits = self.validated_bits(interest)?;
        Ok(DeclaredPortSubscription {
            raw: self.raw.try_subscribe(bits, mailbox, generation)?,
            declaration: self.declaration,
        })
    }

    pub fn subscribe(
        &self,
        interest: E,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
    ) -> DeclaredPortSubscription<E> {
        let bits = self
            .validated_bits(interest)
            .expect("typed bus port subscription must use declared bits");
        DeclaredPortSubscription {
            raw: self.raw.subscribe(bits, mailbox, generation),
            declaration: self.declaration,
        }
    }

    /// Compatibility bridge: see [`RawQueue::subscribe_with_waker`].
    pub fn subscribe_with_waker(&self, interest: E, waker: Waker) -> DeclaredPortSubscription<E> {
        let bits = self
            .validated_bits(interest)
            .expect("typed bus port subscription must use declared bits");
        DeclaredPortSubscription {
            raw: self.raw.subscribe_with_waker(bits, waker),
            declaration: self.declaration,
        }
    }

    /// Compatibility bridge: see [`RawQueue::try_subscribe_with_waker`].
    pub fn try_subscribe_with_waker(
        &self,
        interest: E,
        waker: Waker,
    ) -> Result<DeclaredPortSubscription<E>, DeclaredWireError> {
        let bits = self.validated_bits(interest)?;
        Ok(DeclaredPortSubscription {
            raw: self.raw.try_subscribe_with_waker(bits, waker)?,
            declaration: self.declaration,
        })
    }

    pub fn try_fire(&self, event: E) -> Result<usize, DeclaredWireError> {
        let event = self.validated_bits(event)?;
        Ok(self.raw.try_fire(event)?)
    }

    pub fn fire(&self, event: E) -> usize {
        self.try_fire(event).unwrap_or(0)
    }

    pub fn try_terminate(&self, gone_event: E) -> Result<usize, DeclaredWireError> {
        let event = self.validated_bits(gone_event)?;
        Ok(self.raw.terminate(event))
    }

    pub fn terminate(&self, gone_event: E) -> usize {
        self.try_terminate(gone_event).unwrap_or(0)
    }

    pub fn try_retire(
        &self,
        gone_event: E,
        guard: &crate::epoch::Guard<'_>,
    ) -> Result<WireRetirement, DeclaredWireError> {
        let event = self.validated_bits(gone_event)?;
        Ok(self.raw.retire(event, guard))
    }

    pub fn retire(&self, gone_event: E, guard: &crate::epoch::Guard<'_>) -> WireRetirement {
        self.try_retire(gone_event, guard)
            .expect("typed bus port retirement must use declared bits")
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

    fn validated_bits(&self, event: E) -> Result<u64, WireDeclarationError> {
        let bits = event.bits();
        self.declaration.validate_bits(bits)?;
        Ok(bits)
    }
}

impl<E> Clone for DeclaredPort<E> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw.clone(),
            declaration: self.declaration,
        }
    }
}

impl<E: WireEventSet> DeclaredPortSubscription<E> {
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

    #[deprecated(note = "see take_ready")]
    pub fn try_take_ready(&mut self) -> Result<bool, DeclaredSubscriptionError> {
        #[allow(deprecated)]
        Ok(self.raw.try_take_ready()?)
    }
}

impl RawPortSubscription {
    pub fn state(&self) -> RawSubscriptionState {
        match self.id {
            Some(id) => self.port.subscription_state(id),
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
            self.port.unsubscribe(id)
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
                .port
                .update_subscription(id, interest, mailbox, generation),
            None if self.terminal_snapshot => Err(RawSubscriptionError::Terminal),
            None => Err(RawSubscriptionError::Unsubscribed),
        }
    }

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
        // Legacy one-shot terminal edge: if the wire has retired
        // since we last observed, surface a `true` once.
        if !self.terminal_observed && self.state() == RawSubscriptionState::Terminal {
            self.terminal_observed = true;
            return true;
        }
        false
    }

    #[deprecated(note = "see take_ready")]
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

impl Drop for RawPortSubscription {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.port.unsubscribe(id);
        }
    }
}
