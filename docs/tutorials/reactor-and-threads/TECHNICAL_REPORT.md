# Modeling Synchronous Kernel Execution as Futures: The txKernel Reactor and Userspace Thread Architecture

**A Technical Report**

**Subject:** The design and mechanics of txKernel's reactor-driven execution model
**Scope:** Scheduling, context switching, trap and interrupt handling, syscall
dispatch, blocking, and signal delivery — and how each classical synchronous
mechanism is re-expressed in terms of Rust `Future`s and a cooperative executor.
**Audience:** Kernel engineers familiar with the traditional synchronous model
(trap vectors, run-queues, `schedule()`, wait-queues, `wake_up()`) seeking a
precise account of the async re-formulation.
**Conventions:** Code listings are *pseudocode* — simplified control flow with
elided error paths and generics — but all named data-structure types, fields, and
enum variants are accurate to the source tree. Source locations are cited as
`path:line`.

---

## Abstract

A conventional monolithic kernel services a system call synchronously inside the
trap handler: the trap entry saves registers onto a per-thread kernel stack, the
handler runs to completion (sleeping on a wait-queue if it must block), and a
return-from-trap instruction resumes userspace. The defining cost of this model is
that a *blocked* thread holds a *parked kernel stack* for the entire duration of
the wait.

txKernel preserves the conceptual stages of this model but cuts the timeline at the
blocking point. A userspace thread is represented as a Rust `Future`; the *reactor*
is the executor that polls it; and "blocking" becomes the future returning
`Poll::Pending` after registering a `Waker`. No thread busy-waits, and no kernel
stack is pinned across a wait — a suspended thread is a heap-resident future in a
task table, plus a single wait-queue subscription.

This report develops the model bottom-up. It establishes that the reactor is a
recognizable scheduler; that a userspace thread is an `async` state machine whose
saved context lives in an explicit payload rather than on a stack; that a hardware
trap is converted into the resolution of an awaited wait via a tightly-scoped
synchronous "handoff" plus a longjmp back into the executor; that blocking syscalls
are ordinary futures parked on wait-sources; and that interrupts and signals are
wakers and wait-interruptions respectively. A complete end-to-end case study — a
reader blocking on an empty pipe, woken by a writer — ties the layers together. The
central architectural claim, stated once and demonstrated repeatedly, is: **a
suspended thread is a parked future, not a parked kernel stack.**

---

## Table of Contents

1. Introduction and Central Thesis
2. Background: Futures as State Machines
3. The Reactor as a Scheduler
4. The Userspace Thread as a Future
5. Trap Handling and the Synchronous/Asynchronous Handoff
6. The Suspension Point: `UserspaceRunWait`
7. Blocking System Calls as Futures
8. Interrupts and Wakes
9. Signals as Wait Interruption
10. End-to-End Case Study: `read()` on an Empty Pipe
11. Design Discussion and Trade-offs
12. Conclusion
- Appendix A: Type Reference
- Appendix B: Source Anchor Index

---

## 1. Introduction and Central Thesis

### 1.1 The traditional execution model

In a classical monolithic kernel, a thread alternates between two privilege modes.
In user mode it executes instructions until an event forces entry into the kernel:
a system call (`ecall`/`syscall`/`int 0x80`), a fault (page fault, illegal
instruction), or an asynchronous interrupt (timer, device). The hardware vectors to
a fixed trap entry, which saves the interrupted register state and runs a handler.
The handler executes *to completion* on the thread's kernel stack. If it cannot
complete immediately — the canonical example being `read()` on a pipe with no data
— it places the thread on a wait-queue, marks it `TASK_INTERRUPTIBLE`, and calls
`schedule()` to switch to another thread. The blocked thread's kernel stack remains
*parked* at the `schedule()` call site until a `wake_up()` makes it runnable again.

This model is correct, well-understood, and has one structural cost worth isolating:
**the unit of suspension is the kernel stack.** Every thread blocked in the kernel
holds a full kernel stack frozen mid-execution. Ten thousand threads blocked in
`read` is ten thousand kernel stacks resident in memory, each pinned for the
duration of its wait.

### 1.2 The reactor model in one inversion

txKernel keeps every conceptual stage above — trap entry, handler, blocking,
wakeup, return-to-user — but changes the *unit of suspension*. Instead of a kernel
stack frozen at `schedule()`, a suspended computation is a **`Future` that returned
`Poll::Pending`**. The kernel-side logic that services a thread is written as
`async` Rust; the compiler transforms it into a state machine whose fields hold
exactly the state that must survive a suspension. Suspension is returning
`Poll::Pending`; resumption is being polled again.

The consequences cascade:

- A thread is a future, scheduled by an executor (the *reactor*) rather than by a
  stack-switching `schedule()`.
- "Saving the context" of a blocked computation is not copying registers to a
  stack — the future *is* the saved context.
- A blocking syscall is a future that yields `Pending`; the thread parks with no
  kernel stack held.
- `wake_up()` becomes `Waker::wake()`.

### 1.3 The central thesis

> **A suspended thread is a parked future, not a parked kernel stack.**

This single sentence is the organizing claim of the entire architecture. Every
section that follows is, in effect, a demonstration of one facet of it.

### 1.4 What is *not* novel

A recurring risk in reading this system is over-attributing novelty. The following
are *identical* to any conventional kernel and should be recognized as such:

- the assembly trap vector that saves registers into a trap frame;
- the trap-frame memory layout;
- the cause-classification logic (decode `scause`/`cause` into syscall vs. fault
  vs. interrupt);
- a fast-path set of pure, non-blocking syscalls handled synchronously.

The novelty is concentrated at exactly **one seam**: the trap handler, instead of
running the syscall to completion, *records the trap as the resolution of an
awaited wait* and longjmps back into the executor. Section 5 dissects that seam.

### 1.5 The master mapping

The report repeatedly refers to the following correspondence. Each row is
established in the section noted.

| Traditional kernel | txKernel future model | Section |
|---|---|---|
| Thread with a kernel stack | A `Future` (`run_thread`) in a boxed task | §4 |
| Scheduler run-queue + `schedule()` | Reactor poll loop + priority run-queues | §3 |
| Context switch (save/restore registers) | `poll()` returns; the future's state machine *is* the context | §3, §4 |
| Trap entry (asm vector) | Asm vector → trap frame (unchanged) | §5 |
| Handler runs the syscall to completion | Trap shell captures registers + resolves a wait, then longjmps out | §5 |
| Sleep on a wait-queue | `Poll::Pending` after registering a `Waker` on a `WaitSource` | §6, §7 |
| `wake_up()` | `Waker::wake()` → task re-queued for poll | §3, §8 |
| Return-from-trap (`sret`/`iret`) | `enter_userspace_with_context()` (divergent call) | §4, §5 |
| Timer IRQ → preempt → reschedule | Timer trap → `TimerPreempt` → `yield_now().await` | §8 |
| `signal_pending()` in interruptible sleep | `SignalDelivered` wake-hint → re-poll reads `InterruptSummary` → `EINTR` | §9 |

---

## 2. Background: Futures as State Machines

This section establishes the minimum async vocabulary the report relies on. Readers
fluent in `poll`/`Waker` may skip to §3.

### 2.1 Push-driven threads versus pull-driven futures

A traditional kernel thread is *push-driven*: the CPU pushes instructions through it
until something forces it to stop, and its progress lives implicitly in a kernel
stack and a saved register set. A Rust future is *pull-driven*: it makes progress
only when polled, and consumes nothing between polls. Its progress lives explicitly
in a value the compiler generates.

That inversion — *progress lives in a pollable value, not in a switchable stack* —
is the mechanism that lets the kernel suspend a thread without holding a stack.

### 2.2 The `Future` contract

The kernel is `no_std`, but the future contract is the one from `core` and is
unchanged from ordinary Rust:

```rust
pub trait Future {
    type Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output>;
}

pub enum Poll<T> { Ready(T), Pending }
```

Three invariants matter for the rest of the report:

1. **`poll` returns immediately.** It yields `Ready(value)` (done) or `Pending`
   (not yet). It never blocks the OS thread it runs on.
2. **The future owns its resumption state.** Each `poll` resumes where the last
   left off; the "where" is stored in the future's own fields.
3. **`Context` carries a `Waker`.** A future returning `Pending` promises to have
   stashed the `Waker` and to invoke it when it is worth polling again. The
   executor relies on that promise to avoid spinning.

### 2.3 `async fn` compiles to a state machine

The compiler rewrites an `async fn` into a struct implementing `Future`. Each
`.await` point becomes a state; the struct's fields are the locals that must survive
across an `.await`. Given:

```rust
async fn copy_one(src: &Pipe, dst: &Pipe) -> usize {
    let byte = src.read_one().await;   // await point A
    dst.write_one(byte).await;         // await point B
    1
}
```

the compiler produces, conceptually:

```rust
enum CopyOne {
    Start { src, dst },
    AfterRead { dst, read_fut: ReadOne },   // suspended at A
    AfterWrite { write_fut: WriteOne },     // suspended at B
    Done,
}

impl Future for CopyOne {
    type Output = usize;
    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<usize> {
        loop {
            match self.state {
                Start { .. }      => { /* begin read; -> AfterRead */ }
                AfterRead { .. }  => match read_fut.poll(cx) {
                    Pending     => return Poll::Pending,   // still at A
                    Ready(byte) => { /* begin write; -> AfterWrite */ }
                },
                AfterWrite { .. } => match write_fut.poll(cx) {
                    Pending => return Poll::Pending,       // still at B
                    Ready(()) => { self.state = Done; return Poll::Ready(1); }
                },
                Done => unreachable!(),
            }
        }
    }
}
```

The precise shape is unimportant; the principle is decisive: **the saved context of
an async computation is the set of fields of a struct.** No stack is involved.
Suspending is returning `Pending`; resuming is being polled and matching on the
saved state. In §4 the "thread" whose context survives a syscall is exactly such a
struct.

### 2.4 `Waker`: the re-poll request

A future returning `Pending` arranges its own re-polling through the `Waker` in
`cx`:

```rust
fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<T> {
    if self.ready() {
        Poll::Ready(self.take())
    } else {
        self.stash_waker(cx.waker().clone());  // "call me when ready"
        Poll::Pending
    }
}
```

Later, unrelated code — an interrupt handler, a timer, another task — calls
`waker.wake()`. This does *not* run the future; it informs the executor that the
future is worth polling again, and the executor re-queues it. The correspondence to
a wait-queue is exact:

| Traditional | Futures |
|---|---|
| Add `current` to wait-queue, set `TASK_INTERRUPTIBLE`, `schedule()` | Stash `cx.waker()`, return `Poll::Pending` |
| `wake_up(&wq)` | `waker.wake()` |
| Scheduler runs the woken thread | Executor re-polls the woken future |

### 2.5 `Pin` and the executor loop

A self-referential state machine must not move once polled; `Pin<&mut Self>` is the
type-level guarantee that it will not. The stored form of a task is therefore
`Pin<Box<dyn Future>>`. For this report the only consequence is that a running
future has a stable address, which lets other components hold references to its
slot.

An executor is then simply:

```
loop:
    take a future ready to make progress
    build a Waker that re-queues it when called
    match future.poll(Context::from(waker)):
        Ready(_) -> drop the future (done)
        Pending  -> leave it parked; it stashed the Waker
```

Everything in §3 is this loop with run-queues, priorities, time-slices, and harts
layered on.

---

## 3. The Reactor as a Scheduler

The reactor is the executor that drives all kernel work. This section shows that it
is a scheduler in the conventional sense, with one substitution: `switch_to` is
replaced by `future.poll()`.

### 3.1 The traditional scheduler, abstracted

```
schedule():
    prev = current
    next = pick_next(run_queues)      # priority / fairness policy
    if next != prev:
        switch_to(prev, next)          # save prev registers, restore next's
```

