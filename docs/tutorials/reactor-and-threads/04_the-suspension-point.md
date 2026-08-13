# Part 4 — The Suspension Point: `UserspaceRunWait`

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 3 — Traps and the Handoff](03_traps-and-handoff.md) · **Next:** [Part 5 — Blocking Syscalls as Futures](05_blocking-syscalls-as-futures.md)

Part 3 ended with the trap shell calling `complete_interesting_trap`. Part 2 ended
with the thread future sitting at `entry_wait.await`. This chapter is the hinge
between them — a single, small wait object that is the entire interface between the
synchronous trap world and the async reactor world. It is a **wait-queue with
exactly one waiter**, and seeing it reduces the whole architecture to something you
can hold in your head.

## The role

Recall the round-trip from Parts 2–3:

```
run_thread (Phase 1):  open a wait        -> UserspaceRunWait
run_thread (Phase 2):  enter userspace    (divergent)
   ... user runs, then traps ...
trap shell:            resolve the wait    (complete_interesting_trap)
   ... longjmp back into the reactor ...
run_thread (Phase 3):  entry_wait.await    -> Poll::Ready(UserspaceTrapInfo)
```

The wait carries one of four trap outcomes back to the future:

```rust
// crates/tx-reactor/src/userspace.rs:83
enum UserspaceTrapInfo {
    Syscall(SyscallRequest),
    PageFault(PageFaultInfo),
    TimerPreempt,
    Fatal(FatalTrapInfo),
}
```

## The data structure

The slot is a clone-shared cell. The thread future holds one handle (via its
`ThreadPayload.userspace_slot`), the trap shell gets another by cloning it off the
per-hart payload (Part 3):

```rust
// crates/tx-reactor/src/userspace.rs:190
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
```

And the future the thread `.await`s:

```rust
// crates/tx-reactor/src/userspace.rs:197
struct UserspaceRunWait {
    slot:     UserspaceRunSlot,
    request:  UserspaceRunRequest,  // which generation this wait belongs to
    finished: bool,
}
```

`ActivePhase` is just `TASK_RUNNING` vs `TASK_INTERRUPTIBLE` vs "wake condition
satisfied," specialized to one waiter. The `request` generation is the ABA guard:
a stale `complete_interesting_trap` for a previous round-trip is rejected rather
than resolving the wrong wait.

## Opening the wait

```rust
// UserspaceRunSlot::start_request  (userspace.rs:234)
fn start_request(&self) -> Result<UserspaceRunWait, UserspaceRunError> {
    let mut state = self.state.lock();
    if state.active.is_some() {
        return Err(Busy(...));         // one in-flight wait per thread
    }
    let request = UserspaceRunRequest(state.next_request);
    state.next_request += 1;           // new generation
    state.active = Some(ActiveRun {
        request, phase: ActivePhase::Pending,
        dispatches: 0, preemptions: 0, waker: None,
    });
    Ok(UserspaceRunWait { slot: self.clone(), request, finished: false })
}
```

This is Phase 1 of `run_thread`. The thread stores `request` in
`payload.active_request` so the trap shell can name this exact wait later.

## Resolving the wait (the trap shell side)

```rust
// UserspaceRunSlot::complete_interesting_trap  (userspace.rs:341)
fn complete_interesting_trap(&self, request, trap: UserspaceTrapInfo)
    -> Result<UserspaceRunStatus, UserspaceRunError>
{
    let (status, waker) = {
        let mut state = self.state.lock();
        let active = state.active.as_mut().ok_or(NoActiveRequest)?;
        if active.request != request {
            return Err(StaleRequest { attempted: request, active: active.request });
        }
        // ... (a timer-preempt resolution can be upgraded to a real trap) ...
        active.phase = ActivePhase::Resolved(trap);
        (active.status(), active.waker.take())   // take the parked Waker
    };
    if let Some(waker) = waker {
        waker.wake();                            // <- THE wake_up()
    }
    Ok(status)
}
```

Two moves, and they are the whole interface:

1. Flip `phase` to `Resolved(trap)` — record *what* woke the waiter.
2. Take the stashed `Waker` and call `wake()` — tell the reactor to re-poll the
   thread's task.

This is textbook `wake_up()`: set the condition, then wake the sleeper. The
generation check (`active.request != request`) is the only added safety.

## Awaiting the wait (the thread future side)

```rust
// impl Future for UserspaceRunWait  (userspace.rs:490)
fn poll(self, cx) -> Poll<UserspaceTrapInfo> {
    if self.finished { return Poll::Pending; }       // one-shot
    let mut state = self.slot.state.lock();
    let active = match state.active.as_mut() { Some(a) => a, None => return Pending };
    if active.request != self.request { return Poll::Pending; }  // not ours

    match active.phase {
        ActivePhase::Resolved(trap) => {
            state.active = None;           // consume the slot
            self.finished = true;
            Poll::Ready(trap)              // hand the trap to run_thread
        }
        ActivePhase::Pending | ActivePhase::Running => {
            // stash the latest Waker (idempotent via will_wake)
            if active.waker.as_ref().is_none_or(|w| !w.will_wake(cx.waker())) {
                active.waker = Some(cx.waker().clone());
            }
            Poll::Pending                  // suspend the thread future
        }
    }
}
```

