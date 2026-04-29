#![no_std]
//! Minimal task-aware reactor machinery.
//!
//! This crate is still below the full REACTOR_v0 contract: signal interruption,
//! AST slots, scheduler policy hooks, and the long-running idle loop are not
//! implemented yet. The implemented invariant is narrower and load-bearing for
//! those later pieces: each submitted task owns the wake state used by its
//! `Waker`, so a wake marks exactly that task runnable and does not authorize
//! semantic truth. `wait_event` makes that re-observation rule explicit by
//! rechecking its condition after every channel wake or timeout wake.

extern crate alloc;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

pub mod ast {
    pub struct AstSlot;
}

pub mod preempt {
    pub struct PreemptionPoint;
}

pub mod task {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct TaskId(pub usize);

    /// Current reactor-visible state of a task future.
    ///
    /// These states describe polling mechanics only. They are not thread,
    /// process, or subsystem state, and do not carry object-model authority.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum TaskStatus {
        Runnable,
        Polling,
        Parked,
        Completed,
    }
}

pub mod scheduler {
    use alloc::{collections::VecDeque, vec::Vec};

    use crate::task::TaskId;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct HartId(pub usize);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct TaskHandle {
        id: TaskId,
    }

    impl TaskHandle {
        pub const fn new(id: TaskId) -> Self {
            Self { id }
        }