`switch_to` is the pivot: it saves the outgoing thread's registers and restores the
incoming thread's, then *jumps into* the new thread, not returning until that thread
itself blocks or is preempted. Threads are passive; the scheduler drives them by
switching the CPU among their stacks.

### 3.2 The task: a boxed future plus bookkeeping

The reactor's unit of work is a `Task`. Its essential fields:

```rust
// crates/tx-reactor/src/task.rs:21,90,113
type TaskFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

struct Task {
    future: Option<TaskFuture>,        // the thread or work item itself
    status: TaskStatus,
    wake_state: Arc<TaskWakeState>,    // shared with this task's Wakers
    mailbox: Arc<TaskMailbox>,         // event channel (signals, readiness)
    // ...
}

enum TaskStatus { Runnable, Polling, Parked, Completed, Cancelled }
```

`TaskStatus` is the classical thread-state set under different names:

| Traditional | txKernel |
|---|---|
| `TASK_RUNNING` (on a run-queue) | `Runnable` |
| Currently executing on a CPU | `Polling` |
| `TASK_INTERRUPTIBLE` / on a wait-queue | `Parked` |
| `TASK_DEAD` / zombie | `Completed` / `Cancelled` |

A critical distinction: **a `Task` is not an OS thread.** It is a unit of pollable
work. A userspace thread is *one kind* of task (its future is `run_thread`, §4), but
kernel-internal work — the network stack, child-publish bookkeeping — are tasks too.
All multiplex onto the harts the reactor runs.

### 3.3 Run-queues and selection

Each hart owns a set of priority-classed run-queues (per-CPU queues, for locality
and reduced contention):

```rust
// crates/tx-reactor/src/scheduler.rs:357
struct HartRunQueues {
    kernel_queue:    VecDeque<TaskId>,   // kernel-only cooperative work
    boosted_queue:   VecDeque<TaskId>,   // temporarily elevated priority
    new_queue:       VecDeque<TaskId>,   // freshly runnable, short slice
    preempted_queue: VecDeque<TaskId>,   // used up a slice, re-queued
}
```

Selection runs in priority order with fairness adjustments:

```rust
// Phase1Scheduler::pick_next_from_local  (scheduler.rs:1271)
fn pick_next_from_local(hart) -> Option<(TaskHandle, SliceConfig)> {
    pop(kernel_queue)
        .or_else(|| pop(boosted_queue))
        .or_else(|| pop_fair_aged_preempted())     // anti-starvation
        .or_else(|| pop_latency_wake_preempted())  // recently-woken boost
        .or_else(|| pop(new_queue))
        .or_else(|| pop(preempted_queue))
}
```

When a hart's queues are empty it attempts to **work-steal** from another hart
before idling — the same design as a modern SMP work-stealing scheduler.

### 3.4 The poll loop is the scheduler loop

The core of the reactor (`runtime.rs:786`,
`run_until_idle_on_hart_with_reschedule_and_slice_clock`), pseudocoded:

```rust
loop {
    // (1) Move newly-woken tasks Parked -> Runnable.
    drain_wakes_for_hart(hart);                       // runtime.rs:704

    // (2) Pick the next runnable task, or work-steal; else idle.
    let (handle, slice) = match pick_next_or_steal_local(hart) {
        Some(x) => x,
        None    => break,
    };

    // (3) Take the future from its slot, mark it Polling. This is "switch_to" —
    //     but we take a future, not a stack.
    let (key, mut future, wake_state) =
        tasks.lock().take_runnable_future_by_id(handle.id());
    set_current_mailbox(hart, task.mailbox);

    // (4) Build the Waker that will re-queue THIS task when invoked.
    let waker = task_waker(wake_state);               // waker.rs:49
    let mut cx = Context::from_waker(&waker);

    // (5) Run the task.
    let result = future.as_mut().poll(&mut cx);       // runtime.rs:849

    // (6) Handle the outcome.
    match result {
        Poll::Ready(()) => {
            tasks.lock().finish_polled_complete(key, future);   // reap
            scheduler.task_dropped(key.id());
        }
        Poll::Pending => match tasks.lock().finish_polled_pending(key, future) {
            Woken { hint } => mark_runnable_from_hart(key, hint, hart), // self-woke
            Parked         => task_stopped_local(key.id(), Blocked),    // genuinely blocked
        },
    }
}
```

Mapped onto `schedule()`:

| Reactor step | Scheduler analogue |
|---|---|
| (1) `drain_wakes_for_hart` | move `wake_up`'d threads onto the run-queue |
| (2) `pick_next_or_steal_local` | `pick_next(run_queues)` (+ work-stealing) |
| (3) take future, mark `Polling` | choose `next`, mark it running |
| (4) build `Waker` | (no analogue; futures-specific) |
| (5) `future.poll(cx)` | `switch_to(prev, next)` |
| (6a) `Ready` → reap | thread exited → reap zombie |
| (6b) `Pending`/`Parked` | thread blocked → leave off run-queue |

The decisive difference is step (5). `switch_to` jumps into the next thread and does
not return until it blocks; `poll` *calls* the future and **always returns** —
`Ready` or `Pending`. The reactor never loses control, and there is no separate
scheduler stack to trampoline through: the loop simply proceeds to its next
iteration.

### 3.5 The wake mechanism

A parked task becomes runnable when its `Waker` is invoked. The waker is backed by:

```rust
// crates/tx-reactor/src/waker.rs:16,31
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

Thus `waker.wake()` merely enqueues a `TaskId` and sets a flag. On the next loop
iteration, step (1) `drain_wakes_for_hart` (`runtime.rs:704`) pops those IDs, flips
each task `Parked → Runnable`, and routes it to a run-queue. The classical
`wake_up()` is therefore split across two moments: the waker enqueues; the next loop
iteration makes the task schedulable.

### 3.6 Preemption and time-slices

Cooperative-only scheduling would let a CPU-bound future monopolize a hart. The
reactor times each poll against a slice and maintains preemption markers:

```rust
// crates/tx-reactor/src/scheduler.rs
enum SliceConfig {
    Cooperative,             // runs until it yields (Pending/Ready)
    Preemptive { slice_ns }, // budgeted; overrun -> re-queue to preempted_queue
}
// crates/tx-reactor/src/preempt.rs:92 — PreemptionPoint: atomic marker bits
//   NeedResched, SliceExpired, UserspacePreempt
```

A subtlety that the rest of the report depends on: **`poll` is cooperative by
construction** — the reactor cannot interrupt a future mid-poll. So how is a
CPU-bound *userspace* thread preempted? Not by interrupting its poll, but by the
timer interrupt firing *while the thread is in userspace*, which is converted into a
`TimerPreempt` that makes the thread's future voluntarily `yield_now().await` at a
safe point. That bridge between hardware preemption and cooperative polling is built
in §5 and §8.

### 3.7 Reactor structure

```rust
// crates/tx-reactor/src/runtime.rs:182,262
struct Reactor { shared: Arc<ReactorShared>, /* per-hart locals */ }

struct ReactorShared {
    tasks: SpinLock<TaskTable>,        // slot allocator: TaskId -> Task
    scheduler: Phase1Scheduler,        // run-queues + policy
    timers: SpinLock<TimerQueue>,      // deadline wakes (nanosleep, futex timeout)
    // ...
}
```

`TaskTable` is a slot allocator with a free list and a generation-checked `TaskKey`,
so a reused `TaskId` cannot be confused with a dead one (an ABA guard). Tasks are
submitted with `submit_task` (`runtime.rs:1293`); §4 shows the userspace thread
submitted this way at boot.

---

## 4. The Userspace Thread as a Future

§3 polled opaque tasks. This section opens one: a userspace thread is the future
returned by `run_thread`. Its `async` body is an infinite loop, each iteration of
which is exactly one userspace round-trip — enter userspace, take a trap, service
it, repeat. The thread *terminates* when its body returns, on `exit`/`exit_group`
or an unrecoverable fault.

### 4.1 The two-mode life, relocated

A classical kernel thread bounces between user mode (runs until a trap) and kernel
mode (handler services the trap, then returns). The state surviving a trip through
the kernel lives in the kernel stack and the trap frame. `run_thread` keeps the
identical two-mode life, but the surviving state lives in two nameable places:

- the **future's own state machine** (locals across `.await`); and
- a **`ThreadPayload`** (the user registers and the pending syscall result).

### 4.2 The top-level future

```rust
// crates/tx-kernel/src/thread_future.rs:256
async fn run_thread<P: TxPlatform>(
    thread:  Cap<ThreadIdentity>,        // identity (epoch-managed capability)
    payload: PayloadCap<ThreadPayload>,  // mutable execution state
) {
    loop {
        // (1) open the entry wait; run the AST checkpoint (signals, stop-state)
        // (2) merge saved registers + pending syscall result; enter userspace
        // (3) .await the trap the trap shell will resolve
        // (4) dispatch the trap: syscall / page-fault / timer / fatal
        // (5) loop
    }
    // exits the loop -> thread terminated
}
```

Both capabilities are deliberately held across every `.await`:
`Cap<ThreadIdentity>` is the thread's identity (safe to hold across yields per
`txdoc:THREAD-4-2-OWNERSHIP`); `PayloadCap<ThreadPayload>` is where the registers
live between polls.

### 4.3 The saved-context home: `ThreadPayload`

This type replaces "kernel stack + trap frame" as the home of suspended thread
state. The execution-relevant fields (signal fields deferred to §9):

```rust
// crates/tx-subsystems/src/thread_runtime/structure.rs:140
struct ThreadPayload {
    task: SpinMutex<Option<TaskKey>>,                  // which reactor task drives this

    userspace_slot: UserspaceRunSlot,                  // the wait the trap shell resolves
    active_request: SpinMutex<Option<UserspaceRunRequest>>, // in-flight wait token

    saved_user_context:     SpinMutex<Option<UserTrapContext>>,  // the user registers
    pending_syscall_return: SpinMutex<Option<Result<i64, i32>>>, // result destined for a0

    mailbox: SpinMutex<Option<Weak<TaskMailbox>>>,     // wake channel (signals/readiness)
    stopped: AtomicBool,                               // SIGSTOP parked?
    proc_sleeping: AtomicBool,                         // procfs "is sleeping" hint
    clear_child_tid: SpinMutex<Option<u64>>,           // futex teardown on exit
    robust_list_head: SpinMutex<Option<u64>>,
    robust_list_len:  SpinMutex<usize>,
    // signal fields (signal_mask, thread_pending, signal_summary,
    //   saved_signal_context, saved_signal_mask, alt_stack) — see §9
}
```

The register set is the HAL type:

```rust
// crates/tx-hal/src/trap.rs:169
struct UserTrapContext {
    regs: [usize; 32],   // x0..x31 (RV64); a0 = regs[10]
    pc:   usize,
    status: usize,       // sstatus
    fp:   UserFpContext, // f0..f31 + fcsr
}
```

Consequently, when a thread is suspended mid-syscall, "its registers" are
`payload.saved_user_context` — a plain struct in a heap-managed payload, not bytes
on a parked stack. This is the concrete realization of §2's "context is a value."

### 4.4 The per-hart slot bridge: `PerHartSlotted`

A problem must be solved before the loop can function. The trap shell (§5) runs in a
*synchronous* context — a hardware trap, with no future and no `Context`. When a trap
fires it must determine *which thread is running on this hart*. It cannot consult the
reactor; it needs a direct pointer.

`PerHartSlotted` is that bridge. It wraps the `run_thread` future and, on every poll,
installs this thread's identity and payload into a per-hart slot, clearing them on
poll exit:

```rust
// crates/tx-kernel/src/thread_future.rs:192
struct PerHartSlotted<P, F> {
    thread:  Cap<ThreadIdentity>,
    payload: PayloadCap<ThreadPayload>,
    inner:   F,                          // the run_thread future
}