This is the exact shape from the Part 0 primer: *ready → return the value; not
ready → stash the waker, return Pending*. The waker it stashes is the one the
reactor built for this task in Part 1, step (4). So when the trap shell calls
`waker.wake()`, the task is re-queued, the reactor re-polls `run_thread`, and this
`poll` runs again — now finding `Resolved` and returning the trap.

There is also a `Drop` impl (`userspace.rs:~535`) that clears `state.active` if a
wait is dropped unfinished — so an aborted thread cannot leave a stuck slot.

## The two timelines: hardware vs. host test

This is the single most clarifying way to understand the suspension point, because
the two environments resolve the wait at *different times* relative to the await.

### On real hardware — the wait is already resolved

```
poll #N of the task:
  run_thread Phase 1: start_request -> Pending
  run_thread Phase 2: enter_userspace_with_context   (divergent; control leaves)
       userspace executes ... ecall ...
       trap vector -> on_syscall -> hand_off_syscall:
            complete_interesting_trap -> phase=Resolved, waker.wake()
       apply_trap_action(Reschedule) -> longjmp back into the reactor
  run_thread Phase 3: entry_wait.await -> poll sees Resolved -> Poll::Ready(trap)
  run_thread Phase 4: dispatch the trap
```

All of this happens **within a single reactor poll of the task**. The divergent
userspace dive and the longjmp-back happen *inside* Phase 2→3, so by the time the
`.await` re-checks the slot, it is already `Resolved`. The `waker.wake()` the trap
shell fired is effectively redundant on the hot path (the poll never actually
suspended) — but it is essential for correctness if anything reorders, and it is
what makes the *host* path work.

### On the host test platform — the await genuinely suspends

There is no real trap on the host. `enter_userspace_with_context` is a test stub
that records the entry and returns. So:

```
poll #1: Phase 1 start_request -> Pending
         Phase 2 enter (stub returns)
         Phase 3 entry_wait.await -> poll sees Pending -> the task suspends here

test driver: payload.userspace_slot().complete_interesting_trap(req, Syscall(...))
             -> phase=Resolved, waker.wake()  (re-queues the task)

poll #2: Phase 3 entry_wait.await -> poll sees Resolved -> Poll::Ready(Syscall(req))
         Phase 4 dispatch
```

Now the state machine is fully observable, one poll at a time. The tests exploit
exactly this. From `crates/tx-kernel/src/init/tests.rs:884`:

```rust
let future = run_thread::<TestPlatform>(leader.clone(), payload.clone());
// poll once: must be Pending (parked at entry_wait.await, no trap yet)
assert!(poll_once(&mut future).is_pending());     // tests.rs:903

// inject a syscall trap, then poll again
payload.userspace_slot().complete_interesting_trap(token, Syscall(req));
// ... drives the dispatch, asserts exactly the expected re-entries ...
```

and the timer-preempt variant (`tests.rs:1014`) checks that a `TimerPreempt`
resolution makes the thread `yield_now().await` and re-enter *from the same saved
context without resolving a new wait* — i.e. preemption costs one extra poll and
zero architectural change. The `thread_future/tests.rs` suite pins the same
behaviors against the in-tree test platform.

The lesson: the host/hardware asymmetry is not a hack — it is the proof that the
trap round-trip is *modeled as a wait*. On hardware the wait happens to resolve
synchronously inside one poll; on the host it resolves across two. The future code
is identical either way, which is precisely the point of
`txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT`.

## Summary

- `UserspaceRunSlot`/`UserspaceRunWait` is a one-waiter wait-queue: `ActivePhase`
  is the wait condition, the stashed `Waker` is the sleeper, `request` is the ABA
  generation guard.
- `complete_interesting_trap` (trap shell) = set condition + `wake()`.
  `UserspaceRunWait::poll` (thread future) = ready→return trap, else stash waker +
  `Pending`. This is `wake_up()` and interruptible sleep, reduced to essentials.
- On hardware the wait resolves *within one poll* (the divergent dive + longjmp
  happen inside it); on the host it resolves *across two polls*. Same future code —
  which is the evidence that "a trap is a wait."

Next we let the trap the future just received be a *blocking* syscall, and watch
the second-level await park the thread without parking a hart.

---

**Anchors:**
- slot + wait types: `crates/tx-reactor/src/userspace.rs:83,190,197,208`
- `start_request`: `:234`; `complete_interesting_trap`: `:341`; `record_timer_preemption`: `:300`
- `Future for UserspaceRunWait`: `:490`; `Drop`: `:535`
- tests: `crates/tx-kernel/src/init/tests.rs:884,1014`; `crates/tx-kernel/src/thread_future/tests.rs`

**Next:** [Part 5 — Blocking Syscalls as Futures](05_blocking-syscalls-as-futures.md)
