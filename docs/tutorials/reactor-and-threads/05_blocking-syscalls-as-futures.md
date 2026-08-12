# Part 5 — Blocking Syscalls as Futures

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 4 — The Suspension Point](04_the-suspension-point.md) · **Next:** [Part 6 — Interrupts and Wakes](06_interrupts-and-wakes.md)

In a traditional kernel, a `read()` on an empty pipe does this:

```
sys_read():
    if no data:
        add current thread to pipe's wait-queue
        set state TASK_INTERRUPTIBLE
        schedule()        # <- this thread's kernel stack is now parked here
    # ... woken later, resumes right here, re-checks, copies data ...
```

The thread's kernel stack is *pinned* at the `schedule()` call until data arrives.
A thousand blocked readers means a thousand parked kernel stacks. This chapter
shows the futures replacement: the blocked `read` is a `Future` returning
`Poll::Pending`, parked in the task table, costing one waker registration and no
stack. The hart is free to poll anything else.

## Where we are in the round-trip

Picking up from Part 2, Phase 4: the thread future has received
`UserspaceTrapInfo::Syscall(req)` from the wait, and dispatches it:

```rust
// run_thread, syscall arm  (thread_future.rs Phase 4)
let result: SyscallResult = dispatch::<P>(req, &ctx).await;   // async!
```

`dispatch` is an `async fn`. *This* `.await` is the second-level suspension: not
the trap wait of Part 4, but the syscall's own internal wait. When the syscall
cannot complete immediately, this await returns `Pending` — which propagates all
the way up through `run_thread` and `PerHartSlotted` to the reactor, parking the
*whole thread task*. That is the entire trick: a blocking syscall is a future, and
awaiting it parks the thread the same way any `Pending` does.

## The syscall context

Every async syscall handler receives a `SyscallCtx`, which carries the handles a
blocking syscall needs to park and be woken:

```rust
// crates/tx-shims/src/linux_syscall/ctx.rs:19
struct SyscallCtx<'a> {
    process: Cap<ProcessIdentity>,
    thread:  Cap<ThreadIdentity>,
    aspace:  Cap<AddressSpace>,
    mailbox: Option<Arc<TaskMailbox>>,          // park/wake channel
    timer_wheel: Option<TimerWheel>,            // deadline wakes (timeouts)
    delegate_registry: Option<Arc<DelegateRegistry>>, // off-task replies
    // credential snapshot, etc.
}
```

The `mailbox` is the one bound to this thread's reactor task back in `PerHartSlotted`
(Part 2). So a wake posted to the mailbox fires the task's `Waker` — re-polling
`run_thread`, which re-polls the suspended `dispatch` future, which resumes the
syscall. The loop closes.

## The wait machinery: `WaitSource`, `TaskMailbox`, `ActiveWait`

This is the general blocking primitive — the futures equivalent of a wait-queue
plus `wake_up()`. Three accurate types:

```rust
// crates/tx-substrate/src/wake/wait_source.rs:83
struct WaitSource {                 // == a wait-queue / readiness object
    id: WaitSourceId,
    subscribers: SpinMutex<Vec<Subscriber>>,   // who is waiting on it
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
    SignalDelivered { /* ... */ },   // a wake-hint; truth lives in InterruptSummary (Part 7)
    // AgentReplied / Abort (off-task delegate replies)
}
```

### The blocking pattern, generically

A blocking syscall future implements `poll` like this (condensed from the wait
helpers; this is the shape `await_wait_source` uses):

```rust
fn poll(self, cx) -> Poll<()> {
    // 1. Make sure THIS task's Waker is the one the mailbox will fire.
    self.mailbox.register_waker(cx.waker().clone());

    // 2. Drain mailbox events; resolve if one matches our interest,
    //    or if a signal arrived (interruptible wait).
    while let Some(event) = self.mailbox.poll() {
        if self.active.matches(&event) {           // ActiveWait::matches: gen+source+mask
            self.mailbox.clear_waker();
            return Poll::Ready(());                // condition satisfied
        }
        if matches!(event, MailboxEvent::SignalDelivered { .. }) {
            self.mailbox.clear_waker();
            return Poll::Ready(());                // interrupted -> caller maps to EINTR
        }
    }
    // 3. Nothing yet: stay parked.
    Poll::Pending
}
```

The pieces map one-to-one onto the traditional model:

| Traditional | Futures |
|---|---|
| wait-queue object | `WaitSource` |
| `current`'s wait-queue entry | `ActiveWait` (+ `Subscriber` row) |
| `add_wait_queue` + `TASK_INTERRUPTIBLE` | `WaitSource::register` + `register_waker` + `Pending` |
| `wake_up(&wq)` | post `SourceFired` to subscribers' mailboxes → `Waker::wake()` |
| `signal_pending()` check before sleeping | `SignalDelivered` resolves the wait → `EINTR` |
| ABA / lost-wakeup guards | `WaitGeneration` matched in `ActiveWait::matches` |

The generation deserves a note: a waiter captures `mailbox.next_generation()` at
registration and embeds it in its `ActiveWait`. A `SourceFired` event carries the
generation captured *when it was queued*. `matches` requires equality — so a wake
meant for a previous wait on the same source is dropped, closing the classic
lost-wakeup race without disabling interrupts.

## Three concrete handlers

### `nanosleep` — wait on a deadline

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

`sleep_until_deadline` registers the deadline with `ctx.timer_wheel`; when the
reactor's timer queue (Part 1) crosses the deadline it posts to the mailbox and
wakes the task. This is `schedule_timeout()` — minus the parked stack.

### `futex` wait — wait on a hashed bucket