impl<P, F: Future> Future for PerHartSlotted<P, F> {
    type Output = F::Output;
    fn poll(self, cx) -> Poll<F::Output> {
        let hart = current_cpu_id();

        // Publish "this thread is live on this hart" for the trap shell.
        set_current_thread_identity(hart, self.thread.clone());
        set_current_thread_payload(hart, self.payload.clone());
        if let Some(mailbox) = current_task_mailbox(hart) {
            self.payload.bind_mailbox(downgrade(mailbox));  // wire wakes -> this task
        }

        let out = self.inner.poll(cx);   // run_thread runs here

        clear_current_thread_payload(hart);
        clear_current_thread_identity(hart);
        out
    }
}
```

Two points deserve emphasis:

- **The slot is cleared on both `Ready` and `Pending`.** There is an intentional
  race: between the wrapper's poll exit and the reactor's next decision, the slot is
  briefly `None`; a trap arriving in that window finds no payload and falls back to a
  terminate policy. The source comment (`thread_future.rs:184`) documents this as
  deliberate — holding the slot across yields would deny it to other futures under
  SMP. This is the precise async/synchronous seam §5 examines.
- **`bind_mailbox`** connects the thread's `ThreadPayload.mailbox` to the reactor
  *task's* mailbox, so a later signal or readiness event reaches the correct `Waker`
  (§8, §9).

### 4.5 The loop body, phase by phase

#### Phase 1 — open the entry wait, run the AST checkpoint

```rust
let entry_wait  = payload.userspace_slot().start_request().expect("slot free");
let entry_token = entry_wait.request();
payload.set_active_userspace_request(Some(entry_token));

while payload.is_stopped() { /* SIGSTOP: park until SIGCONT clears it */ }

ast_checkpoint(&payload);   // turn pending signals into handler frames, etc. (§9)
```

`start_request` returns a `UserspaceRunWait` future and a generation-checked
`UserspaceRunRequest` token. Stashing the token in the payload is what lets the
synchronous trap shell later resolve *this specific* wait. The wait object is the
subject of §6.

#### Phase 2 — merge state and enter userspace

```rust
let mut ctx = UserTrapContext::empty();
prepare_userspace_entry_payload_into(&payload, &mut ctx);  // execution.rs:775
//   folds saved_user_context + pending_syscall_return (-> a0) into ctx
payload.set_active_userspace_request(Some(entry_token));   // re-arm after prepare
<P as TrapIf>::enter_userspace_with_context(&ctx, root);   // DIVERGENT
```

`enter_userspace_with_context` is the `sret`/`iret` analogue. On real hardware it
**does not return**: control leaves the kernel and resumes userspace at `ctx.pc`
with `ctx.regs`. The kernel regains control only through a trap, which longjmps back
*through* this call site (§5). The merge step is the **sole** site where a syscall
result is written into `a0` — the "Plan B" discipline (§5.6).

#### Phase 3 — await the trap

```rust
let trap: UserspaceTrapInfo = entry_wait.await;   // thread_future.rs:624
```

This is the thread's single suspension point per round-trip. On real hardware the
trap shell has already resolved the wait by the time the future is re-polled, so the
`.await` returns `Ready` immediately. On the host test platform there is no real
trap, so it genuinely returns `Pending` and a test driver resolves it later (§6.5).

#### Phase 4 — dispatch the resolved trap

```rust
// crates/tx-reactor/src/userspace.rs:83
enum UserspaceTrapInfo {
    Syscall(SyscallRequest),
    PageFault(PageFaultInfo),
    TimerPreempt,
    Fatal(FatalTrapInfo),
}

match trap {
    UserspaceTrapInfo::Syscall(req) => {
        let result: SyscallResult = dispatch::<P>(req, &ctx).await;   // async; may block (§7)
        match result {
            NoReturn      => return,                              // exit/exit_group
            ExecCommitted => { /* new image; do NOT write a0 */ }
            other         => payload.store_pending_syscall_return(other.into()),
        }
    }
    UserspaceTrapInfo::PageFault(info) => {
        match aspace.fault_script(info.into()).await {            // async VM fault
            Ok(())  => { /* mapping published; re-run faulting insn; no a0 write */ }
            Err(_)  => { deliver_synchronous_fault(SIGSEGV); return; }
        }
    }
    UserspaceTrapInfo::TimerPreempt => { yield_now().await; }     // §8
    UserspaceTrapInfo::Fatal(_)     => { deliver_synchronous_fault(SIGSEGV); return; }
}
// fall through to Phase 1
```

The relevant result type:

```rust
// crates/tx-shims/src/linux_syscall/result.rs:22
enum SyscallResult {
    Return(i64),                                   // -> a0 = value
    CloneReturn { value: i64, child_submit: SubmitChildThreadStatus },
    Error(i32),                                    // -> a0 = -errno
    NoReturn,                                      // exit/exit_group: future returns
    ExecCommitted,                                 // execve replaced the image
    // ...
}
```

Note the asymmetry the writeback discipline (§5.6) exists to manage: the **syscall**
arm produces a value that must land in `a0` on the *next* entry; the **page-fault**
arm produces *no* `a0` write (the faulting instruction is simply re-executed);
`TimerPreempt` produces no architectural change at all. One loop, several distinct
"returns to user."

### 4.6 Thread creation at boot

At boot the init thread's future is wrapped in `PerHartSlotted` and submitted to the
reactor like any other task:

```rust
// crates/tx-kernel/src/init/exec.rs:647
let (task_key, _) = reactor.submit_task_with_meta_from_hart(
    PerHartSlotted::<P, _>::new(
        thread.clone(),
        payload.clone(),
        run_thread::<P>(thread, payload),    // the thread's entire life
    ),
    userspace_thread_sched_meta(),
    current_hart,
    &mut signal,
);
register_thread_reactor_task(tid, task_key);
P::enable_timer_wakeups();
```

From the reactor's perspective (§3) this is just another `Pin<Box<dyn Future>>` in
the task table. Threads created later by `clone` are submitted the same way through
the `reactor_submit` seam (`init/reactor_submit.rs:305`), which is how `tx-shims` — a
library that cannot depend on the kernel's reactor — gets a child thread onto the
run-queue (§7.5).

---

## 5. Trap Handling and the Synchronous/Asynchronous Handoff

This is the architectural crux. A hardware trap is unavoidably synchronous: the CPU
jumps to a fixed vector with no future, no `Context`, no `Waker`. Yet the thread is a
future that wishes to `.await` the trap. This section dissects the seam joining the
two worlds — the *trap handoff* — and shows that everything surrounding that seam is
identical to a conventional kernel.

### 5.1 The unchanged machinery

**The assembly trap vector** (RISC-V 64):

```
; boards/tx-hal-riscv64-qemu-virt/src/trap.rs:103 (tx_rv64_qemu_minimal_trap_vector)
swap sscratch <-> sp           ; switch to the per-CPU trap stack
save x1..x31 into trap frame   ; integer registers
read scause, sepc, stval, sstatus
(save f0..f31 + fcsr if FP dirty)
call tx_rv64_qemu_kernel_trap_entry(&mut frame)
```

**The trap frame** is the ordinary RISC-V layout:

```rust
// boards/tx-hal-riscv64-qemu-virt/src/trap.rs:530
struct Rv64TrapFrame {
    x:       [u64; 32],   // x0..x31
    scause:  u64,         // why we trapped
    sepc:    u64,         // PC at trap
    stval:   u64,         // fault address / aux value
    sstatus: u64,
    f:       [u64; 32],   // FP registers
    fcsr:    u64,
}
```

**Cause classification:**

```rust
// boards/tx-hal-riscv64-qemu-virt/src/trap.rs:927 (classify_rv64_trap)
match scause {
    8  => Syscall,            // ECALL from user
    12 => InstructionPageFault,
    13 => LoadPageFault,
    15 => StorePageFault,
    2  => IllegalInstruction,
    INT|5 => TimerInterrupt,
    INT|9 => ExternalInterrupt,
    INT|1 => SoftwareInterrupt,  // IPI
    _  => /* ... */
}
```

None of this is unusual. A conventional kernel has the same vector, the same frame,
the same decode. The difference lies entirely in *what the handler decides to do*.

### 5.2 The trap shell: decide, do not execute

The classified trap reaches a platform-independent dispatcher implementing
`KernelTrapSink`. Its handlers return a `TrapAction` — they decide a *policy*; they
do not run the syscall to completion:

```rust
// crates/tx-hal/src/trap.rs:304,311
enum TrapAction { Resume, Reschedule, DeliverSignal, Terminate }

trait KernelTrapSink<P> {
    fn on_page_fault(view: TrapFrameMut, fault: FaultInfo) -> TrapAction;
    fn on_syscall(view: TrapFrameMut) -> TrapAction;
    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut) -> TrapAction;
    fn on_external_irq(...) -> TrapAction;
    // ...
}
```

The four actions correspond to four fates for the trapped thread:

| `TrapAction` | Meaning |
|---|---|
| `Resume` | Return straight to userspace via `sret` (fast path, handled inline) |
| `Reschedule` | Longjmp back into the reactor; the thread future will service the trap |
| `DeliverSignal` | Return to user, but into a signal handler frame (§9) |
| `Terminate` | Fatal — no valid thread context to hand off to |

### 5.3 `on_syscall`: capture and resolve, then leave

```rust
// crates/tx-kernel/src/trap.rs:35
fn on_syscall(view: TrapFrameMut) -> TrapAction {
    let req = translate_syscall(&view);     // a7 -> nr, a0..a5 -> args

    // Fast path: a small allow-list handled synchronously, right here.
    if let Some(action) = try_direct_trap_syscall(&mut view, &req) {
        return action;                       // usually TrapAction::Resume
    }

    // Slow path: hand the syscall off to the thread future.
    let hart = current_cpu_id();
    let outcome = hand_off_syscall(hart, &view, req);
    outcome_to_trap_action(&outcome)         // Resolved -> Reschedule
}
```

`translate_syscall` builds the request type from §4:

```rust
// crates/tx-reactor/src/userspace.rs:37
struct SyscallRequest { nr: u64, args: [u64; 6] }
```

### 5.4 The handoff

`hand_off_syscall` is the seam. It runs in the synchronous trap context but performs
only three actions — none of which is "execute the syscall":

```rust
// crates/tx-kernel/src/trap_handoff.rs:202
fn hand_off_syscall(hart, view, req) -> HandoffOutcome {
    // 1. Find the thread running on this hart (the per-hart slot from §4).
    let payload = current_payload_for_hart(hart)?;       // None -> NoActivePayload
    let active  = payload.active_userspace_request()?;   // the Phase-1 wait token

    // 2. Snapshot user registers into the payload; advance PC past ecall.
    let mut ctx = view.capture_user_context();
    ctx.pc = ctx.pc + RV64_ECALL_INSN_BYTES;             // 4: don't re-run the ecall
    payload.store_saved_user_context(Some(ctx));

    // 3. Resolve the in-flight userspace-run wait with the trap info.
    let slot = payload.userspace_slot().clone();
    slot.complete_interesting_trap(active, UserspaceTrapInfo::Syscall(req))?;
    HandoffOutcome::Resolved
}