        pub const fn id(self) -> TaskId {
            self.id
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum SliceConfig {
        Cooperative,
        Preemptive { slice_ns: u64 },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum StopReason {
        Blocked,
        Completed,
        Yielded,
        SliceExpired,
        UserspaceTrap,
        PreemptedExternal,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum WakeHint {
        Normal,
        SignalDelivery,
        PriorityBoost,
        None,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum SchedClass {
        Fair,
        RtFifo,
        RtRoundRobin,
        Deadline,
        Idle,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct InitialSchedMeta {
        pub class: SchedClass,
        pub nice: i8,
        pub rt_priority: u8,
        pub affinity: u64,
        pub kernel_only: bool,
    }

    impl InitialSchedMeta {
        pub const fn fair() -> Self {
            Self {
                class: SchedClass::Fair,
                nice: 0,
                rt_priority: 0,
                affinity: u64::MAX,
                kernel_only: false,
            }
        }

        pub const fn kernel() -> Self {
            Self {
                class: SchedClass::Fair,
                nice: 0,
                rt_priority: 0,
                affinity: u64::MAX,
                kernel_only: true,
            }
        }
    }

    pub trait SchedulerPolicy {
        fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)>;
        fn task_stopped(
            &mut self,
            task: TaskId,
            reason: StopReason,
            consumed_ns: u64,
            hart: HartId,
        );
        fn task_runnable(&mut self, task: TaskId, hint: WakeHint);
        fn task_submitted(
            &mut self,
            task: TaskId,
            handle: TaskHandle,
            initial_meta: InitialSchedMeta,
        );
        fn task_dropped(&mut self, task: TaskId);

        fn defer_kernel_work(&self) -> bool {
            false
        }
    }

    #[derive(Clone, Debug)]
    pub struct Phase1Scheduler {
        meta: Vec<Option<TaskSchedMeta>>,
        per_hart: Vec<HartSchedLocal>,
    }

    #[derive(Clone, Debug)]
    struct TaskSchedMeta {
        handle: TaskHandle,
        class: SchedClass,
        remaining_budget_ns: u64,
        current_slice_ns: u64,
        total_runtime_ns: u64,
        last_hart: Option<HartId>,
        kernel_only: bool,
        queued: bool,
    }

    #[derive(Clone, Debug)]
    struct HartSchedLocal {
        kernel_queue: VecDeque<TaskId>,
        new_queue: VecDeque<TaskId>,
        preempted_queue: VecDeque<TaskId>,
    }

    impl HartSchedLocal {
        fn new() -> Self {
            Self {
                kernel_queue: VecDeque::new(),
                new_queue: VecDeque::new(),
                preempted_queue: VecDeque::new(),
            }
        }
    }

    impl Phase1Scheduler {
        pub const BASE_SLICE_NS: u64 = 10_000_000;
        pub const NEW_QUEUE_SLICE_NS: u64 = 1_000_000;
        pub const PREEMPTED_QUEUE_SLICE_NS: u64 = 10_000_000;

        pub fn new() -> Self {
            Self {
                meta: Vec::new(),
                per_hart: alloc::vec![HartSchedLocal::new()],
            }
        }

        pub fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
            <Self as SchedulerPolicy>::pick_next(self, hart)
        }

        pub fn task_stopped(
            &mut self,
            task: TaskId,
            reason: StopReason,
            consumed_ns: u64,
            hart: HartId,
        ) {
            <Self as SchedulerPolicy>::task_stopped(self, task, reason, consumed_ns, hart);
        }

        pub fn task_runnable(&mut self, task: TaskId, hint: WakeHint) {
            <Self as SchedulerPolicy>::task_runnable(self, task, hint);
        }

        pub fn task_submitted(
            &mut self,
            task: TaskId,
            handle: TaskHandle,
            initial_meta: InitialSchedMeta,
        ) {
            <Self as SchedulerPolicy>::task_submitted(self, task, handle, initial_meta);
        }

        pub fn task_dropped(&mut self, task: TaskId) {
            <Self as SchedulerPolicy>::task_dropped(self, task);
        }

        pub fn peek_next(&self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
            let local = self.per_hart.get(hart.0)?;
            if let Some(result) = self.peek_queue(&local.kernel_queue, SliceConfig::Cooperative) {
                return Some(result);
            }
            if let Some(result) = self.peek_queue(
                &local.new_queue,
                SliceConfig::Preemptive {
                    slice_ns: Self::NEW_QUEUE_SLICE_NS,
                },
            ) {
                return Some(result);
            }
            self.peek_preempted_queue(&local.preempted_queue)
        }

        fn peek_queue(
            &self,
            queue: &VecDeque<TaskId>,
            slice: SliceConfig,
        ) -> Option<(TaskHandle, SliceConfig)> {
            queue
                .iter()
                .find_map(|task| self.meta_for(*task).map(|meta| (meta.handle, slice)))
        }

        fn peek_preempted_queue(
            &self,
            queue: &VecDeque<TaskId>,
        ) -> Option<(TaskHandle, SliceConfig)> {
            queue.iter().find_map(|task| {
                self.meta_for(*task).map(|meta| {
                    let slice_ns = if meta.remaining_budget_ns > 0 {
                        meta.remaining_budget_ns
                    } else {
                        Self::PREEMPTED_QUEUE_SLICE_NS
                    };
                    (meta.handle, SliceConfig::Preemptive { slice_ns })
                })
            })
        }

        fn meta_for(&self, task: TaskId) -> Option<&TaskSchedMeta> {
            self.meta.get(task.0).and_then(Option::as_ref)
        }

        fn meta_for_mut(&mut self, task: TaskId) -> Option<&mut TaskSchedMeta> {
            self.meta.get_mut(task.0).and_then(Option::as_mut)
        }

        fn ensure_hart(&mut self, hart: HartId) {
            while self.per_hart.len() <= hart.0 {
                self.per_hart.push(HartSchedLocal::new());
            }
        }

        fn enqueue_kernel(&mut self, task: TaskId, hart: HartId) {
            self.enqueue(task, hart, QueueKind::Kernel, false);
        }

        fn enqueue_new(&mut self, task: TaskId, hart: HartId) {
            self.enqueue(task, hart, QueueKind::New, false);
        }

        fn enqueue_preempted_back(&mut self, task: TaskId, hart: HartId) {
            self.enqueue(task, hart, QueueKind::Preempted, false);
        }

        fn enqueue_preempted_front(&mut self, task: TaskId, hart: HartId) {
            self.enqueue(task, hart, QueueKind::Preempted, true);
        }

        fn enqueue(&mut self, task: TaskId, hart: HartId, queue: QueueKind, front: bool) {
            if self.meta_for(task).map(|meta| meta.queued) != Some(false) {
                return;
            }

            self.ensure_hart(hart);
            if let Some(meta) = self.meta_for_mut(task) {
                meta.queued = true;
            }
            let local = &mut self.per_hart[hart.0];
            let target = match queue {
                QueueKind::Kernel => &mut local.kernel_queue,
                QueueKind::New => &mut local.new_queue,
                QueueKind::Preempted => &mut local.preempted_queue,
            };
            if front {
                target.push_front(task);
            } else {
                target.push_back(task);
            }
        }

        fn pop_from_queue(
            &mut self,
            hart: HartId,
            queue: QueueKind,
        ) -> Option<(TaskHandle, SliceConfig)> {
            loop {
                let task = {
                    let local = self.per_hart.get_mut(hart.0)?;
                    let queue = match queue {
                        QueueKind::Kernel => &mut local.kernel_queue,
                        QueueKind::New => &mut local.new_queue,
                        QueueKind::Preempted => &mut local.preempted_queue,
                    };
                    queue.pop_front()?
                };

                let slice = match queue {
                    QueueKind::Kernel => SliceConfig::Cooperative,
                    QueueKind::New => SliceConfig::Preemptive {
                        slice_ns: Self::NEW_QUEUE_SLICE_NS,
                    },
                    QueueKind::Preempted => {
                        let slice_ns = self
                            .meta_for(task)
                            .map(|meta| {
                                if meta.remaining_budget_ns > 0 {
                                    meta.remaining_budget_ns
                                } else {
                                    Self::PREEMPTED_QUEUE_SLICE_NS
                                }
                            })
                            .unwrap_or(Self::PREEMPTED_QUEUE_SLICE_NS);
                        SliceConfig::Preemptive { slice_ns }
                    }
                };

                if let Some(meta) = self.meta_for_mut(task) {
                    meta.queued = false;
                    meta.current_slice_ns = match slice {
                        SliceConfig::Cooperative => 0,
                        SliceConfig::Preemptive { slice_ns } => slice_ns,
                    };
                    if meta.remaining_budget_ns == 0 {
                        meta.remaining_budget_ns = meta.current_slice_ns;
                    }
                    return Some((meta.handle, slice));
                }
            }
        }
    }

    enum QueueKind {
        Kernel,
        New,
        Preempted,
    }

    impl SchedulerPolicy for Phase1Scheduler {
        fn pick_next(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
            self.ensure_hart(hart);
            self.pop_from_queue(hart, QueueKind::Kernel)
                .or_else(|| self.pop_from_queue(hart, QueueKind::New))
                .or_else(|| self.pop_from_queue(hart, QueueKind::Preempted))
        }

        fn task_stopped(
            &mut self,
            task: TaskId,
            reason: StopReason,
            consumed_ns: u64,
            hart: HartId,
        ) {
            let mut requeue = None;
            if let Some(meta) = self.meta_for_mut(task) {
                meta.total_runtime_ns = meta.total_runtime_ns.saturating_add(consumed_ns);
                meta.last_hart = Some(hart);
                meta.remaining_budget_ns = meta.remaining_budget_ns.saturating_sub(consumed_ns);

                match reason {
                    StopReason::SliceExpired | StopReason::Yielded => {
                        meta.remaining_budget_ns = 0;
                        requeue = Some((QueueKind::Preempted, false));
                    }
                    StopReason::UserspaceTrap => {
                        requeue = Some((QueueKind::Preempted, meta.remaining_budget_ns > 0));
                    }
                    StopReason::PreemptedExternal => {
                        requeue = Some((QueueKind::Preempted, true));
                    }
                    StopReason::Blocked | StopReason::Completed => {}
                }
            }

            if let Some((queue, front)) = requeue {
                match (queue, front) {
                    (QueueKind::Preempted, false) => self.enqueue_preempted_back(task, hart),
                    (QueueKind::Preempted, true) => self.enqueue_preempted_front(task, hart),
                    (QueueKind::Kernel, false) => self.enqueue_kernel(task, hart),
                    (QueueKind::New, false) => self.enqueue_new(task, hart),
                    (queue, front) => self.enqueue(task, hart, queue, front),
                }
            }
        }

        fn task_runnable(&mut self, task: TaskId, _hint: WakeHint) {
            let Some(meta) = self.meta_for(task) else {
                return;
            };
            let hart = meta.last_hart.unwrap_or(HartId(0));
            if meta.kernel_only {
                self.enqueue_kernel(task, hart);
            } else {
                match meta.class {
                    SchedClass::Fair => {
                        if meta.remaining_budget_ns > 0 {
                            self.enqueue_preempted_front(task, hart);
                        } else {
                            self.enqueue_new(task, hart);
                        }
                    }
                    SchedClass::RtFifo
                    | SchedClass::RtRoundRobin
                    | SchedClass::Deadline
                    | SchedClass::Idle => self.enqueue_new(task, hart),
                }
            }
        }

        fn task_submitted(
            &mut self,
            task: TaskId,
            handle: TaskHandle,
            initial_meta: InitialSchedMeta,
        ) {
            while self.meta.len() <= task.0 {
                self.meta.push(None);
            }
            self.meta[task.0] = Some(TaskSchedMeta {
                handle,
                class: initial_meta.class,
                remaining_budget_ns: 0,
                current_slice_ns: 0,
                total_runtime_ns: 0,
                last_hart: None,
                kernel_only: initial_meta.kernel_only,
                queued: false,
            });

            if initial_meta.kernel_only {
                self.enqueue_kernel(task, HartId(0));
            } else {
                self.enqueue_new(task, HartId(0));
            }
        }

        fn task_dropped(&mut self, task: TaskId) {
            if let Some(slot) = self.meta.get_mut(task.0) {
                *slot = None;
            }
        }
    }

    impl Default for Phase1Scheduler {
        fn default() -> Self {
            Self::new()
        }
    }
}

pub mod wait {
    use alloc::{rc::Rc, vec::Vec};
    use core::{
        cell::RefCell,
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

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

        const fn intersects(self, other: Self) -> bool {
            self.0 & other.0 != 0
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
        const fn deadline_ns(self) -> Option<u64> {
            match self {
                Self::InterruptibleTimeout(deadline_ns) | Self::KillableTimeout(deadline_ns) => {
                    Some(deadline_ns)
                }
                Self::Uninterruptible | Self::Interruptible | Self::Killable => None,
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
        state: Rc<RefCell<ChannelState>>,
        timers: Option<TimerQueue>,
    }

    struct ChannelState {
        next_waiter: usize,
        waiters: Vec<Waiter>,
        ready: Vec<WaitToken>,
    }

    struct Waiter {
        token: WaitToken,
        mask: Mask,
        waker: Waker,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct WaitToken(usize);

    #[derive(Clone)]
    pub(crate) struct TimerQueue {
        state: Rc<RefCell<TimerQueueState>>,
    }

    struct TimerQueueState {
        now_ns: u64,
        next_timer: usize,
        timers: Vec<TimerWaiter>,
    }

    struct TimerWaiter {
        token: TimerToken,
        deadline_ns: u64,
        waker: Waker,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct TimerToken(usize);

    struct DeadlineFuture {
        timers: TimerQueue,
        deadline_ns: u64,
        token: Option<TimerToken>,
    }

    /// Future returned by `Channel::wait`.
    pub struct WaitFuture {
        channel: Channel,
        mask: Mask,
        token: Option<WaitToken>,
    }

    /// Future returned by `Channel::wait_event`.
    pub struct WaitEventFuture<C> {
        channel: Channel,
        mask: Mask,
        protocol: WaitProtocol,
        condition: C,
        wait: Option<WaitFuture>,
        timer: Option<DeadlineFuture>,
    }

    impl TimerQueue {
        pub(crate) fn new() -> Self {
            Self {
                state: Rc::new(RefCell::new(TimerQueueState {
                    now_ns: 0,
                    next_timer: 0,
                    timers: Vec::new(),
                })),
            }
        }

        pub(crate) fn advance_time_to(&self, now_ns: u64) -> usize {
            let mut wakers = Vec::new();
            {
                let mut state = self.state.borrow_mut();
                state.now_ns = state.now_ns.max(now_ns);
                let mut index = 0;
                while index < state.timers.len() {
                    if state.timers[index].deadline_ns <= state.now_ns {
                        let waiter = state.timers.swap_remove(index);
                        wakers.push(waiter.waker);
                    } else {
                        index += 1;
                    }
                }
            }

            let woke = wakers.len();
            for waker in wakers {
                waker.wake();
            }
            woke
        }

        pub(crate) fn next_deadline_ns(&self) -> Option<u64> {
            self.state
                .borrow()
                .timers
                .iter()
                .map(|waiter| waiter.deadline_ns)
                .min()
        }

        fn wait_until(&self, deadline_ns: u64) -> DeadlineFuture {
            DeadlineFuture {
                timers: self.clone(),
                deadline_ns,
                token: None,
            }
        }

        fn unregister(&self, token: TimerToken) {
            let mut state = self.state.borrow_mut();
            if let Some(index) = state.timers.iter().position(|waiter| waiter.token == token) {
                state.timers.swap_remove(index);
            }
        }
    }

    impl Channel {
        /// Creates an event-only wait channel.
        ///
        /// Timeout-capable wait channels are produced by `Reactor::channel()`
        /// so they share the reactor's deadline queue and clock source.
        pub fn new() -> Self {
            Self {
                state: Rc::new(RefCell::new(ChannelState {
                    next_waiter: 0,
                    waiters: Vec::new(),
                    ready: Vec::new(),
                })),
                timers: None,
            }
        }

        /// Creates a channel wired to a reactor-owned timer queue.
        pub(crate) fn with_timer_queue(timers: TimerQueue) -> Self {
            Self {
                state: Rc::new(RefCell::new(ChannelState {
                    next_waiter: 0,
                    waiters: Vec::new(),
                    ready: Vec::new(),
                })),
                timers: Some(timers),
            }
        }

        pub fn wait(&self, mask: Mask) -> WaitFuture {
            WaitFuture {
                channel: self.clone(),
                mask,
                token: None,
            }
        }

        pub fn wait_event<C>(
            &self,
            mask: Mask,
            protocol: WaitProtocol,
            condition: C,
        ) -> WaitEventFuture<C>
        where
            C: FnMut() -> bool,
        {
            WaitEventFuture {
                channel: self.clone(),
                mask,
                protocol,
                condition,
                wait: None,
                timer: None,
            }
        }

        pub fn fire(&self, mask: Mask) -> usize {
            if mask.is_empty() {
                return 0;
            }

            let mut wakers = Vec::new();
            {
                let mut state = self.state.borrow_mut();
                let mut index = 0;
                while index < state.waiters.len() {
                    if state.waiters[index].mask.intersects(mask) {
                        let waiter = state.waiters.swap_remove(index);
                        state.ready.push(waiter.token);
                        wakers.push(waiter.waker);
                    } else {
                        index += 1;
                    }
                }
            }

            let woke = wakers.len();
            for waker in wakers {
                waker.wake();
            }
            woke
        }

        fn unregister(&self, token: WaitToken) {
            let mut state = self.state.borrow_mut();
            if let Some(index) = state
                .waiters
                .iter()
                .position(|waiter| waiter.token == token)
            {
                state.waiters.swap_remove(index);
            }
            if let Some(index) = state.ready.iter().position(|ready| *ready == token) {
                state.ready.swap_remove(index);
            }
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

            let mut state = this.channel.state.borrow_mut();
            match this.token {
                Some(token) => {
                    if let Some(index) = state.ready.iter().position(|ready| *ready == token) {
                        state.ready.swap_remove(index);
                        this.token = None;
                        return Poll::Ready(WaitOutcome::Ready);
                    }

                    if let Some(waiter) = state
                        .waiters
                        .iter_mut()
                        .find(|waiter| waiter.token == token)
                    {
                        waiter.mask = this.mask;
                        waiter.waker = cx.waker().clone();
                    } else {
                        state.waiters.push(Waiter {
                            token,
                            mask: this.mask,
                            waker: cx.waker().clone(),
                        });
                    }
                }
                None => {
                    let token = WaitToken(state.next_waiter);
                    state.next_waiter = state.next_waiter.wrapping_add(1);
                    state.waiters.push(Waiter {
                        token,
                        mask: this.mask,
                        waker: cx.waker().clone(),
                    });
                    this.token = Some(token);
                }
            }

            Poll::Pending
        }
    }

    impl Drop for WaitFuture {
        fn drop(&mut self) {
            if let Some(token) = self.token.take() {
                self.channel.unregister(token);
            }
        }
    }

    impl Future for DeadlineFuture {
        type Output = WaitOutcome;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let mut state = this.timers.state.borrow_mut();
            if state.now_ns >= this.deadline_ns {
                this.token = None;
                return Poll::Ready(WaitOutcome::TimedOut);
            }

            match this.token {
                Some(token) => {
                    if let Some(waiter) =
                        state.timers.iter_mut().find(|waiter| waiter.token == token)
                    {
                        waiter.deadline_ns = this.deadline_ns;
                        waiter.waker = cx.waker().clone();
                    } else {
                        state.timers.push(TimerWaiter {
                            token,
                            deadline_ns: this.deadline_ns,
                            waker: cx.waker().clone(),
                        });
                    }
                }
                None => {
                    let token = TimerToken(state.next_timer);
                    state.next_timer = state.next_timer.wrapping_add(1);
                    state.timers.push(TimerWaiter {
                        token,
                        deadline_ns: this.deadline_ns,
                        waker: cx.waker().clone(),
                    });
                    this.token = Some(token);
                }
            }

            Poll::Pending
        }
    }

    impl Drop for DeadlineFuture {
        fn drop(&mut self) {
            if let Some(token) = self.token.take() {
                self.timers.unregister(token);
            }
        }
    }

    impl<C> Future for WaitEventFuture<C>
    where
        C: FnMut() -> bool + Unpin,
    {
        type Output = WaitOutcome;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let deadline_ns = this.protocol.deadline_ns();

            loop {
                if (this.condition)() {
                    this.wait = None;
                    this.timer = None;
                    return Poll::Ready(WaitOutcome::Ready);
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
                    return Poll::Pending;
                }

                let wait = this
                    .wait
                    .get_or_insert_with(|| this.channel.wait(this.mask));
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
}

pub use scheduler::{
    HartId, InitialSchedMeta, Phase1Scheduler, SchedClass, SchedulerPolicy, SliceConfig,
    StopReason, TaskHandle, WakeHint,
};
pub use task::{TaskId, TaskStatus};

type TaskFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunStats {
    pub polled: usize,
    pub completed: usize,
}

pub struct Reactor {
    tasks: Vec<Task>,
    scheduler: Phase1Scheduler,
    next_task_id: usize,
    timers: wait::TimerQueue,
}

struct Task {
    id: TaskId,
    future: Option<TaskFuture>,
    status: TaskStatus,
    wake_state: Arc<TaskWakeState>,
    last_stop_reason: Option<StopReason>,
}

/// Shared state behind every waker cloned from a task poll.
///
/// A single atomic bit is enough for the v0 smoke executor: multiple wake calls
/// before the next drain coalesce into one runnable transition, and the future
/// decides on poll whether the underlying wait condition actually became true.
struct TaskWakeState {
    wake_requested: AtomicBool,
}

impl TaskWakeState {
    fn new() -> Self {
        Self {
            wake_requested: AtomicBool::new(false),
        }
    }

    fn wake(&self) {
        self.wake_requested.store(true, Ordering::Release);
    }

    fn take_wake(&self) -> bool {
        self.wake_requested.swap(false, Ordering::AcqRel)
    }

    fn clear(&self) {
        self.wake_requested.store(false, Ordering::Release);
    }
}

impl Reactor {
    pub fn new() -> Self {
        Self {
            tasks: Vec::new(),
            scheduler: Phase1Scheduler::new(),
            next_task_id: 0,
            timers: wait::TimerQueue::new(),
        }
    }

    /// Creates a wait channel attached to this reactor's timer queue.
    pub fn channel(&self) -> wait::Channel {
        wait::Channel::with_timer_queue(self.timers.clone())
    }

    /// Advances the reactor-owned absolute nanosecond clock and wakes expired timers.
    pub fn advance_time_to(&mut self, now_ns: u64) -> usize {
        self.timers.advance_time_to(now_ns)
    }

    pub fn next_deadline_ns(&self) -> Option<u64> {
        self.timers.next_deadline_ns()
    }

    pub fn submit<F>(&mut self, future: F) -> TaskId
    where
        F: Future<Output = ()> + 'static,
    {
        let id = TaskId(self.next_task_id);
        self.next_task_id += 1;
        self.tasks.push(Task {
            id,
            future: Some(Box::pin(future)),
            status: TaskStatus::Runnable,
            wake_state: Arc::new(TaskWakeState::new()),
            last_stop_reason: None,
        });
        self.scheduler
            .task_submitted(id, TaskHandle::new(id), InitialSchedMeta::kernel());
        id
    }

    pub fn run_until_idle(&mut self) -> RunStats {
        let mut stats = RunStats {
            polled: 0,
            completed: 0,
        };
        loop {
            // External wakers only set a task-local bit. The reactor owns the
            // transition from Parked to Runnable and does it at poll-loop
            // boundaries, where duplicate wakes naturally coalesce.
            self.drain_wakes();
            let Some((handle, _slice)) = self.scheduler.pick_next(HartId(0)) else {
                break;
            };
            let index = handle.id().0;

            if self.tasks[index].status != TaskStatus::Runnable
                || self.tasks[index].future.is_none()
            {
                continue;
            }

            let wake_state = Arc::clone(&self.tasks[index].wake_state);
            let waker = task_waker(wake_state);
            let mut cx = Context::from_waker(&waker);

            let poll = {
                let task = &mut self.tasks[index];
                debug_assert_eq!(task.id.0, index);
                task.status = TaskStatus::Polling;
                task.wake_state.clear();

                let Some(future) = task.future.as_mut() else {
                    continue;
                };

                stats.polled += 1;
                future.as_mut().poll(&mut cx)
            };

            match poll {
                Poll::Ready(()) => {
                    let task = &mut self.tasks[index];
                    task.future = None;
                    task.status = TaskStatus::Completed;
                    task.wake_state.clear();
                    task.last_stop_reason = Some(StopReason::Completed);
                    self.scheduler
                        .task_stopped(task.id, StopReason::Completed, 0, HartId(0));
                    self.scheduler.task_dropped(task.id);
                    stats.completed += 1;
                }
                Poll::Pending => {
                    if self.tasks[index].wake_state.take_wake() {
                        self.mark_runnable(index, WakeHint::Normal);
                    } else {
                        self.tasks[index].status = TaskStatus::Parked;
                        self.tasks[index].last_stop_reason = Some(StopReason::Blocked);
                        self.scheduler.task_stopped(
                            self.tasks[index].id,
                            StopReason::Blocked,
                            0,
                            HartId(0),
                        );
                    }
                }
            }
        }

        stats
    }

    pub fn is_idle(&self) -> bool {
        self.tasks.iter().all(|task| {
            task.status != TaskStatus::Runnable
                && !task.wake_state.wake_requested.load(Ordering::Acquire)
        })
    }

    pub fn task_status(&self, task: TaskId) -> Option<TaskStatus> {
        self.tasks.get(task.0).map(|entry| entry.status)
    }

    pub fn last_stop_reason(&self, task: TaskId) -> Option<StopReason> {
        self.tasks
            .get(task.0)
            .and_then(|entry| entry.last_stop_reason)
    }

    pub fn next_scheduled_task(&mut self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
        self.drain_wakes();
        self.scheduler.peek_next(hart)
    }

    fn drain_wakes(&mut self) -> usize {
        let mut notified = 0;
        for index in 0..self.tasks.len() {
            if self.tasks[index].wake_state.take_wake()
                && self.tasks[index].status == TaskStatus::Parked
            {
                self.mark_runnable(index, WakeHint::Normal);
                notified += 1;
            }
        }
        notified
    }

    fn mark_runnable(&mut self, index: usize, hint: WakeHint) {
        let task = &mut self.tasks[index];
        if task.future.is_some() && task.status != TaskStatus::Runnable {
            task.status = TaskStatus::Runnable;
            self.scheduler.task_runnable(task.id, hint);
        }
    }
}

impl Default for Reactor {
    fn default() -> Self {
        Self::new()
    }
}

fn task_waker(state: Arc<TaskWakeState>) -> Waker {
    unsafe { Waker::from_raw(raw_task_waker(state)) }
}

fn raw_task_waker(state: Arc<TaskWakeState>) -> RawWaker {
    RawWaker::new(Arc::into_raw(state) as *const (), &TASK_WAKER_VTABLE)
}

const TASK_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    task_waker_clone,
    task_waker_wake,
    task_waker_wake_by_ref,
    task_waker_drop,
);

unsafe fn task_waker_clone(data: *const ()) -> RawWaker {
    let state = unsafe { Arc::from_raw(data as *const TaskWakeState) };
    let cloned = Arc::clone(&state);
    let _ = Arc::into_raw(state);
    raw_task_waker(cloned)
}

unsafe fn task_waker_wake(data: *const ()) {
    let state = unsafe { Arc::from_raw(data as *const TaskWakeState) };
    state.wake();
}

unsafe fn task_waker_wake_by_ref(data: *const ()) {
    let state = unsafe { Arc::from_raw(data as *const TaskWakeState) };
    state.wake();
    let _ = Arc::into_raw(state);
}

unsafe fn task_waker_drop(data: *const ()) {
    drop(unsafe { Arc::from_raw(data as *const TaskWakeState) });
}