```rust
// crates/tx-shims/src/linux_syscall/vm.rs:1192
async fn sys_futex<P: TimeIf>(args, ctx) -> SyscallResult {
    let uaddr = args[0]; let op = args[1] as u32; let val = args[2] as u32;
    // FUTEX_WAIT: park on the bucket's WaitSource until FUTEX_WAKE fires it.
    let mut script = build_subject_script_ctx(ctx);
    if let Some(d) = deadline_ns { script = script.with_deadline(Deadline::from_raw(d)); }

    match drive(FutexWaitOp { uaddr, val, .. }, &mut script,
                DriveMode::Waiting, ctx.mailbox.as_ref(), ...).await {
        Ok(())          => SyscallResult::Return(0),
        Err(EINTR)      => SyscallResult::Error(EINTR),
        Err(e)          => SyscallResult::error_from(e),
    }
}
```

The futex bucket is a `WaitSource`; a `FUTEX_WAKE` from another thread calls
`step_futex_wake`, which fires that source. Same hashed-wait-queue design as a
classic kernel futex — the bucket is just a `WaitSource` and the sleepers are
parked futures.

### `readv` — compose smaller awaits

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
loop over `sys_read`, but each `sys_read` may independently park and resume. In the
traditional model you would hand-roll an explicit state machine to remember which
iovec you were on across a sleep. Here the compiler generates it (Part 0).

## What "parked" actually costs

When `dispatch(req).await` returns `Pending`, here is the precise state of the
system:

- The thread's `run_thread` future is suspended at the `dispatch(...).await` — its
  state machine (which iovec, which deadline) lives in the boxed task.
- The thread's registers live in `payload.saved_user_context` (captured at the
  trap, Part 3).
- The task is `Parked` in the reactor's task table; it is on **no** run-queue.
- One `ActiveWait` row sits in one `WaitSource`'s subscriber list, holding a `Weak`
  ref to the mailbox.
- The hart immediately picks the next runnable task and polls it.

No kernel stack is reserved. Ten thousand blocked readers are ten thousand parked
futures plus ten thousand subscriber rows — heap, not stacks. That is the resource
story the future model buys.

## The wake side (preview)

Who fires the `WaitSource`? Whoever makes the condition true: a writer's `write`
pushing data into the pipe, a `FUTEX_WAKE`, a timer crossing a deadline, or a
device interrupt delivering input. The interrupt case — the one that turns a
hardware event into a `Waker::wake()` — is [Part 6](06_interrupts-and-wakes.md).

## The `reactor_submit` seam (aside)

A subtlety worth naming: `tx-shims` (where these handlers live) is a library that
**cannot depend on** the kernel's reactor crate — that would be a dependency cycle.
So when a syscall like `clone` needs to put a *new* thread onto the run-queue, it
goes through a function-pointer seam installed at boot:

```rust
// crates/tx-subsystems/src/reactor_submit/mod.rs
type SubmitChildThreadFn = fn(Cap<ProcessIdentity>, Cap<ThreadIdentity>) -> SubmitChildThreadStatus;
static SUBMIT_CHILD_THREAD_FN: AtomicPtr<()> = AtomicPtr::new(null_mut());

fn install_submit_child_thread(f: SubmitChildThreadFn);   // kernel calls at boot (exec.rs:533)
fn submit_child_thread(p, t) -> SubmitChildThreadStatus;  // shims calls from sys_clone
```

The kernel installs a closure (`init/reactor_submit.rs:305`) that wraps the child
thread in `PerHartSlotted` and submits it — so the child becomes an ordinary task
(Part 2). The `SyscallResult::CloneReturn { child_submit, .. }` you saw in Part 2
carries the status of that submission back to the thread future.

## Summary

- A blocking syscall is an `async fn`; awaiting it (`dispatch(req).await`) parks the
  whole thread task with `Poll::Pending`. No kernel stack is held.
- The generic blocking primitive is `WaitSource` (wait-queue) + `TaskMailbox`
  (the task's wake channel + `Waker`) + `ActiveWait` (the waiter's entry, with a
  `WaitGeneration` ABA guard). Posting `SourceFired` and calling `Waker::wake()`
  *is* `wake_up()`.
- `nanosleep` waits on a deadline (timer wheel), `futex` on a hashed bucket
  `WaitSource`, `readv` composes per-iovec `read` awaits — the compiler builds the
  resume-state machine you would otherwise hand-roll.
- Parking costs one subscriber row + the boxed future, not a stack — the core
  scalability win.
- `tx-shims` reaches the reactor only through the `reactor_submit` function-pointer
  seam, avoiding a dependency cycle.

---

**Anchors:**
- `SyscallCtx`: `crates/tx-shims/src/linux_syscall/ctx.rs:19`
- `WaitSource`: `crates/tx-substrate/src/wake/wait_source.rs:83` (`register` `:118`)
- `TaskMailbox`: `crates/tx-substrate/src/wake/mailbox.rs:258` (`register_waker` `:352`, `poll` `:480`, `next_generation` `:363`, `post` `:378`)
- `ActiveWait` / `matches`: `crates/tx-substrate/src/wake/mailbox.rs:517`
- `MailboxEvent`: `crates/tx-substrate/src/wake/mailbox.rs:127`
- handlers: `time.rs:825` (nanosleep), `vm.rs:1192` (futex), `io.rs:875` (readv)
- wait helper `await_wait_source`: `crates/tx-shims/src/linux_syscall/wait.rs:22`
- `reactor_submit` seam: `crates/tx-subsystems/src/reactor_submit/mod.rs`; install `crates/tx-kernel/src/init/exec.rs:533`

**Next:** [Part 6 — Interrupts and Wakes](06_interrupts-and-wakes.md)