// crates/tx-kernel/src/trap_handoff.rs:145
enum HandoffOutcome {
    Resolved,          // wait resolved -> caller returns TrapAction::Reschedule
    NoActivePayload,   // trap from kernel code, or slot momentarily empty -> Terminate
    NoActiveRequest,   // payload present but no in-flight wait -> Terminate
    SlotError(_),
}
```

Step 2 is where "the registers" land in the `ThreadPayload` (§4). Step 3 calls
`complete_interesting_trap`, which flips the `UserspaceRunWait` to *resolved* and
fires its `Waker` (mechanics in §6). The essential fact: **the syscall has not run.**
All that has happened is that the trap was *recorded* as the resolution of a wait the
thread future is sitting on.

**Why advance PC here but not for a page fault.** `ctx.pc += 4` happens for syscalls
only — a syscall resumes *after* the `ecall`. A page fault must *re-execute* the
faulting instruction once the mapping exists, so `hand_off_user_pf`
(`trap_handoff.rs:279`) snapshots the context with PC **unchanged**. Same seam, one
detail different — and the reason the syscall arm and the page-fault arm of
`run_thread` (§4.5) write back differently.

### 5.5 The longjmp back into the reactor

`on_syscall` returned `Reschedule`, but the CPU is on the trap stack, deep inside the
divergent `enter_userspace_with_context` call the thread future made in §4.5 Phase 2.
Returning to the reactor's poll loop is a longjmp:

```
; boards/tx-hal-riscv64-qemu-virt/src/trap.rs  apply_trap_action(Reschedule):
re-prime sscratch = trap_stack_top
call tx_rv64_resume_kernel_after_reschedule(&KernelResumeCtx)   ; trap.rs:485
;   -> restore (sp, ra, s0..s11) from the snapshot, then `ret`
```

`KernelResumeCtx` is the kernel-side callee-saved register snapshot taken just before
the thread future dove into userspace. Restoring `sp`/`ra`/`s0..s11` and `ret`-ing
**unwinds the divergent call** — control pops back out of
`enter_userspace_with_context` as if it had returned normally, landing the thread
future immediately after its Phase 2 dive, at the `entry_wait.await` of Phase 3.

This is the futures equivalent of a context switch *into the scheduler*, except the
"scheduler" is the reactor poll loop simply continuing. The two worlds compared:

| Traditional | txKernel |
|---|---|
| Trap → handler runs syscall on the kernel stack | Trap → shell records the trap, longjmps to reactor |
| Handler blocks → `schedule()` switches stacks | Future `.await`s → reactor polls another task |
| Handler finishes → `sret` to user | Future loops → `enter_userspace_with_context` → `sret` |

### 5.6 The fast path: where the synchronous model wins

Not every syscall warrants a round-trip through the reactor. `getpid`,
`clock_gettime`, `rt_sigprocmask`, and a few others are pure, never block, and are
hot. `try_direct_trap_syscall` (`trap.rs:122`) handles those *synchronously in the
trap shell*, writes the result directly into the trap frame, and returns
`TrapAction::Resume` — a plain `sret`, with no handoff and no poll. Preconditions:
the syscall is on the allow-list, no signal is pending, and no interrupt summary is
set. The future model is therefore not dogma: a classical synchronous path is
retained where it is strictly faster, and the handoff is reserved for syscalls that
*might* block.

### 5.7 Plan B: the two-site writeback discipline

A question threads through §4–§5: *where does the syscall's return value get written
into `a0`?* The naive answer — "in the trap handler" — is wrong here, instructively
so. The discipline (named "Plan B" in the source) splits it into two sites:

1. **Trap shell (capture site).** `hand_off_syscall` snapshots the user context and
   resolves the wait. It does **not** write the return value — at this moment the
   value does not exist, the syscall not having run.

2. **Userspace-entry shim (the sole writeback site).**
   `prepare_userspace_entry_payload_into` (`execution.rs:775`), in Phase 2 of the
   *next* loop iteration, drains `pending_syscall_return` and folds it into `a0` of a
   fresh context just before `enter_userspace_with_context`:

```rust
// execution.rs:775 (pseudocode)
fn prepare_userspace_entry_payload_into(payload, out: &mut UserTrapContext) {
    let mut ctx = payload.saved_user_context().expect("captured at trap");
    if let Some(result) = payload.drain_pending_syscall_return() {
        ctx.regs[A0] = match result {       // A0 = regs[10]
            Ok(v)      => v as usize,
            Err(errno) => (-(errno as i64)) as usize,  // -errno convention
        };
    }
    payload.set_active_userspace_request(None);
    *out = ctx;
}
```

The rationale for two sites:

- Between capture (1) and writeback (2), the thread future runs
  `dispatch(req).await`, which may suspend and resume many times (§7). The original
  trap frame is long gone; the value must live in `pending_syscall_return` and be
  applied to a *fresh* context at re-entry.
- It keeps the result-write off the trap-restore path, so nothing tramples it.
- It composes cleanly with signal delivery, which may instead overwrite `a0` with a
  handler frame (§9); the page-fault and `ExecCommitted` arms skip the write
  entirely. That selectivity is expressible only because the write is one explicit
  step.

---

## 6. The Suspension Point: `UserspaceRunWait`

§5 ended with the trap shell calling `complete_interesting_trap`; §4 ended with the
thread future at `entry_wait.await`. This section examines the hinge between them — a
single, small wait object that is the entire interface between the synchronous trap
world and the asynchronous reactor. It is a **wait-queue with exactly one waiter**,
and understanding it reduces the architecture to something tractable.

### 6.1 The role

The round-trip from §4–§5:

```
run_thread Phase 1:  open a wait        -> UserspaceRunWait
run_thread Phase 2:  enter userspace    (divergent)
   ... user runs, then traps ...
trap shell:          resolve the wait    (complete_interesting_trap)
   ... longjmp back into the reactor ...
run_thread Phase 3:  entry_wait.await    -> Poll::Ready(UserspaceTrapInfo)
```

The wait carries back one of four trap outcomes (`UserspaceTrapInfo`, §4.5).

### 6.2 The data structures

The slot is a clone-shared cell. The thread future holds one handle (via
`ThreadPayload.userspace_slot`); the trap shell obtains another by cloning it off the
per-hart payload (§5):

```rust
// crates/tx-reactor/src/userspace.rs:190,197,203,208
struct UserspaceRunSlot { state: Arc<SpinLock<SlotState>> }

struct SlotState {
    next_request: u64,            // monotonic generation source
    active: Option<ActiveRun>,    // at most one in-flight wait
}

struct ActiveRun {
    request: UserspaceRunRequest, // generation-checked identity (u64 newtype)
    phase:   ActivePhase,
    dispatches:  u64,
    preemptions: u64,
    waker:   Option<Waker>,       // the parked thread future's Waker
}

enum ActivePhase {
    Pending,                      // wait opened, userspace not yet entered
    Running,                      // userspace dispatched, no trap yet
    Resolved(UserspaceTrapInfo),  // trap arrived; ready to hand back
}

struct UserspaceRunWait {         // the future the thread .awaits
    slot:     UserspaceRunSlot,
    request:  UserspaceRunRequest, // which generation this wait belongs to
    finished: bool,
}
```

`ActivePhase` is `TASK_RUNNING` vs. `TASK_INTERRUPTIBLE` vs. "wake condition
satisfied," specialized to one waiter. The `request` generation is the ABA guard: a
stale `complete_interesting_trap` from a previous round-trip is rejected rather than
resolving the wrong wait.

### 6.3 Opening the wait

```rust
// UserspaceRunSlot::start_request  (userspace.rs:234)
fn start_request(&self) -> Result<UserspaceRunWait, UserspaceRunError> {
    let mut state = self.state.lock();
    if state.active.is_some() { return Err(Busy(...)); }   // one in-flight wait per thread
    let request = UserspaceRunRequest(state.next_request);
    state.next_request += 1;                               // new generation
    state.active = Some(ActiveRun {
        request, phase: ActivePhase::Pending,
        dispatches: 0, preemptions: 0, waker: None,
    });
    Ok(UserspaceRunWait { slot: self.clone(), request, finished: false })
}
```

This is Phase 1 of `run_thread`. The thread stores `request` in
`payload.active_request` so the trap shell can name this exact wait later.

### 6.4 Resolving (trap shell) and awaiting (thread future)

The trap-shell side sets the condition and wakes the sleeper:

```rust
// UserspaceRunSlot::complete_interesting_trap  (userspace.rs:341)
fn complete_interesting_trap(&self, request, trap) -> Result<UserspaceRunStatus, _> {
    let (status, waker) = {
        let mut state = self.state.lock();
        let active = state.active.as_mut().ok_or(NoActiveRequest)?;
        if active.request != request {
            return Err(StaleRequest { attempted: request, active: active.request });
        }
        // (a pending TimerPreempt resolution can be upgraded to a real trap here)
        active.phase = ActivePhase::Resolved(trap);
        (active.status(), active.waker.take())
    };
    if let Some(waker) = waker { waker.wake(); }   // THE wake_up()
    Ok(status)
}
```

The thread-future side reports ready or stashes its waker:

```rust
// impl Future for UserspaceRunWait  (userspace.rs:490)
fn poll(self, cx) -> Poll<UserspaceTrapInfo> {
    if self.finished { return Poll::Pending; }       // one-shot
    let mut state = self.slot.state.lock();
    let active = match state.active.as_mut() { Some(a) => a, None => return Pending };
    if active.request != self.request { return Poll::Pending; }   // not ours

    match active.phase {
        ActivePhase::Resolved(trap) => {
            state.active = None;           // consume the slot
            self.finished = true;
            Poll::Ready(trap)              // hand the trap to run_thread
        }
        ActivePhase::Pending | ActivePhase::Running => {
            if active.waker.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                active.waker = Some(cx.waker().clone());   // idempotent stash
            }
            Poll::Pending                  // suspend the thread future
        }
    }
}
```

This is exactly §2's pattern: *ready → return the value; not ready → stash the waker,
return Pending.* The waker stashed is the one the reactor built for this task (§3,
step 4). When the trap shell calls `waker.wake()`, the task is re-queued, the reactor
re-polls `run_thread`, and this `poll` runs again — now finding `Resolved` and
returning the trap. A `Drop` impl (`userspace.rs:535`) clears `state.active` if a
wait is dropped unfinished, so an aborted thread cannot leave a stuck slot.

### 6.5 Two timelines: hardware versus host test

The most clarifying way to understand the suspension point is that the two
environments resolve the wait at *different times* relative to the await.

**On real hardware — the wait is already resolved.** The divergent userspace dive and
the longjmp-back occur *inside* a single reactor poll of the task:

```
poll #N of the task:
  Phase 1: start_request -> Pending
  Phase 2: enter_userspace_with_context   (divergent; control leaves)
       userspace executes ... ecall ...
       trap vector -> on_syscall -> hand_off_syscall:
            complete_interesting_trap -> phase=Resolved, waker.wake()
       apply_trap_action(Reschedule) -> longjmp back into the reactor
  Phase 3: entry_wait.await -> poll sees Resolved -> Poll::Ready(trap)
  Phase 4: dispatch the trap
```

All of it happens within one poll; by the time `.await` re-checks the slot it is
already `Resolved`. The `waker.wake()` is effectively redundant on this hot path (the
poll never suspended), but is essential for correctness under reordering and is what
makes the host path work.

**On the host test platform — the await genuinely suspends.** There is no real trap;
`enter_userspace_with_context` is a stub that records the entry and returns:

```
poll #1: Phase 1 start_request -> Pending
         Phase 2 enter (stub returns)
         Phase 3 entry_wait.await -> poll sees Pending -> the task suspends here

test driver: slot.complete_interesting_trap(req, Syscall(...))
             -> phase=Resolved, waker.wake()  (re-queues the task)

poll #2: Phase 3 entry_wait.await -> poll sees Resolved -> Poll::Ready(Syscall(req))
         Phase 4 dispatch
