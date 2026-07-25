# Part 1 — The Reactor as a Scheduler

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 0 — A Primer](00_async-primer.md) · **Next:** [Part 2 — A Thread as a Future](02_thread-as-a-future.md)

A textbook scheduler keeps a set of run-queues, and a `schedule()` routine that
picks the next runnable thread and *context-switches* into it. The reactor does
the same job with one structural change: instead of switching stacks, it **polls a
future**. This chapter shows that the reactor is a scheduler, term for term.

## The shape of a traditional scheduler

```
schedule():
    prev = current
    next = pick_next(run_queues)      # priority / fairness policy
    if next != prev:
        switch_to(prev, next)          # save prev regs, restore next regs
```

`switch_to` is the magic: it saves the outgoing thread's registers onto its kernel
stack and restores the incoming thread's. The threads themselves are passive — the
scheduler drives them by switching the CPU between their stacks.

## The shape of the reactor

The reactor replaces `switch_to` with `future.poll()`. "Saving registers" is no
longer a thing the scheduler does — the future *already holds its own state* (Part
0). Picking the next task is still a priority/fairness policy over run-queues.

### The task: a boxed future plus bookkeeping

```rust
// crates/tx-reactor/src/task.rs
type TaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct Task {
    future: Option<TaskFuture>,        // the thread/work itself
    status: TaskStatus,
    wake_state: Arc<TaskWakeState>,    // shared with this task's Wakers
    mailbox: Arc<TaskMailbox>,         // event channel (signals, readiness)
    // ...
}

enum TaskStatus { Runnable, Polling, Parked, Completed, Cancelled }
```

Compare `TaskStatus` to the classic thread states:

| Traditional | txKernel |
|---|---|
| `TASK_RUNNING` (on run-queue) | `Runnable` |
| currently executing on a CPU | `Polling` |
| `TASK_INTERRUPTIBLE` / on a wait-queue | `Parked` |
| `TASK_DEAD` / zombie reaped | `Completed` / `Cancelled` |

A `Task` is **not** an OS thread. It is a unit of pollable work. A userspace
thread is *one kind* of task (its future is `run_thread`, [Part 2](02_thread-as-a-future.md)),
but kernel background work (the net stack, child-publish) are tasks too. They all
multiplex onto the harts the reactor runs on.

### The run-queues: priority classes

```rust
// crates/tx-reactor/src/scheduler.rs
struct HartRunQueues {
    kernel_queue:    VecDeque<TaskId>,   // kernel-only cooperative work
    boosted_queue:   VecDeque<TaskId>,   // temporarily elevated
    new_queue:       VecDeque<TaskId>,   // freshly runnable, short slice
    preempted_queue: VecDeque<TaskId>,   // used up a time slice, re-queued
}
```

Each hart has its own set (cache locality, less contention) — the moral equivalent
of per-CPU run-queues. Selection runs in priority order, with fairness nudges:

```rust
// Phase1Scheduler::pick_next_from_local  (scheduler.rs:1271)
fn pick_next_from_local(hart) -> Option<(TaskHandle, SliceConfig)> {
    pop(kernel_queue)
        .or_else(|| pop(boosted_queue))
        .or_else(|| pop_fair_aged_preempted())     // anti-starvation
        .or_else(|| pop_latency_wake_preempted())  // woken-recently boost
        .or_else(|| pop(new_queue))
        .or_else(|| pop(preempted_queue))
}
```

If this hart's queues are empty it tries to **work-steal** from another hart before
going idle — same idea as a work-stealing run-queue in a modern SMP scheduler.

## The poll loop = the scheduler loop

Here is the core of the reactor, pseudocoded from
`run_until_idle_on_hart_with_reschedule_and_slice_clock` (`runtime.rs:786`):

```rust
loop {
    // (1) Wakes first: move newly-woken tasks Parked -> Runnable.
    drain_wakes_for_hart(hart);                       // runtime.rs:704

    // (2) Pick the next runnable task (or steal one; else we're idle).
    let (handle, slice) = match pick_next_or_steal_local(hart) {
        Some(x) => x,
        None    => break,                             // nothing runnable: idle
    };

    // (3) Take the future out of its slot, mark it Polling.
    //     This is the "switch_to": but we take a future, not a stack.
    let (key, mut future, wake_state) =
        tasks.lock().take_runnable_future_by_id(handle.id());
    set_current_mailbox(hart, task.mailbox);

    // (4) Build the Waker that will re-queue *this* task when called.
    let waker = task_waker(wake_state);               // waker.rs:49
    let mut cx = Context::from_waker(&waker);

    // (5) Run the task: poll it, timed against its slice.
    let result = future.as_mut().poll(&mut cx);       // runtime.rs:849

    // (6) Handle the outcome.
    match result {
        Poll::Ready(()) => {
            tasks.lock().finish_polled_complete(key, future);  // reap
            scheduler.task_dropped(key.id());
        }
        Poll::Pending => match tasks.lock().finish_polled_pending(key, future) {
            // It woke itself during the poll: keep it runnable.
            Woken { hint } => mark_runnable_from_hart(key, hint, hart),
            // It is genuinely blocked: leave it Parked until a Waker fires.
            Parked         => task_stopped_local(key.id(), Blocked),
        },
    }
}
```

