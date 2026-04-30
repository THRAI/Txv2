//! Minimal bus publication primitives.
//!
//! This first slice intentionally keeps the bus semantic-free: wires store
//! subscriber interests and invoke reactor-owned wakers, but they do not
//! evaluate readiness predicates or schedule tasks.

extern crate alloc;

use alloc::{rc::Rc, vec::Vec};
use core::{cell::RefCell, task::Waker};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SubscriptionId(usize);

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

struct Subscriber {
    id: SubscriptionId,
    interest: u64,
    ready: bool,
    waker: Waker,
}

/// Level-triggered readiness wire.
#[derive(Clone)]
pub struct RawQueue {
    state: Rc<RefCell<RawQueueState>>,
}

struct RawQueueState {
    ready_bits: u64,
    terminal: bool,
    next_subscription: usize,
    subscribers: Vec<Subscriber>,
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
            state: Rc::new(RefCell::new(RawQueueState {
                ready_bits: 0,
                terminal: false,
                next_subscription: 0,
                subscribers: Vec::new(),
            })),
        }
    }

    pub fn subscribe(&self, interest: u64, waker: Waker) -> RawQueueSubscription {
        let mut state = self.state.borrow_mut();
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
        let mut state = self.state.borrow_mut();
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
            let mut state = self.state.borrow_mut();
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
        let mut state = self.state.borrow_mut();
        if state.terminal {
            return Err(RawWireError::Terminal);
        }

        state.ready_bits &= !bits;
        Ok(())
    }

    pub fn peek(&self) -> u64 {
        self.state.borrow().ready_bits
    }

    pub fn terminate(&self, terminal_bits: u64) -> usize {
        let mut wakers = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            if state.terminal {
                return 0;
            }

            state.terminal = true;
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
        woke
    }

    pub fn is_terminal(&self) -> bool {
        self.state.borrow().terminal
    }

    pub fn subscriber_count(&self) -> usize {
        self.state.borrow().subscribers.len()
    }

    fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut state = self.state.borrow_mut();
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
        let mut state = self.state.borrow_mut();
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
        let mut state = self.state.borrow_mut();
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
        let state = self.state.borrow();
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

/// Edge-triggered event wire.
#[derive(Clone)]
pub struct RawPort {
    state: Rc<RefCell<RawPortState>>,
}

struct RawPortState {
    terminal: bool,
    next_subscription: usize,
    subscribers: Vec<Subscriber>,
}

/// Subscription token returned by [`RawPort::subscribe`].
pub struct RawPortSubscription {
    port: RawPort,
    id: Option<SubscriptionId>,
    terminal_snapshot: bool,
}

impl RawPort {
    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(RawPortState {
                terminal: false,
                next_subscription: 0,
                subscribers: Vec::new(),
            })),
        }
    }

    pub fn subscribe(&self, interest: u64, waker: Waker) -> RawPortSubscription {
        let mut state = self.state.borrow_mut();
        if state.terminal {
            drop(state);
            waker.wake_by_ref();
            return RawPortSubscription {
                port: self.clone(),
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
    ) -> Result<RawPortSubscription, RawWireError> {
        let mut state = self.state.borrow_mut();
        if state.terminal {
            return Err(RawWireError::Terminal);
        }

        Ok(self.subscribe_with_state(&mut state, interest, waker))
    }

    fn subscribe_with_state(
        &self,
        state: &mut RawPortState,
        interest: u64,
        waker: Waker,
    ) -> RawPortSubscription {
        let id = SubscriptionId(state.next_subscription);
        state.next_subscription = state.next_subscription.wrapping_add(1);
        state.subscribers.push(Subscriber {
            id,
            interest,
            ready: false,
            waker,
        });
        RawPortSubscription {
            port: self.clone(),
            id: Some(id),
            terminal_snapshot: false,
        }
    }

    pub fn fire(&self, event: u64) -> usize {
        self.try_fire(event).unwrap_or(0)
    }

    pub fn try_fire(&self, event: u64) -> Result<usize, RawWireError> {
        if event == 0 {
            return Ok(0);
        }

        let mut wakers = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            if state.terminal {
                return Err(RawWireError::Terminal);
            }

            for subscriber in &mut state.subscribers {
                if !subscriber.ready && subscriber.interest & event != 0 {
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

    pub fn terminate(&self, gone_event: u64) -> usize {
        let mut wakers = Vec::new();
        {
            let mut state = self.state.borrow_mut();
            if state.terminal {
                return 0;
            }

            state.terminal = true;
            if gone_event == 0 {
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
        woke
    }

    pub fn is_terminal(&self) -> bool {
        self.state.borrow().terminal
    }

    pub fn subscriber_count(&self) -> usize {
        self.state.borrow().subscribers.len()
    }

    fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut state = self.state.borrow_mut();
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
        let mut state = self.state.borrow_mut();
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
        let mut state = self.state.borrow_mut();
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
        let state = self.state.borrow();
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

    pub fn update(&mut self, interest: u64, waker: Waker) {
        let _ = self.try_update(interest, waker);
    }

    pub fn try_update(&mut self, interest: u64, waker: Waker) -> Result<(), RawSubscriptionError> {
        match self.id {
            Some(id) => self.port.update_subscription(id, interest, waker),
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
            Some(id) => self.port.take_ready(id),
            None if self.terminal_snapshot => Err(RawSubscriptionError::Terminal),
            None => Err(RawSubscriptionError::Unsubscribed),
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

/// Passive trace publication placeholder.
pub struct RawTrace;

impl RawTrace {
    pub const fn new() -> Self {
        Self
    }

    pub fn emit(&self) {}
}

impl Default for RawTrace {
    fn default() -> Self {
        Self::new()
    }
}