```

The state machine is now observable one poll at a time, which the tests exploit
(`crates/tx-kernel/src/init/tests.rs:884`):

```rust
let future = run_thread::<TestPlatform>(leader.clone(), payload.clone());
assert!(poll_once(&mut future).is_pending());      // parked at entry_wait.await
payload.userspace_slot().complete_interesting_trap(token, Syscall(req));
// ... drives dispatch, asserts exactly the expected re-entries ...
```

The timer-preempt variant (`tests.rs:1014`) checks that a `TimerPreempt` resolution
makes the thread `yield_now().await` and re-enter from the same saved context without
resolving a new wait — i.e. preemption costs one extra poll and zero architectural
change. The `thread_future/tests.rs` suite pins the same behaviors against the
in-tree test platform.

The lesson is that the host/hardware asymmetry is not a workaround but the *proof*
that the trap round-trip is modeled as a wait: on hardware the wait resolves
synchronously within one poll; on the host it resolves across two. The future code is
identical either way, which is precisely the intent of
`txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`.

---

## 7. Blocking System Calls as Futures

In a conventional kernel, `read()` on an empty pipe does:

```
sys_read():
    if no data:
        add current thread to pipe's wait-queue
        set state TASK_INTERRUPTIBLE
        schedule()        # the kernel stack is now parked HERE
    # ... woken later, resumes here, re-checks, copies data ...
```

The kernel stack is pinned at `schedule()` until data arrives; a thousand blocked
readers means a thousand parked stacks. This section presents the futures
replacement: the blocked `read` is a `Future` returning `Poll::Pending`, parked in
the task table at the cost of one waker registration and no stack, leaving the hart
free to poll anything else.

### 7.1 Position in the round-trip

From §4.5 Phase 4, the thread future has received `UserspaceTrapInfo::Syscall(req)`
and dispatches it:

```rust
let result: SyscallResult = dispatch::<P>(req, &ctx).await;   // async!
```

`dispatch` is an `async fn`. This `.await` is the *second-level* suspension — not the
trap wait of §6, but the syscall's own internal wait. When the syscall cannot complete
immediately, this await returns `Pending`, which propagates up through `run_thread`
and `PerHartSlotted` to the reactor, parking the *entire thread task*. That is the
whole mechanism: a blocking syscall is a future, and awaiting it parks the thread the
same way any `Pending` does.

### 7.2 The syscall context

Every async syscall handler receives a `SyscallCtx` carrying the handles a blocking
syscall needs to park and be woken:

```rust
// crates/tx-shims/src/linux_syscall/ctx.rs:19
struct SyscallCtx<'a> {
    process: Cap<ProcessIdentity>,
    thread:  Cap<ThreadIdentity>,
    aspace:  Cap<AddressSpace>,
    mailbox: Option<Arc<TaskMailbox>>,                 // park/wake channel
    timer_wheel: Option<TimerWheel>,                   // deadline wakes (timeouts)
    delegate_registry: Option<Arc<DelegateRegistry>>,  // off-task replies
    // credential snapshot, etc.
}
```

The `mailbox` is the one bound to this thread's reactor task in `PerHartSlotted`
(§4.4). A wake posted to the mailbox fires the task's `Waker`, re-polling
`run_thread`, which re-polls the suspended `dispatch` future, which resumes the
syscall. The loop closes.

### 7.3 The wait machinery

The general blocking primitive — the futures equivalent of a wait-queue plus
`wake_up()` — comprises three types:

```rust
// crates/tx-substrate/src/wake/wait_source.rs:83
struct WaitSource {                 // == a wait-queue / readiness object
    id: WaitSourceId,
    subscribers: SpinMutex<Vec<Subscriber>>,   // who is waiting
    next_subscriber_id: AtomicU64,
    pending_mask: AtomicU64,                    // edge-coalescing
}

// crates/tx-substrate/src/wake/mailbox.rs:258
struct TaskMailbox { /* event queue + the task's Waker + generation counter */ }

// crates/tx-substrate/src/wake/mailbox.rs:517
struct ActiveWait {                 // == this waiter's wait-queue entry
    generation: WaitGeneration,     // ABA guard for lost/stale wakes
    source:     WaitSourceId,
    interests:  InterestMask,        // which events this waiter cares about
}
```

Events flow through the mailbox:

```rust
// crates/tx-substrate/src/wake/mailbox.rs:127
enum MailboxEvent {
    SourceFired { generation: WaitGeneration, source: WaitSourceId, interests: InterestMask },
    SignalDelivered { /* ... */ },   // a wake-hint; truth lives in InterruptSummary (§9)
    // AgentReplied / Abort (off-task delegate replies)
}
```

The generic blocking poll (the shape `await_wait_source` uses):

```rust
fn poll(self, cx) -> Poll<()> {
    self.mailbox.register_waker(cx.waker().clone());   // 1. arm THIS task's Waker
    while let Some(event) = self.mailbox.poll() {       // 2. drain events
        if self.active.matches(&event) {                //    gen + source + mask overlap
            self.mailbox.clear_waker();
            return Poll::Ready(());                     //    condition satisfied
        }
        if matches!(event, MailboxEvent::SignalDelivered { .. }) {
            self.mailbox.clear_waker();
            return Poll::Ready(());                     //    interrupted -> caller maps to EINTR
        }
    }
    Poll::Pending                                        // 3. stay parked
}
```

The correspondence to the traditional model is exact:

| Traditional | Futures |
|---|---|
| Wait-queue object | `WaitSource` |
| `current`'s wait-queue entry | `ActiveWait` (+ a `Subscriber` row) |
| `add_wait_queue` + `TASK_INTERRUPTIBLE` | `WaitSource::register` + `register_waker` + `Pending` |
| `wake_up(&wq)` | post `SourceFired` to subscribers' mailboxes → `Waker::wake()` |
| `signal_pending()` check before sleeping | `SignalDelivered` resolves the wait → `EINTR` |
| ABA / lost-wakeup guards | `WaitGeneration` matched in `ActiveWait::matches` |

The generation guard merits note: a waiter captures `mailbox.next_generation()` at
registration and embeds it in its `ActiveWait`; a `SourceFired` event carries the
generation captured *when queued*; `matches` requires equality. A wake intended for a
previous wait on the same source is therefore dropped, closing the classic
lost-wakeup race without disabling interrupts.

### 7.4 Three concrete handlers

**`nanosleep` — wait on a deadline:**

```rust
// crates/tx-shims/src/linux_syscall/time.rs:825
async fn sys_nanosleep<P: TimeIf>(args, ctx) -> SyscallResult {
    let req_ns = read_timespec_ns(ctx.aspace, args[0])?;
    if req_ns == 0 { return SyscallResult::Return(0); }
    let deadline = P::read_ns() + req_ns;
    match sleep_until_deadline::<P>(deadline, req_ns, ctx).await {  // parks here
        Error(EINTR) => { write_remaining_timespec(ctx.aspace, args[1], ...); Error(EINTR) }
        other        => other,
    }
}
```

`sleep_until_deadline` registers with `ctx.timer_wheel`; when the reactor's timer
queue (§3.7) crosses the deadline it posts to the mailbox and wakes the task. This is
`schedule_timeout()` without the parked stack.

**`futex` wait — wait on a hashed bucket:**

```rust
// crates/tx-shims/src/linux_syscall/vm.rs:1192
async fn sys_futex<P: TimeIf>(args, ctx) -> SyscallResult {
    let uaddr = args[0]; let op = args[1] as u32; let val = args[2] as u32;
    let mut script = build_subject_script_ctx(ctx);
    if let Some(d) = deadline_ns { script = script.with_deadline(Deadline::from_raw(d)); }
    match drive(FutexWaitOp { uaddr, val, .. }, &mut script,
                DriveMode::Waiting, ctx.mailbox.as_ref(), ...).await {
        Ok(())     => SyscallResult::Return(0),
        Err(EINTR) => SyscallResult::Error(EINTR),
        Err(e)     => SyscallResult::error_from(e),
    }
}
```

The futex bucket is a `WaitSource`; a `FUTEX_WAKE` from another thread calls
`step_futex_wake`, firing that source. This is the classical hashed-wait-queue futex
design, with the bucket a `WaitSource` and the sleepers parked futures.

**`readv` — compose smaller awaits:**

```rust
// crates/tx-shims/src/linux_syscall/io.rs:875
async fn sys_readv<P: TimeIf>(args, ctx) -> SyscallResult {
    let mut total = 0;
    for iov in read_iovecs(ctx.aspace, args[1], args[2])? {
        match sys_read::<P>([args[0], iov.base, iov.len, 0,0,0], ctx).await {  // may park
            Return(n) => { total += n; if n < iov.len { break; } }
            Error(e)  => return if total > 0 { Return(total) } else { Error(e) },
            other     => return other,
        }
    }
    SyscallResult::Return(total)
}
```

This is the compositional payoff of `async`: `readv` is *written* as a sequential
loop over `sys_read`, yet each `sys_read` may independently park and resume. In the
traditional model one would hand-roll an explicit state machine to remember which
iovec was in progress across a sleep; here the compiler generates it (§2.3).

### 7.5 The cost of "parked"

When `dispatch(req).await` returns `Pending`, the precise system state is:

- the thread's `run_thread` future is suspended at the `dispatch(...).await`, its
  state machine (which iovec, which deadline) resident in the boxed task;
- the thread's registers live in `payload.saved_user_context` (captured at the trap);
- the task is `Parked` in the task table, on **no** run-queue;
- one `ActiveWait` row sits in one `WaitSource`'s subscriber list, holding a `Weak`
  reference to the mailbox;
- the hart immediately picks the next runnable task.

No kernel stack is reserved. Ten thousand blocked readers are ten thousand parked
futures plus ten thousand subscriber rows — heap, not stacks. This is the resource
story the model exists to deliver.

### 7.6 The `reactor_submit` seam

A structural subtlety: `tx-shims` (where these handlers live) **cannot depend on** the
kernel's reactor crate — that would be a dependency cycle. So when a syscall such as
`clone` needs to place a *new* thread on the run-queue, it goes through a
function-pointer seam installed at boot:

```rust
// crates/tx-subsystems/src/reactor_submit/mod.rs
type SubmitChildThreadFn = fn(Cap<ProcessIdentity>, Cap<ThreadIdentity>) -> SubmitChildThreadStatus;
static SUBMIT_CHILD_THREAD_FN: AtomicPtr<()> = AtomicPtr::new(null_mut());

fn install_submit_child_thread(f: SubmitChildThreadFn);   // kernel calls at boot (exec.rs:533)
fn submit_child_thread(p, t) -> SubmitChildThreadStatus;  // shims calls from sys_clone
```

The kernel installs a closure (`init/reactor_submit.rs:305`) that wraps the child
thread in `PerHartSlotted` and submits it, so the child becomes an ordinary task
(§4). The `SyscallResult::CloneReturn { child_submit, .. }` variant (§4.5) carries the
submission status back to the parent thread future.

---

## 8. Interrupts and Wakes

§7 left a parked `read` future waiting on a `WaitSource`, raising the question of who
makes the condition true and fires the waker. For I/O the answer is a device
interrupt. This section follows a hardware IRQ from the trap vector to
`Waker::wake()`, then treats the special case of the timer interrupt — the mechanism
that lets a cooperative poll loop nonetheless preempt a CPU-bound userspace thread.

### 8.1 Two kinds of interrupt, two jobs

- A **device IRQ** (UART, virtio) signals an external event; its job is to make some
  `WaitSource` ready and wake whichever task was parked on it.
- A **timer IRQ** signals an elapsed slice; its job is to force the currently-running
  userspace thread to yield the hart.

Both arrive through the same asm vector and `classify_rv64_trap` (§5.1) but take
different `TrapAction` paths.

### 8.2 Device IRQ: from wire to `wake()`

```rust
// KernelTrapSink::on_external_irq  (trap.rs:67, pseudocode)
fn on_external_irq(...) -> TrapAction {
    let irq = P::claim();                       // PLIC claim
    let handled = P::dispatch_irq(irq);         // run the registered handler
    P::complete(irq);                           // PLIC complete (EOI)
    match handled {
        IrqHandled::Wake    => TrapAction::Reschedule,  // a task may now be runnable
        IrqHandled::Done    => TrapAction::Resume,
        IrqHandled::NotMine => TrapAction::Resume,
    }
}