Map it to `schedule()`:

| Reactor step | Scheduler analogue |
|---|---|
| (1) `drain_wakes_for_hart` | move `wake_up`'d threads onto the run-queue |
| (2) `pick_next_or_steal_local` | `pick_next(run_queues)` (+ work-stealing) |
| (3) take future, mark `Polling` | choose `next`, mark it running |
| (4) build `Waker` | (no analogue — this is the futures-specific bit) |
| (5) `future.poll(cx)` | `switch_to(prev, next)` |
| (6a) `Ready` → reap | thread exited → reap zombie |
| (6b) `Pending`/`Parked` | thread blocked → leave off run-queue |

The profound difference is step (5). `switch_to` *jumps into* the next thread and
does not come back until that thread blocks or is preempted. `poll` *calls* the
future and **always returns** — either `Ready` or `Pending`. The reactor stays in
control the whole time. There is no separate scheduler stack to trampoline through;
the loop simply continues to the next iteration.

## How a task gets woken (the other half)

A parked task is re-queued when someone calls its `Waker`. The waker is backed by:

```rust
// crates/tx-reactor/src/waker.rs
struct TaskWakeState {
    task: TaskId,
    wake_queue: Arc<SpinLock<VecDeque<TaskId>>>,   // global; all harts drain it
    wake_requested: AtomicBool,
}

impl TaskWakeState {
    fn wake(&self) {                                // waker.rs:31
        self.wake_requested.store(true, Release);
        self.wake_queue.lock().push_back(self.task);
    }
}
```

So `waker.wake()` just pushes a `TaskId` onto a queue and sets a flag. The next
turn of the loop, step (1) `drain_wakes_for_hart` pops those IDs, flips each task
`Parked → Runnable`, and routes it to a run-queue (`runtime.rs:704`). This is
`wake_up()` followed by "the woken thread becomes schedulable" — split across the
waker and the next loop iteration.

## Preemption and time-slices

Cooperative-only scheduling would let a CPU-bound future hog a hart. The reactor
times each poll against a slice and uses preemption markers:

```rust
// crates/tx-reactor/src/scheduler.rs
enum SliceConfig {
    Cooperative,            // runs until it yields (returns Pending/Ready)
    Preemptive { slice_ns },// budgeted; overrun -> re-queue to preempted_queue
}

// crates/tx-reactor/src/preempt.rs  (PreemptionPoint, ~:92)
// atomic marker bits: NeedResched, SliceExpired, UserspacePreempt
```

But here is the subtlety that the rest of the series turns on: **`poll` is
cooperative by construction** — the reactor cannot interrupt a future in the middle
of `poll`. So how does a CPU-bound *userspace* thread get preempted? Not by
interrupting its poll — but by the timer interrupt firing *while the thread is in
userspace*, which turns into a `TimerPreempt` that makes the thread's future
voluntarily `yield_now().await` at a safe point. That mechanism is the bridge
between hardware preemption and cooperative polling, and we build it up across
[Part 3](03_traps-and-handoff.md) and [Part 6](06_interrupts-and-wakes.md).

## Where the reactor lives

```rust
// crates/tx-reactor/src/runtime.rs
struct Reactor { shared: Arc<ReactorShared>, /* per-hart locals */ }

struct ReactorShared {
    tasks: SpinLock<TaskTable>,        // slot allocator: TaskId -> Task
    scheduler: Phase1Scheduler,        // the run-queues + policy
    timers: SpinLock<TimerQueue>,      // deadline wakes (nanosleep, futex timeout)
    // ...
}
```

`TaskTable` is a slot allocator with a free-list and a generation-checked
`TaskKey`, so a reused `TaskId` can't be confused with a dead one (the ABA guard).
Tasks are submitted with `submit_task` (`runtime.rs:1293`); we will see the
userspace thread submitted this way at boot in [Part 2](02_thread-as-a-future.md).

## Summary

- A `Task` is a `Pin<Box<dyn Future>>` plus status/waker/mailbox. It is a unit of
  pollable work, **not** an OS thread; a userspace thread is one kind of task.
- The reactor keeps per-hart, priority-classed run-queues and work-steals — a
  recognizable SMP scheduler.
- The poll loop is `schedule()` with `switch_to` replaced by `future.poll(cx)`.
  The crucial property: **poll always returns**, so the executor never loses
  control.
- Waking is split: `Waker::wake()` enqueues a `TaskId`; the next loop iteration
  drains it and makes the task runnable. That is `wake_up()` in two halves.
- Polling is cooperative; preempting a *userspace* thread is done via the timer
  trap, not by interrupting a poll. (Parts 3 & 6.)

---

**Anchors:**
- Poll loop: `crates/tx-reactor/src/runtime.rs:786`; drain wakes `:704`; poll call `:849`; submit `:1293`
- Run-queues + selection: `crates/tx-reactor/src/scheduler.rs:357`, pick `:1271`
- Task + status + future type: `crates/tx-reactor/src/task.rs:113,90,21`
- Waker: `crates/tx-reactor/src/waker.rs:16,31,49`
- Preemption markers: `crates/tx-reactor/src/preempt.rs:92`

**Next:** [Part 2 — A Thread as a Future](02_thread-as-a-future.md)