// crates/tx-hal/src/lib.rs:1180
enum IrqHandled { Done, Wake, NotMine }
```

Handlers are registered in a dispatch table at boot (`irq.rs:45,85,159`).

**The IRQ-context restriction.** An IRQ handler runs in interrupt context, where in
this kernel it **cannot create an epoch (EBR) guard** — and therefore cannot touch
capability-managed structures such as the TTY line discipline. This forces a
two-phase design that makes the "interrupt context vs. process context" boundary
concrete.

*Phase A — in IRQ context, minimal work:*

```rust
// crates/tx-kernel/src/irq.rs:182
fn uart_rx_irq_handler<P: ConsoleIf>(_irq: u32) -> IrqHandled {
    let mut buf = [0u8; UART_RX_DRAIN_MAX];
    let n = P::read_bytes(&mut buf);            // drain the UART FIFO
    if n == 0 { return IrqHandled::Done; }      // spurious
    if console_tty().is_none() { return IrqHandled::NotMine; }  // pre-boot race
    UART_RX_PENDING.lock().push(&buf[..n]);     // plain ring buffer; no epoch guard
    IrqHandled::Wake                            // request a reschedule
}
```

*Phase B — back in the reactor (process context), the real work:*

```rust
// crates/tx-kernel/src/irq.rs:222 — called by the reactor loop after each poll
fn drain_uart_rx_pending() -> usize {
    let (bytes, n) = UART_RX_PENDING.lock().take_snapshot();   // drain + clear
    if n == 0 { return 0; }
    let tty = console_tty()?;
    let guard = guard();                        // now legal: process context
    tty.step_ingest(&bytes, &guard);            // push into the line discipline
    //   step_ingest fires the TTY's WaitSource and wakes its subscribers
    n
}
```

`step_ingest` fires the `WaitSource` the parked `read` subscribed to in §7, posting
`SourceFired` to the reader's mailbox and invoking its `Waker::wake()`. The complete
chain:

```
UART RX asserts IRQ
  -> asm vector -> classify -> on_external_irq
       -> uart_rx_irq_handler: FIFO -> UART_RX_PENDING ring, return Wake
  -> TrapAction::Reschedule (longjmp into the reactor)
reactor loop: poll a task ... then drain_uart_rx_pending()
  -> tty.step_ingest -> TTY WaitSource fires
       -> post SourceFired to the reader's TaskMailbox
       -> TaskWakeState::wake(): push TaskId onto the global wake queue  (waker.rs:31)
next loop iteration: drain_wakes_for_hart  (runtime.rs:704)
  -> reader task Parked -> Runnable -> enqueued
  -> reactor polls run_thread -> dispatch(read).await resumes -> copies data
  -> SyscallResult::Return(n) -> pending_syscall_return -> a0 on re-entry
```

Every step after `step_ingest` is the machinery of §3, §6, and §7. The IRQ's sole
novel contribution is *bridging interrupt context to process context* via the
`UART_RX_PENDING` ring and the `Wake` request.

### 8.3 Timer IRQ: preempting a cooperative loop

§3.6 noted the tension: `poll` is cooperative, so the reactor cannot interrupt a
future mid-poll, and a userspace thread in a tight loop never returns to the reactor
on its own. The resolution: the timer interrupt does not fire *during a poll* — it
fires while the thread is **in userspace**, between `enter_userspace_with_context` and
the next trap. At that instant the kernel is not polling anything; it is suspended
inside the divergent userspace dive. The timer trap is the kernel regaining control.

```rust
// KernelTrapSink::on_timer_interrupt  (trap.rs:52, pseudocode)
fn on_timer_interrupt(cpu, view) -> TrapAction {
    P::cancel_deadline();
    if view.previous_mode == User {
        let outcome = hand_off_timer_preempt(hart, &view);   // trap_handoff.rs:356
        if matches!(outcome, Preempted) { mark_boot_reactor_userspace_preempt(cpu); }
        timer_preempt_outcome_to_trap_action(&outcome)       // Preempted -> Reschedule
    } else {
        // timer fired in kernel context: account and Resume
    }
}
```

The handoff mirrors the syscall one (§5.4) but resolves the wait with a different
trap info:

```rust
// crates/tx-kernel/src/trap_handoff.rs:356
fn hand_off_timer_preempt(hart, view) -> TimerPreemptOutcome {
    let payload = current_payload_for_hart(hart)?;
    let active  = payload.active_userspace_request()?;
    payload.store_saved_user_context(Some(view.capture_user_context())); // PC UNCHANGED
    payload.userspace_slot().clone()
        .record_timer_preemption(active)   // -> ActivePhase::Resolved(TimerPreempt) + wake
        .map(|_| TimerPreemptOutcome::Preempted)
}
```

Two distinctions from a syscall handoff: (1) **PC is captured unchanged** — a
preempted thread resumes the exact instruction it was on, with nothing to skip
(contrast the `+4` past `ecall` in §5.4); (2) the wait resolves with
`UserspaceTrapInfo::TimerPreempt` via `record_timer_preemption` (`userspace.rs:300`),
firing the waker.

Back in `run_thread`, the `TimerPreempt` arm (§4.5) performs the cooperative yield:

```rust
UserspaceTrapInfo::TimerPreempt => { yield_now().await; }
// loop back to Phase 1: re-enter userspace from the unchanged saved context
```

`yield_now()` is a future that returns `Pending` exactly once (re-queuing itself
runnable), then `Ready`. The thread thus releases the hart for one scheduling
decision and re-enters userspace where it was. A CPU-bound thread is forced through
the reactor's pick-next logic on every timer tick — the outcome of a traditional
preemptive tick, achieved without ever interrupting a `poll`.

> **Implementation note.** Because timer preemption resolves the wait, a *real* trap
> (syscall/fault) arriving in the same window can be upgraded over a pending
> `TimerPreempt` resolution rather than lost — `complete_interesting_trap` handles
> that replace case (`userspace.rs:341`), with the generation check preventing it
> from crossing round-trips. (The source comment on `hand_off_timer_preempt`
> describing preemption as "not resolving the wait" predates the current
> `record_timer_preemption` path, which does resolve it with `TimerPreempt`.)

### 8.4 The unified wake plumbing

Both interrupt kinds feed the same two-stage wake from §3:

```rust
// stage 1: anyone requests a re-poll
TaskWakeState::wake():                       // waker.rs:31
    wake_requested = true
    global_wake_queue.push(task_id)

// stage 2: the reactor loop, next iteration
drain_wakes_for_hart(hart):                  // runtime.rs:704
    for task_id in drain(global_wake_queue):
        task: Parked -> Runnable
        scheduler.enqueue(task_id)           // routed by WakeHint to a run-queue
```

Device IRQs reach stage 1 via `step_ingest` → `WaitSource` → mailbox →
`Waker::wake()`; the timer reaches it via `record_timer_preemption` → the thread's own
`UserspaceRunWait` waker. Either way, the parked thread becomes runnable and the loop
polls it.

---

## 9. Signals as Wait Interruption

Signals touch the future model in two places: they must **interrupt** a thread parked
in a blocking syscall (the source of `EINTR`), and they must be **delivered** —
turned into a handler frame on the user stack — at a safe point. Both map onto the
traditional "`signal_pending()` in interruptible sleep" plus "deliver signals on
return to user."

### 9.1 The traditional model, briefly

```
# while sleeping interruptibly:
if signal_pending(current) and state == TASK_INTERRUPTIBLE:
    remove from wait-queue; return -ERESTARTSYS / -EINTR
# on the path back to userspace:
if signal_pending(current):
    setup_signal_frame()   # push handler context onto user stack, redirect PC
```

Two jobs, two boundaries. The future model keeps both.

### 9.2 Signal state in the `ThreadPayload`

```rust
// crates/tx-subsystems/src/thread_runtime/structure.rs:140 (signal subset)
struct ThreadPayload {
    signal_mask:    AtomicU64,          // blocked set (sigprocmask)
    thread_pending: PendingSignalQueue, // per-thread pending bitset
    group_pending_summary: AtomicU64,   // conservative process-group hint
    signal_summary: AtomicU8,           // InterruptSummary, packed (the fast check)

    saved_signal_context: SpinMutex<Option<UserTrapContext>>, // pre-handler registers
    saved_signal_mask:    SpinMutex<Option<SignalMask>>,      // pre-handler mask
    alt_stack:            SpinMutex<Option<(usize, usize)>>,  // sigaltstack
    mailbox: SpinMutex<Option<Weak<TaskMailbox>>>,            // wake channel
    // ...
}
```

The hot field is `signal_summary`, an `InterruptSummary` packed into a byte:

```rust
// crates/tx-subsystems/src/signal/mod.rs:442
struct InterruptSummary {
    deliverable_signal: bool,   // a signal is pending and unblocked
    termination: bool,          // SIGKILL / fatal already escalated
    stop_requested: bool,       // SIGSTOP-family wants us parked
}
```

A parked future can answer "must I wake for a signal?" by reading one atomic byte —
no lock, no scan. This is the futures `signal_pending()`.

### 9.3 Interruption: the lost-wake-safe path

The hard part of signal interruption is the **lost wakeup**: a signal posted in the
window between "check condition" and "park" must not be missed. The traditional
solution is the wait-queue lock plus `set_current_state` ordering. The future model
solves it through the mailbox.

When a signal is sent (`step_kill_process`, `signal/mod.rs:1247`), the routing path
performs **two** actions per affected thread:

```rust
// signal/mod.rs:739,1344 (pseudocode)
// 1. Update the authoritative state: set pending bit + refresh signal_summary.
thread_payload.signal_summary.set(deliverable_signal = true);
// 2. Post a wake-hint to the thread's mailbox.
post_signal_mailbox(&thread_payload, sig, SignalRouting::ProcessDirected);
```

`post_signal_mailbox` (`thread_runtime/execution.rs:165`) posts a
`MailboxEvent::SignalDelivered` and fires the registered `Waker`:

```rust
// crates/tx-substrate/src/wake/mailbox.rs:187
enum MailboxEvent {
    SourceFired { generation, source, interests },
    SignalDelivered { signum: u32, routing: SignalRouting },   // the wake-hint
    // ...
}
```

The decisive design choice: **`SignalDelivered` does not "match" the wait.**
`ActiveWait::matches` (§7.3) returns `false` for it, so the event does not satisfy the
wait's *condition*. Instead it forces a **re-poll**, and on re-poll the future
consults the authoritative `signal_summary` (via `WaitProtocol::classify_interrupt`)
to decide whether it was interrupted. As the source states: the `SignalDelivered`
event is a wake-hint; the truth lives in `InterruptSummary`.

This split is what makes it lost-wake-safe:

- The **summary bit** is the durable truth. If set before the future's next poll, the
  future observes it, whenever the post happened.
- The **mailbox post + `wake()`** is only a nudge to *cause* that next poll. If it
  races early, the re-poll still reads the summary correctly; if it arrives late, the
  summary was already set, so a subsequent poll catches it.

Waits classify themselves by protocol:

```rust
// crates/tx-substrate/src/step/wait_protocol.rs:15,57
enum WaitProtocol { Uninterruptible, Interruptible, Killable }
enum WaitOutcome  { Ready, Interrupted, Killed, TimedOut }
```

An `Interruptible` wait (§7) resolves as `WaitOutcome::Interrupted` when a signal's
summary bit is set, and the handler maps that to `SyscallResult::Error(EINTR)`. The
reactor `wait.rs` variant adds timeout flavors (`InterruptibleTimeout(deadline)`,
`KillableTimeout(deadline)`).

**The regression test.** `crates/tx-subsystems/tests/v3_signal_interrupt_wake.rs`
pins exactly this. Paraphrasing its own doc comment: (1) bind a `TaskMailbox` to a
one-thread process's payload; (2) submit a task awaiting
`Channel::wait_event(..., Interruptible, || false)` — condition *always false*, so the
only exit is interruption, and the channel is **never fired**; (3) run the reactor
once → the task parks; (4) `step_kill_process(proc, SIGTERM)` → sets
`summary.deliverable_signal` *and* posts `SignalDelivered`, waking the task; (5) run
again → re-poll → `classify_interrupt` sees the summary → the future resolves
`WaitOutcome::Interrupted`. Asserted invariants: the outcome is `Interrupted` (not
`Ready`, since the condition was always false), it happens in ≤2 reactor ticks, and
the channel was never fired — the only wake path was the mailbox. That test is the
executable proof of lost-wake safety in the future model.

### 9.4 Delivery: the AST checkpoint

Interruption gets the thread *out* of its wait and back into the `run_thread` loop;
actual delivery — building a handler frame — happens at a controlled boundary: Phase 1
of the loop, before re-entering userspace (§4.5). This is the "deliver on return to
user" boundary, now an explicit checkpoint.

```rust
// run_thread Phase 1, expanded
ast_checkpoint(&payload):
    match ast_check(&thread) {                       // signal/mod.rs:836
        AstOutcome::Continue => { /* nothing to do; enter userspace */ }
        AstOutcome::DeliverHandler { sig, action } => {
            payload.store_saved_signal_context(payload.saved_user_context());
            payload.store_saved_signal_mask(current_mask());
            build_signal_frame(&payload, sig, action);   // push onto (alt) stack, redirect PC
        }
        AstOutcome::DefaultTerminate { sig } => { /* exit process group */ }
        AstOutcome::DefaultStop { sig }      => { payload.set_stopped(true); }
        AstOutcome::DefaultContinue { sig }  => { /* clear stop */ }
        AstOutcome::InitiateTermination      => { /* SIGKILL: exit, never enter user */ }
    }

// crates/tx-subsystems/src/signal/mod.rs:808
enum AstOutcome {
    InitiateTermination,
    DefaultTerminate { sig: Signum },
    DefaultStop      { sig: Signum },
    DefaultContinue  { sig: Signum },
    DeliverHandler   { sig: Signum, action: SigActionEntry },
}
```

`ast_check` (`signal/mod.rs:836`) reads the summary, selects the lowest-numbered
deliverable signal, consults the disposition table, and returns the intent. The
`DeliverHandler` path saves the pre-handler `UserTrapContext` and mask into
`saved_signal_context` / `saved_signal_mask`, then constructs the handler frame so the
next userspace entry runs the handler rather than resuming the interrupted point.

**`rt_sigreturn`** is the inverse: when the user handler finishes it calls
`sigreturn`, whose syscall arm drains `saved_signal_context` / `saved_signal_mask`
back into `saved_user_context` and the mask, so the *next* userspace entry resumes the
original execution. In the future model this is just another Phase-4 syscall arm
writing the payload — no special machinery beyond the saved-context slots.

### 9.5 The two boundaries, mapped

| Job | Traditional boundary | Future-model boundary |
|---|---|---|
| Wake an interruptible sleeper | `wake_up` + `signal_pending` in `schedule()` | `SignalDelivered` post + `Waker::wake()`; re-poll reads `signal_summary` → `Interrupted` → `EINTR` |
| Deliver (build handler frame) | `do_signal()` on return-to-user | `ast_check` in `run_thread` Phase 1, before `enter_userspace_with_context` |
| Return from handler | `sys_rt_sigreturn` restores sigframe | `sys_rt_sigreturn` arm drains `saved_signal_context` into the payload |
| `SIGSTOP` / `SIGCONT` | `TASK_STOPPED` state | `payload.stopped` flag checked at Phase 1 (§4.5) |

The boundaries are the same two a traditional kernel uses; the difference is that each
is now an explicit, testable step in an `async` state machine rather than an implicit
consequence of where the kernel stack happened to be.

---

## 10. End-to-End Case Study: `read()` on an Empty Pipe

This case study traces one operation — a reader blocking on an empty pipe, then woken
by a writer — through every layer the report has built. Two threads are involved:
**R** (reader) and **W** (writer), each a `run_thread` future, both ordinary tasks in
the reactor table.

**Cast.** Reader task **R** (`PerHartSlotted<run_thread>`, payload `P_R`); writer
task **W** (payload `P_W`); the pipe's read end with a readiness `WaitSource` **S** and
a byte buffer; a single-hart reactor (for clarity).

### 10.1 Act 1 — R issues the syscall

```
[R in userspace]  read(fd, buf, 256)  ->  ecall

asm vector (boards/.../trap.rs:103)
  save x1..x31 + CSRs into Rv64TrapFrame, classify -> Syscall(8)

on_syscall (trap.rs:35)
  req = translate_syscall: nr=READ, args=[fd, buf, 256, ...]
  try_direct_trap_syscall -> None    (read is not on the fast-path allow-list)
  hand_off_syscall(hart, view, req)                          (trap_handoff.rs:202)
    payload P_R = current_payload_for_hart(hart)             (the per-hart slot)
    ctx = view.capture_user_context();  ctx.pc += 4          (skip the ecall)
    P_R.store_saved_user_context(ctx)                        (R's registers live here now)
    P_R.userspace_slot.complete_interesting_trap(tok, Syscall(req))
        -> ActivePhase::Resolved(Syscall(req));  waker.wake()
  -> HandoffOutcome::Resolved -> TrapAction::Reschedule

apply_trap_action(Reschedule)
  tx_rv64_resume_kernel_after_reschedule(&KernelResumeCtx)   (trap.rs:485)
  -> restore sp/ra/s0..s11, ret -> unwinds enter_userspace_with_context
  -> control re-enters run_thread(R) just after its Phase 2 dive
```

The syscall has **not run**: R's trap became the resolution of a wait (§5, §6).

### 10.2 Act 2 — R's future dispatches, then parks

```
run_thread(R) Phase 3:  entry_wait.await
  UserspaceRunWait::poll -> Resolved(Syscall(req)) -> Poll::Ready(Syscall(req))   (userspace.rs:490)

run_thread(R) Phase 4:  syscall arm
  result = dispatch::<P>(req, &ctx_R).await
    sys_read: pipe buffer EMPTY
      register ActiveWait{gen, source=S, interest=READABLE} on WaitSource S   (wait_source.rs:118)
      mailbox_R.register_waker(cx.waker())                                    (mailbox.rs:352)
      no data, no signal -> Poll::Pending
  -> dispatch(...).await is Pending -> run_thread(R) returns Pending
  -> PerHartSlotted clears the per-hart slot, returns Pending

reactor finish_polled_pending(R) -> Parked
```

State of the world:

| | |
|---|---|
| Task R | `Parked` — on no run-queue |
| R's registers | in `P_R.saved_user_context` (heap) |
| R's resume point | the `dispatch(...).await` inside R's boxed state machine |
| R's wait entry | one `Subscriber` row in `WaitSource S`, holding `Weak<mailbox_R>` |
| Kernel stacks held by R | **zero** |

The hart proceeds; `pick_next_or_steal_local` returns task **W**. No stack was parked
— the §7 payoff, concretely.

### 10.3 Act 3 — W writes and fires the source

```
run_thread(W) Phase 4:  sys_write
  copy bytes into the pipe buffer
  pipe now READABLE -> fire WaitSource S
     for each Subscriber whose interest overlaps READABLE and gen matches:
        post MailboxEvent::SourceFired{gen, S, READABLE} to mailbox_R
        mailbox_R's Waker -> TaskWakeState::wake():                 (waker.rs:31)
              wake_requested = true;  global_wake_queue.push(R)
  sys_write -> SyscallResult::Return(n_written)
  P_W.store_pending_syscall_return(Ok(n_written))
```

W's `write` is the `wake_up()` for R's wait-queue: it enqueued R's `TaskId`; it did
not run R.

### 10.4 Act 4 — the reactor re-runs R

```
reactor loop, next iteration:
  drain_wakes_for_hart(hart)                                   (runtime.rs:704)
    pop R; R: Parked -> Runnable; scheduler.enqueue(R)
  pick_next_or_steal_local -> R
  poll R:
    run_thread(R) resumes inside dispatch(...).await:
      sys_read::poll: mailbox_R.poll() -> SourceFired{gen, S, READABLE}
        ActiveWait::matches? gen ok, source S, READABLE overlaps -> YES
        unregister Subscriber from S; clear_waker
        copy min(256, available) bytes into user buf
        -> Poll::Ready  =>  dispatch resolves SyscallResult::Return(n_read)
  run_thread(R) Phase 4 tail: P_R.store_pending_syscall_return(Ok(n_read))
  loop back to Phase 1
```

R is runnable again purely because its `Waker` was invoked and the reactor re-polled
it; the compiler-generated state machine resumed at the `.await` (§2).

### 10.5 Act 5 — R returns to userspace with the result

```
run_thread(R) Phase 1:  start_request -> new UserspaceRunWait (next generation)
                        ast_checkpoint: no pending signal -> Continue           (§9)
run_thread(R) Phase 2:  prepare_userspace_entry_payload_into(P_R, &mut ctx)     (execution.rs:775)
                          ctx = P_R.saved_user_context
                          drain pending_syscall_return = Ok(n_read)
                          ctx.regs[A0] = n_read         <-- the ONLY a0 writeback (Plan B)
                        enter_userspace_with_context(&ctx, root)   (divergent)
                          -> sret: user resumes after the ecall, with a0 = n_read
[R in userspace]  read() returns n_read.  Done.
```

The return value lived in `pending_syscall_return` across the entire
park/wake/resume cycle and was applied to a *fresh* trap frame at exactly one site —
the Plan B discipline (§5.7), now visibly necessary: the original trap frame had been
gone for many polls.

### 10.6 The whole flow as one diagram

```
   userspace R            trap shell (sync)         reactor (async executor)
   ───────────            ─────────────────         ────────────────────────
   read() ecall  ───────► capture regs -> P_R
                          resolve wait (Syscall)
                          wake()         ┐
                          Reschedule ────┘ longjmp ──► run_thread(R) Phase3 .await = Ready
                                                       Phase4 dispatch(read).await
                                                         pipe empty -> register on S
                                                         -> Pending  ──► R PARKED
                                                       (hart polls W instead)
   userspace W
   ───────────
   write() ecall ───────► ... -> run_thread(W) dispatch(write):
                                  buffer += bytes; fire WaitSource S
                                    -> post SourceFired to mailbox_R
                                    -> Waker(R).wake(): enqueue R
                                  return n_written
                                                       drain_wakes: R Parked->Runnable
                                                       poll R: dispatch(read) resumes
                                                         event matches -> copy bytes
                                                         -> Ready(n_read)
                                                       store pending_syscall_return
   read()=n_read ◄──────── sret (a0=n_read) ◄───────── Phase2 merge a0=n_read; enter_userspace
```

### 10.7 The master mapping, demonstrated

| Traditional kernel | txKernel future model | Act |
|---|---|---|
| Thread with a kernel stack | `run_thread` future in a boxed task | Cast / Act 2 |
| Scheduler run-queue + `schedule()` | reactor poll loop + run-queues | Acts 2, 4 |
| Context switch | `poll()` returns; state machine *is* the context | Act 2 |
| Trap entry (asm vector) | asm vector → `Rv64TrapFrame` | Act 1 |
| Handler runs syscall to completion | trap shell captures + resolves a wait, longjmps | Act 1 |
| Sleep on a wait-queue | register `ActiveWait` on `WaitSource S`, return `Pending` | Act 2 |
| `wake_up()` | fire `S` → `SourceFired` → `Waker::wake()` | Act 3 |
| Woken thread re-checks & completes | re-poll resumes `dispatch(read).await` | Act 4 |
| Return-from-trap | `enter_userspace_with_context` (a0 via Plan B) | Act 5 |
| Blocking costs a parked kernel stack | blocking costs one subscriber row + a boxed future | Act 2 |

---

## 11. Design Discussion and Trade-offs

### 11.1 What the model buys

- **Memory under concurrency.** The unit of suspension is a boxed future plus a
  wait-source subscription, not a kernel stack. Blocked-thread cost scales with live
  async state, typically far smaller than a full kernel stack per blocked thread.
- **Compositional blocking logic.** Multi-step blocking operations (`readv` over
  `read`, `futex` with timeout) are written as ordinary sequential `async` code; the
  compiler synthesizes the resume-state machine that a synchronous kernel would
  hand-roll.
- **Explicit, testable boundaries.** Trap handoff, suspension, wake, signal
  interruption, and signal delivery are each discrete steps with names and types,
  exercisable on a host test platform one poll at a time (§6.5, §9.3) without
  hardware.
- **A clean executor invariant.** Because `poll` always returns, the reactor never
  loses control to a runaway handler; preemption of userspace is handled out-of-band
  by the timer trap (§8.3) rather than by interrupting kernel code.

### 11.2 What it costs

- **A synchronous/asynchronous seam.** The trap shell runs without a future and must
  reach the running thread through a per-hart slot (§4.4), whose
  cleared-on-`Pending` window is a real race the architecture is explicitly built
  around (`thread_future.rs:184`). This is irreducible: hardware traps are
  synchronous.
- **Deferred writeback complexity.** The two-site Plan B discipline (§5.7) is less
  obvious than writing `a0` in the handler, and demands care that the page-fault,
  `ExecCommitted`, and signal-delivery arms each handle `a0` correctly.
- **Cooperative-poll constraints.** Kernel-side work must not block a hart inside a
  single `poll`; genuinely long synchronous work must be chunked or yielded, and IRQ
  context cannot take epoch guards (§8.2), forcing two-phase handlers.
- **Generation/ABA bookkeeping.** Lost-wake and stale-wake safety is achieved with
  explicit `WaitGeneration` / `UserspaceRunRequest` generations rather than implicit
  stack identity, adding fields and equality checks across the wait paths.

### 11.3 The retained synchronous fast path

The fast-path direct syscalls (§5.6) show the model is applied judgmentally: pure,
non-blocking, hot syscalls bypass the reactor entirely and return via a plain `sret`.
The handoff is reserved for operations that *might* block — exactly the operations
whose traditional cost was a parked kernel stack.

### 11.4 SMP considerations

Tasks R and W may sit on different harts; the global wake queue and per-hart
run-queues (§3) already account for cross-hart wakes, and work-stealing balances load.
The per-hart slot race (§4.4) is the seam that SMP wake-placement design is built
around. The memory in this report describes a pre-existing cfg-on `smp>1` boot issue
unrelated to the execution model itself; the single-hart narrative here is for
exposition, not a constraint of the design.

---

## 12. Conclusion

txKernel re-expresses the classical synchronous kernel without discarding any of its
conceptual stages. A thread is a `Future`; the reactor is its executor; a trap is the
resolution of an awaited wait; a wakeup is a `Waker`; and "blocking" is
`Poll::Pending`. The assembly vector, the trap frame, the cause decode, and a
fast-path of pure syscalls remain exactly as in any kernel — the novelty is
concentrated at a single seam, where the trap handler records a trap and longjmps back
into the executor instead of running the syscall to completion.

The payoff of that one change is that the unit of suspension shifts from the kernel
stack to a heap-resident future plus a wait-source subscription, and the cost is a
carefully managed synchronous/asynchronous boundary with explicit generation
bookkeeping. The end-to-end `read()` case study shows the layers composing exactly as
designed: a blocked reader holds no kernel stack, a writer's `wake()` re-queues it,
and the result reaches `a0` at one disciplined writeback site. The organizing claim
holds throughout — **a suspended thread is a parked future, not a parked kernel
stack.**

---

## Appendix A: Type Reference

| Type | Definition | Role |
|---|---|---|
| `Reactor` / `ReactorShared` | `tx-reactor/src/runtime.rs:182,262` | The executor; task table, scheduler, timers |
| `Task` / `TaskStatus` / `TaskFuture` | `tx-reactor/src/task.rs:113,90,21` | A unit of pollable work; `Pin<Box<dyn Future>>` |
| `HartRunQueues` | `tx-reactor/src/scheduler.rs:357` | Per-hart priority run-queues |
| `SliceConfig` | `tx-reactor/src/scheduler.rs` | Cooperative vs. preemptive budgeting |
| `TaskWakeState` | `tx-reactor/src/waker.rs:16` | Waker backing; enqueues `TaskId` on `wake()` |
| `UserspaceRunSlot` / `UserspaceRunWait` | `tx-reactor/src/userspace.rs:190,197` | One-waiter wait-queue: the trap-as-wait hinge |
| `ActivePhase` | `tx-reactor/src/userspace.rs:216` | `Pending` / `Running` / `Resolved(trap)` |
| `UserspaceTrapInfo` | `tx-reactor/src/userspace.rs:83` | `Syscall` / `PageFault` / `TimerPreempt` / `Fatal` |
| `SyscallRequest` | `tx-reactor/src/userspace.rs:37` | `{ nr, args: [u64; 6] }` |
| `run_thread` / `PerHartSlotted` | `tx-kernel/src/thread_future.rs:256,192` | The thread future; the per-hart slot bridge |
| `ThreadPayload` | `tx-subsystems/src/thread_runtime/structure.rs:140` | Saved registers, pending result, signal state |
| `UserTrapContext` | `tx-hal/src/trap.rs:169` | `{ regs[32], pc, status, fp }` |
| `TrapAction` / `KernelTrapSink` | `tx-hal/src/trap.rs:304,311` | Trap-shell policy; trap handler trait |
| `HandoffOutcome` | `tx-kernel/src/trap_handoff.rs:145` | Result of `hand_off_syscall` |
| `Rv64TrapFrame` | `boards/tx-hal-riscv64-qemu-virt/src/trap.rs:530` | The hardware trap frame |
| `SyscallResult` | `tx-shims/src/linux_syscall/result.rs:22` | `Return` / `Error` / `NoReturn` / `ExecCommitted` / `CloneReturn` |
| `SyscallCtx` | `tx-shims/src/linux_syscall/ctx.rs:19` | Per-syscall handles incl. mailbox, timer wheel |
| `WaitSource` | `tx-substrate/src/wake/wait_source.rs:83` | A wait-queue / readiness object |
| `TaskMailbox` | `tx-substrate/src/wake/mailbox.rs:258` | Per-task event queue + Waker |
| `ActiveWait` | `tx-substrate/src/wake/mailbox.rs:517` | A waiter's entry; generation ABA guard |
| `MailboxEvent` | `tx-substrate/src/wake/mailbox.rs:127` | `SourceFired` / `SignalDelivered` / ... |
| `WaitProtocol` / `WaitOutcome` | `tx-substrate/src/step/wait_protocol.rs:15,57` | Interruptibility class; wait result |
| `InterruptSummary` | `tx-subsystems/src/signal/mod.rs:442` | Packed deliverable/termination/stop bits |
| `AstOutcome` | `tx-subsystems/src/signal/mod.rs:808` | Signal-delivery decision at the AST checkpoint |
| `IrqHandled` | `tx-hal/src/lib.rs:1180` | `Done` / `Wake` / `NotMine` |

## Appendix B: Source Anchor Index

**Reactor / scheduler**
- Poll loop: `crates/tx-reactor/src/runtime.rs:786`; drain wakes `:704`; poll call `:849`; submit `:1293`
- Run-queues + selection: `crates/tx-reactor/src/scheduler.rs:357`, pick `:1271`
- Waker: `crates/tx-reactor/src/waker.rs:31,49`
- Preemption markers: `crates/tx-reactor/src/preempt.rs:92`

**Thread future**
- `run_thread`: `crates/tx-kernel/src/thread_future.rs:256`; loop top `:263`; entry await `:624`
- `PerHartSlotted`: `crates/tx-kernel/src/thread_future.rs:192`; slot-race note `:184`
- `ThreadPayload`: `crates/tx-subsystems/src/thread_runtime/structure.rs:140`
- entry merge: `crates/tx-subsystems/src/thread_runtime/execution.rs:775`; signal mailbox post `:165`
- boot submit: `crates/tx-kernel/src/init/exec.rs:647`; child submit: `crates/tx-kernel/src/init/reactor_submit.rs:305`; seam install `crates/tx-kernel/src/init/exec.rs:533`

**Traps / handoff**
- asm vector / frame / classify / resume: `boards/tx-hal-riscv64-qemu-virt/src/trap.rs:103,530,927,485`
- trap shell: `crates/tx-kernel/src/trap.rs:35` (`on_syscall`), `:52` (`on_timer_interrupt`), `:67` (`on_external_irq`), `:122` (fast path)
- handoff: `crates/tx-kernel/src/trap_handoff.rs:202` (syscall), `:279` (page fault), `:356` (timer), `:145` (`HandoffOutcome`)

**Suspension point**
- slot + wait + future + drop: `crates/tx-reactor/src/userspace.rs:190,197,234,341,300,490,535`

**Blocking syscalls / wait machinery**
- `SyscallCtx`: `crates/tx-shims/src/linux_syscall/ctx.rs:19`
- `WaitSource`: `crates/tx-substrate/src/wake/wait_source.rs:83` (`register` `:118`)
- `TaskMailbox`: `crates/tx-substrate/src/wake/mailbox.rs:258` (`register_waker` `:352`, `poll` `:480`, `next_generation` `:363`, `post` `:378`)
- `ActiveWait` / `MailboxEvent`: `crates/tx-substrate/src/wake/mailbox.rs:517,127`
- handlers: `crates/tx-shims/src/linux_syscall/time.rs:825` (nanosleep), `vm.rs:1192` (futex), `io.rs:875` (readv); wait helper `wait.rs:22`
- `reactor_submit` seam: `crates/tx-subsystems/src/reactor_submit/mod.rs`

**Interrupts**
- `IrqHandled`: `crates/tx-hal/src/lib.rs:1180`
- IRQ table / register / install: `crates/tx-kernel/src/irq.rs:45,85,159`
- `uart_rx_irq_handler` / `drain_uart_rx_pending`: `crates/tx-kernel/src/irq.rs:182,222`

**Signals**
- signal fields: `crates/tx-subsystems/src/thread_runtime/structure.rs:140`
- `InterruptSummary` / `ast_check` / `select_next_signal` / `AstOutcome`: `crates/tx-subsystems/src/signal/mod.rs:442,836,575,808`
- signal send + mailbox post: `crates/tx-subsystems/src/signal/mod.rs:1247,1344,739`
- `MailboxEvent::SignalDelivered`: `crates/tx-substrate/src/wake/mailbox.rs:187`
- `WaitProtocol` / `WaitOutcome`: `crates/tx-substrate/src/step/wait_protocol.rs:15,57`; reactor variant `crates/tx-reactor/src/wait.rs:39,94`
- regression test: `crates/tx-subsystems/tests/v3_signal_interrupt_wake.rs`

**Tests**
- `crates/tx-kernel/src/init/tests.rs:884,1014`; `crates/tx-kernel/src/thread_future/tests.rs`

---

*This report consolidates the nine-part tutorial series in the same directory
(`README.md`, `00_async-primer.md` … `08_end-to-end.md`). Code listings are
pseudocode; data-structure names, fields, and enum variants are accurate to the
source tree at the cited locations.*
