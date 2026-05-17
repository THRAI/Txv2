# Thread Runtime — v1

<!-- txdoc:02-EXECUTION-THREAD-RUNTIME-V1 -->

> **[deprecated by v5]** — Superseded by `docs/Txv3/` step model
> (03_STEP_MODEL_v2.md), execution scope (06_EXECUTION_SCOPE_v1.md),
> and `docs/Txv3/07_BLAST_RADIUS.md` migration plan. v4 vocabulary
> (`Blocked`, `WakeCarrier`, `InterestConditions`) is fully retired
> from code. This document is retained for historical reference only.

## Status
<!-- txdoc:THREAD-STATUS -->

Draft v1.5. [deprecated by v5]

This document specifies the **runtime semantics of a thread** in txKernel: what a thread is as a running entity, what states it occupies, how it interacts with the reactor, and how signal delivery is realized across the step/script/reactor boundary.

It builds on:

- **`REACTOR_v0`** — the reactor boundary (tasks, wait/wake, AST, sync coord).
- **`object_model_v2`** — the Identity/Payload split applied to threads.
- **`STEP_MODEL_v1`** and **`INVARIANTS`** — the step discipline and script rules.
- **`BUS_v1`** — wakers, carriers, publication.

It is the semantic counterpart to `REACTOR_v0`. The reactor contract is mechanism-only; this document carries the weight of *what threads are* and *how they behave* over time.

Related and consumer specs:

- **`SIGNAL_v1`** — POSIX signal compatibility shim. Consumes THREAD_RUNTIME's state placement (signal_mask, signal_summary, thread_pending on ThreadPayload; stop_state on ThreadPayload). Specifies `deliver_posix_signal` as the canonical producer entry point; THREAD_RUNTIME's `step_post_signal` is an internal helper invoked by that entry point for thread-targeted catchable-handler routings.
- **`PROCESS_v1`** — thread-group container. PROCESS_v1 §7.6 specifies the process-level fan-out helpers invoked by `deliver_posix_signal` for Process/ProcessGroup target kinds.

### What this document pins
<!-- txdoc:THREAD-WHAT-THIS-DOCUMENT-PINS -->

- `ThreadIdentity` / `ThreadPayload` content and lifetime.
- Thread states and transitions.
- The thread-as-future model and the reactor-future ownership split.
- Interruption mechanics: the wait-adapt decision site, the thread-runtime-provided interrupt predicate, and the two-site check_interrupt / AST discipline.
- Per-thread signal state placement; `group_pending` vs. per-thread pending.
- Thread exit ordering and its interaction with process exit.

### Zone-derived type policy
<!-- txdoc:THREAD-ZONE-DERIVED-TYPE-POLICY -->

THREAD_RUNTIME inherits the global policy-zone rule through the
`ThreadIdentity` / `ThreadPayload` split:

| Thread-runtime declaration | Zone-derived public type | Reclamation role |
|---|---|---|
| `ThreadIdentity` | `Cap<ThreadIdentity>`, `Weak<ThreadIdentity>`, `IdentRef<'g, ThreadIdentity>` | tid addressability and post-exit status shell |
| `ThreadPayload` | `PayloadCap<ThreadPayload>` reached through `ThreadIdentity.payload` | running execution state, signal mask, saved regs |
| Reactor future retention | `Cap<ThreadPayload>` held by the future | keeps payload alive until task drain completes |
| `TaskHandle` / `TaskId` | reactor-owned opaque handles | temporal, not zone-derived semantic entities |
| Witnesses for signal/exit steps | `IdentRef<'g, ThreadIdentity>` inside witness types | EBR-scoped observation only |

The reactor and scheduler never select thread zone policies. Thread steps
upgrade witnesses to `Cap` / payload evidence and let the entity-zone
declaration choose the hidden reclamation policy.

### What this document defers
<!-- txdoc:THREAD-WHAT-THIS-DOCUMENT-DEFERS -->

- **Full signal action set and disposition rules.** Dispositions (ignore / default / catch) and per-signal default behaviors are catalogued in a signal spec built on this doc's state placement.
- **ptrace runtime stops.** The three orthogonal intercepts are observation-subsystem territory; this doc names where they attach but not what they do.
- **Scheduler policy.** Per REACTOR_v0 non-goals.
- **Thread-group-leader election and group-exit propagation details.** Sketched here; fully specified in PROCESS_v1.

---

## 1. Position in the architecture
<!-- txdoc:THREAD-1-POSITION-IN-THE-ARCHITECTURE -->

A thread is:

- a **reactor tenant** — realized as exactly one reactor task carrying an author-supplied future;
- a **semantic entity** — split into `ThreadIdentity` (addressability) and `ThreadPayload` (runtime state) per `object_model_v2`;
- a **process member** — part of a thread group linked by intrusive DLL, sharing `ProcessPolicy` and process-attached objects.

It is **not**:

- the reactor's abstraction (the reactor sees a task, not a thread);
- a purely static policy object (execution state lives in `ThreadPayload`);
- the same thing as a kernel worker (workers are reactor tenants too, but carry no `ThreadIdentity`).

The layering is:

```
reactor (REACTOR_v0)                ← task scheduling, wait/wake, AST
    ↑
wait-adapt (reactor service)        ← classified outcomes, interrupt decision
    ↑
thread-runtime (this document)      ← thread-as-future, signal delivery, exit
    ↑
scripts (per-syscall, per-thread)   ← drive steps, compose waits
```

Thread-runtime sits *between* the reactor and the scripts. It supplies the future that the reactor schedules, and it consumes wait-adapt's classified outcomes to deliver signals and manage exit.

---

## 2. ThreadIdentity / ThreadPayload
<!-- txdoc:THREAD-2-THREADIDENTITY-THREADPAYLOAD -->

### 2.1 Rationale for the split
<!-- txdoc:THREAD-2-1-RATIONALE-FOR-THE-SPLIT -->

Threads admit the same `structural ⟂ payload` partial order that processes do:

- A thread that has exited but not yet been reaped by `waitid`-of-tid (or equivalent) is addressable (by tid, for ptrace's wait surface) but has no running computation.
- A thread whose parent has collected its status can be fully reclaimed.

Per `object_model_v2` §8.1.1, this forces factoring into two zone-allocated types with independent reclamation lifetimes.

### 2.2 ThreadIdentity
<!-- txdoc:THREAD-2-2-THREADIDENTITY -->

```rust
pub struct ThreadIdentity {
    pub tid: Tid,                              // allocated from pid namespace
    pub owner_proc: Cap<ProcessIdentity>,      // which thread group
    pub chain: DllNode<ThreadIdentity>,        // per-process thread DLL
    pub payload: Option<PayloadCap<ThreadPayload>>,
    pub exit_status: AtomicExitStatus,         // written before payload drop
}
```

- `tid`: namespace-scoped identifier; allocated and reclaimed per the PID namespace rules (see PROCESS_v1).
- `owner_proc`: back-pointer to the thread group. An addressability binding: it names which group this thread belongs to without retaining any payload on the process side that isn't already retained by virtue of there being a live thread.
- `chain`: intrusive DLL linkage. The process keeps a per-group thread DLL; each thread's entry participates in that list. Per BSD intrusive-DLL discipline.
- `payload`: `None` after thread exit; `Some(PayloadCap<ThreadPayload>)` while the thread is running or exiting.
- `exit_status`: written before `payload` transitions to `None`. Living in `ThreadIdentity` so that post-exit observers (parent waitid, ptrace tracer) can read status without touching payload.

`ThreadIdentity` projections:

- **structural** — identity retention count > 0 (some Cap<ThreadIdentity> extant).
- **addressability-for-waitid** — binding present in the process's `threads` container.
- **addressability-for-tgkill** — structural AND `payload.is_some()` (signalable implies payload live; dead threads are not signalable).

### 2.3 ThreadPayload
<!-- txdoc:THREAD-2-3-THREADPAYLOAD -->

```rust
pub struct ThreadPayload {
    // Reactor linkage
    pub task: TaskHandle,                      // reactor's handle on this thread's task

    // Signal runtime state (per-thread)
    pub signal_mask: AtomicSignalMask,         // sigprocmask current value
    pub thread_pending: PendingSignalQueue,    // thread-directed pending signals (tkill)
    pub signal_summary: AtomicInterruptSummary, // wait-adapt reads this (see §5)

    // Stop / intercept state
    pub stop_state: AtomicStopState,           // running / stop-requested / stopped / ptrace-stopped

    // Execution state
    pub regs: RegisterContext,                 // saved userspace trap frame
    pub alt_stack: Option<AltSignalStack>,     // sigaltstack (userspace buffer descriptor)

    // Thread exit notification (CLONE_CHILD_CLEARTID / set_tid_address)
    pub child_tid_clear: AtomicOption<UserPtr<u32>>,  // see §2.6

    // Identity back-pointer
    pub identity: Weak<ThreadIdentity>,        // upgradeable under epoch guard
}
```

- **Reactor linkage.** `task` is the reactor's opaque handle, acquired at thread creation when the thread's future is submitted. Dropping it initiates task drain (see §4.4).
- **Signal mask and thread-pending.** Placed on payload, not identity. The mask is execution-delivery state with no meaningful post-exit semantics; thread-directed pending signals lose their target when the thread exits.
- **Signal summary.** An atomic summary the wait-adapt primitive reads cheaply on wake (see §5.2 for the contract). Its value is derived from the intersection of pending signals and the current mask; thread-runtime is responsible for keeping it current.
- **Stop state.** Explicit state for SIGSTOP / ptrace-stop transitions (see §6).
- **Execution state.** `regs` is the saved userspace trap frame (captured when the thread entered the kernel at its most recent trap). `alt_stack` is a descriptor into userspace memory for sigaltstack-registered alternate signal stacks. **There is no kernel stack here** — see §2.4.
- **Thread exit notification.** `child_tid_clear` holds an optional userspace address at which the thread's tid value was written (by clone) and will be cleared (at thread exit) — see §2.7.
- **Identity back-pointer.** `Weak<ThreadIdentity>` rather than `Cap`: the payload references its identity for ownership queries without creating a retention cycle.

`ThreadPayload` projections:

- **structural** — payload retention count > 0 (some `PayloadCap<ThreadPayload>` extant).
- **payload** — same as structural for `ThreadPayload` (trivially co-extensive; ThreadPayload has no sub-projections).

### 2.4 The stackless coroutine model
<!-- txdoc:THREAD-2-4-THE-STACKLESS-COROUTINE-MODEL -->

txKernel runs **stackless coroutines**. This has structural consequences that are worth naming explicitly, because they rule out assumptions a reader coming from a stackful-kernel background (Linux, BSD, Mach) might otherwise carry in.

**What "stackless" means here.** At any `.await` in the thread's future, the saved state *is* the future object itself — a compiler-generated async state machine whose fields encode the values live across the yield point. There is no separate kernel call stack persisting across yields. When the reactor polls the future, execution resumes by re-entering the state machine at the saved resume point; when the future yields, control unwinds through all frames back to the reactor.

**Consequences.**

- **No per-thread kernel stack.** The kernel has no `kstack`-like structure per thread. Saved state across yields lives in the future. This is why `ThreadPayload` does not carry a kernel stack field.
- **Trap-entry scratch is per-CPU, not per-thread.** The short window between "userspace trap entered kernel mode" and "reactor resumes polling this thread's future" needs stack space to run the trap prologue. That space is HAL-owned, per-CPU, and transient across individual trap-handling episodes. It is not per-thread and has no ThreadPayload representation.
- **`regs` is the saved userspace trap frame.** On trap entry, the HAL saves userspace registers (and whatever architectural trap-frame state is needed to return to userspace) into `ThreadPayload.regs`. This is the full saved state needed to re-enter userspace; it is not a fragment of a kernel call stack.
- **Signal-frame construction uses transient step-local buffers.** When site B delivers a signal, it composes a userspace signal frame in a buffer whose lifetime is the step's lifetime, then copies it to the userspace stack. The buffer does not persist across the step boundary and is not a per-thread field.
- **Deep recursion in kernel code is bounded by the future's state-machine size, not by a stack guard page.** Each level of `async fn` nesting becomes a nested state in the generated type. This is a compile-time resource, not a runtime one.

This model is the reason the reactor's polling discipline works: a poll runs the future forward until it yields, and yielding is a compiler-generated return rather than a stack switch. The reactor does not need context-switching infrastructure in the traditional sense.

### 2.5 Reclamation order
<!-- txdoc:THREAD-2-5-RECLAMATION-ORDER -->

Two-stage, matching `ProcessIdentity`/`ProcessPayload`:

1. **Thread exit** (`step_thread_exit`): write `exit_status` into `ThreadIdentity`; transition `stop_state` to terminal; publish thread-exit wake events; drop `ThreadIdentity.payload` from `Some(PayloadCap<ThreadPayload>)` to `None`.
   - This releases thread-runtime's direct retention on `ThreadPayload`. The reactor's task future may still hold a `Cap<ThreadPayload>` (see §4); reclamation waits.
2. **Task drain**: reactor polls the thread's future one last time (or observes it already resolved) and drops the future. The future's `Cap<ThreadPayload>` releases. If no other `Cap<ThreadPayload>` exists, `ThreadPayload` SENTINEL_DEAD fires and its zone slot is enqueued for reclamation.
3. **Reap**: parent (or tracer) collects status via waitid-of-tid; the binding in the process's thread container is withdrawn; last `Cap<ThreadIdentity>` releases; SENTINEL_DEAD on `ThreadIdentity`.

**Note: semantic exit ≠ physical payload reclamation.** Step 1 is when the thread is semantically dead. Step 2 may lag by an arbitrary reactor-scheduling delay. During that window:

- `ThreadIdentity.payload.is_some()` is `false`.
- Signal-delivery to this thread fails (addressability-for-tgkill is false).
- The reactor is still draining the task.

This is not a zombie in the process sense — it's a physical-reclamation lag window. No observable state claims the thread is alive during the window; only the implementation has not yet returned the payload's zone slot.

### 2.6 Thread exit notification (CLONE_CHILD_CLEARTID)
<!-- txdoc:THREAD-2-6-THREAD-EXIT-NOTIFICATION-CLONE-CHILD-CLEARTID -->

Userspace pthread_join is implemented on top of a kernel mechanism that clears a userspace memory location at thread exit and wakes any waiters on that location via futex. This is driven by two clone-related features:

- **`CLONE_CHILD_CLEARTID`** — a clone flag. Registers a userspace address; at child thread exit, the kernel writes 0 there and performs FUTEX_WAKE.
- **`set_tid_address(2)`** — a syscall. Changes (or queries) the registered address for the calling thread.

State:

- `ThreadPayload.child_tid_clear: AtomicOption<UserPtr<u32>>`.
  - `Some(addr)` — the address to clear on thread exit.
  - `None` — no clearing to do.
- Set at thread creation if `CLONE_CHILD_CLEARTID` is set.
- Modified via `set_tid_address` at any time.

Exit behavior (§7.2, step_thread_exit commit phase):

1. Take the `child_tid_clear` value (atomic swap to None).
2. If Some(addr): write 0 to `*addr` in userspace.
3. If the write succeeds: issue FUTEX_WAKE on addr with a wake count of 1.
4. If the write fails (userspace address invalid, process AS torn down): silently ignore — the clear is best-effort.

This is the only mechanism a thread has for notifying userspace waiters of its exit. pthread_join's userspace implementation does FUTEX_WAIT on the address; kernel's clear-and-wake satisfies the wait.

**CLONE_PARENT_SETTID / CLONE_CHILD_SETTID** are related but distinct: they write the *new* tid to specified userspace buffers at clone time, not at exit. They are handled in the clone step, not in thread_exit.

Interaction with AS teardown: at exit_group or process_exit, the AS may be torn down before each thread completes its exit sequence. In that case the userspace write in step 2 fails, and no FUTEX_WAKE fires. Any pthread_join waiters are woken by the AS teardown's side effects (their FUTEX_WAIT blocks on memory that no longer exists in a process that is also exiting). This is POSIX-acceptable: pthread_join's contract applies to threads in a still-existing process.

---

## 3. Thread states
<!-- txdoc:THREAD-3-THREAD-STATES -->

Thread states describe **where the thread is in its lifecycle from thread-runtime's vantage point**. They are distinct from reactor task state (runnable / parked / stopped-for-scheduler), which is mechanism-only.

### 3.1 State set
<!-- txdoc:THREAD-3-1-STATE-SET -->

- **Running** — the thread's future is live and the task is either being polled or is runnable/waiting within normal step flow.
- **Waiting** — the thread is parked inside a wait-adapt invocation, blocked on a step's `Blocked(channel, mask)`. From the reactor's view, task is blocked-on-wake.
- **Stopped** — the thread is parked on the **stop channel**, awaiting SIGCONT or ptrace resume. Realized as a wait on a thread-runtime-owned channel; no new reactor primitive needed (see §6).
- **Exiting** — `step_thread_exit` has begun; payload drop is in progress; exit_status already written.
- **Dead (physical-lag)** — `ThreadIdentity.payload == None`; reactor future drain may still be in progress.

Reap is a `ThreadIdentity` reclamation event, not a thread state — by the time reap happens, there is no thread in the runtime sense.

### 3.2 Transitions
<!-- txdoc:THREAD-3-2-TRANSITIONS -->

```
                 fork/clone               waitid(tid)
                    ↓                         ↓
               [ Running ] ←───────→  [ Dead ] → (ThreadIdentity reclaimed)
                 ↕      ↕                ↑
                 ↕      ↕                │
          [ Waiting ]  [ Stopped ]       │
                 ↕      ↕                │
                 └──────┴─────→ [ Exiting ]
                                  (step_thread_exit)
```

Transitions:

- **Running → Waiting** — step returns `Blocked(channel, mask)`; script invokes wait-adapt.
- **Waiting → Running** — wait-adapt returns a classified outcome (Ready/Interrupted/Killed/TimedOut); script receives it.
- **Running → Stopped** — SIGSTOP received (at delivery site, see §5) or ptrace stop ordered; AST transitions stop_state and the next script iteration parks on the stop channel.
- **Stopped → Running** — SIGCONT received or ptrace resume ordered; stop channel fires; wait-adapt returns.
- **{Running, Waiting, Stopped} → Exiting** — step_thread_exit initiated (ordinary exit, exit-on-signal, or group-exit cascade).
- **Exiting → Dead** — payload drop completes.

Waiting and Stopped are both "parked in wait-adapt." The distinction is *what they're waiting on* — Waiting is a step-initiated wait on a subsystem channel; Stopped is a thread-runtime-initiated wait on the stop channel. Same mechanism; different origin.

### 3.3 Monotonicity
<!-- txdoc:THREAD-3-3-MONOTONICITY -->

Thread states are monotone in one direction only: once Exiting is entered, the thread cannot return to Running/Waiting/Stopped. The Running ↔ Waiting ↔ Stopped transitions are reversible in the ordinary run of events; the Exiting transition is terminal.

This matches the general projection-monotonicity discipline (PRED-5): `ThreadIdentity.payload.is_some()` is monotone true-to-false.

---

## 4. The thread-as-future model
<!-- txdoc:THREAD-4-THE-THREAD-AS-FUTURE-MODEL -->

### 4.1 Shape
<!-- txdoc:THREAD-4-1-SHAPE -->

A thread is realized as **one reactor task carrying a thread-runtime-authored future**. The loop is **enter-then-await** per iteration: each iteration first opens the next userspace-run wait, runs the AST/signal checkpoint, builds the merged trap-frame context, dives into userspace via the platform's `enter_userspace_with_context`, and only then awaits the (typically already-resolved) wait to receive the trap.

```rust
async fn thread_future(payload: Cap<ThreadPayload>) -> ExitStatus {
    loop {
        // (1) Open the next userspace-run wait. The trap shell will
        //     resolve this wait via `complete_interesting_trap` when
        //     the upcoming user trap arrives.
        let entry_wait = open_userspace_run_wait(&payload);

        // (2) AST checkpoint (§5.4 site B). Signals, exit, stop are
        //     evaluated here, before userspace entry.
        handle_pending_ast(&payload).await;

        // (3) Build the merged trap-frame context (drains
        //     pending_syscall_return into the a0 slot — Plan B
        //     writeback discipline; this is the SINGLE site that
        //     mutates the user-visible register file) and dive into
        //     userspace. The platform's `enter_userspace_with_context`
        //     sret's into user mode and *returns* to this call site
        //     when the trap shell returns `TrapAction::Reschedule`
        //     after resolving the wait. Timer preemption during
        //     userspace is transparent (REACTOR_v0 §Preemption).
        let ctx = prepare_userspace_entry_payload(&payload);
        Hal::enter_userspace_with_context(ctx);

        // (4) Await the resolved wait (Poll::Ready immediately on a
        //     real platform; Pending in host smokes that don't
        //     simulate the trap-shell longjmp).
        let trap_info = entry_wait.await;

        // (5) Run the trap's handler as a script (syscall dispatch,
        //     fault handler). Side-effects land in
        //     `pending_syscall_return` (syscall return arm) or in
        //     the AddressSpace (fault-script arm); the next
        //     iteration's prepare_* drains them.
        match trap_info {
            Syscall(req) => dispatch_syscall(req, &payload).await,
            PageFault(info) => dispatch_fault(info, &payload).await,
            Fatal(_) => break run_fatal_sequence(payload).await,
        }

        if exit_requested(&payload) {
            break run_exit_sequence(payload).await;
        }
    }
}
```

The exact shape is implementation detail. What matters at the architecture level:

- The future's outer loop is **one iteration per interesting trap** (syscall, fault, fatal). Timer preemptions do not advance the loop.
- The loop is **enter-then-await**, not await-then-dispatch. Iteration 1 enters userspace before any await; the wait is awaited only after `enter_userspace_with_context` returns. This shape is necessary because a stackless coroutine can never be polled to advance through the divergent userspace round-trip — the platform's reschedule longjmp brings control back through `enter_userspace_with_context`'s normal function return.
- Each iteration invokes a trap-dispatch script (a per-syscall driver for syscall traps, a fault handler for page faults, etc.); the script composes step invocations with wait-adapt waits.
- Between script completion and the next userspace entry, AST events are consumed at the top of the next iteration (site B per §5.4).
- The loop terminates when exit is requested (`exit`, `exit_group`, or fatal signal).

### 4.2 Ownership
<!-- txdoc:THREAD-4-2-OWNERSHIP -->

Per REACTOR_v0 and the pinned positions in §Q2:

- **The reactor owns the future.** Submission hands the future to the reactor; the reactor places it in its runqueue/task table; the reactor polls it and eventually drops it.
- **`ThreadPayload.task: TaskHandle`** holds the reactor's handle on the task. This is thread-runtime's only means of addressing its own reactor task (e.g., to trigger cancellation).
- **The future holds `Cap<ThreadPayload>`.** This retention keeps `ThreadPayload` alive for as long as the future is live. It is released when the future resolves or is dropped by the reactor.

The circularity is only apparent: `ThreadPayload` holds a `TaskHandle` (not a `Cap` on the future); the future holds a `Cap<ThreadPayload>`. Drop is unidirectional — thread exit initiates payload drop from the identity side; the future's independently-held Cap keeps payload alive until the reactor drains the future.

### 4.3 Why reactor ownership
<!-- txdoc:THREAD-4-3-WHY-REACTOR-OWNERSHIP -->

Alternatives were considered and rejected:

- **ThreadPayload owns future.** Creates a retention cycle (payload → future → Cap<payload>) or requires unsafe self-reference. Rejected.
- **Split ownership with explicit Arc.** Adds a refcount layer without simplifying anything. Rejected.

Reactor ownership gives a clean story: the reactor is the runqueue; the runqueue contains futures; when a future resolves or is dropped, its held Caps release.

### 4.4 Task drain on exit
<!-- txdoc:THREAD-4-4-TASK-DRAIN-ON-EXIT -->

When `step_thread_exit` runs:

1. `exit_status` is written to `ThreadIdentity`.
2. `ThreadIdentity.payload` is set to `None` (dropping thread-runtime's direct `PayloadCap<ThreadPayload>`).
3. `TaskHandle` is consumed to signal the reactor that the task should drain.

The reactor, on receiving drain-signal (mechanism not specified by REACTOR_v0; may be a reactor-internal event), polls the future once more or observes that it has already returned. The future drops. Its `Cap<ThreadPayload>` releases. Payload reclamation proceeds.

**A thread's future must not block-forever after drain-signal.** This is a discipline on thread-runtime: once exit is initiated, the future must reach termination in bounded steps. In practice, the future's outer loop exits on `exit_requested` detection.

### 4.5 Composition: thread_future ⊃ scripts ⊃ steps
<!-- txdoc:THREAD-4-5-COMPOSITION-THREAD-FUTURE-SCRIPTS-STEPS -->

The thread's kernel-side execution is a composition of three distinct runtime entities, at three different scales. Understanding their layering is essential because they have different policies for what can yield, what saves state, and how preemption interacts with them.

**thread_future** (this section) is the long-lived reactor task. Its state machine lives for the thread's lifetime. Its `.await` points are exactly the boundaries between kernel-side activities — await on `request_userspace_run` for userspace execution, await on `dispatch_trap` for syscall handling, etc.

**Scripts** (per SUBSYSTEM_ANATOMY §6; per-syscall async fns like `script_read`, `script_execve`, etc.) are short-lived futures. Each script runs for the duration of one syscall or fault handler. Scripts are async fns; they compose steps and reactor waits. A script's state machine is nested inside thread_future's state machine — when thread_future `.await`s `dispatch_trap(trap_info, &payload)`, the script's state machine is the future being awaited.

**Steps** (STEP-2; synchronous functions returning `StepOutcome<T>`) are not futures at all. A step runs in a single synchronous call; it does not `.await`. Scripts invoke steps as ordinary function calls and receive the StepOutcome immediately.

The composition:

```
reactor polls thread_future
    thread_future awaits dispatch_trap (a script future)
        script future awaits reactor::wait (between steps)
            reactor::wait registers waker, returns Pending
        (yield propagates up: script → thread_future → reactor)
    (reactor marks task not-runnable)
```

When a waker fires, the reactor polls thread_future again. The compiler-generated state machines resume at every nesting level from saved points. Locals across `.await` are preserved by the state machines themselves (no kernel stack; no context save/restore; just structured resumption).

**Saved thread-side state is thus in two physical places:**

- **`ThreadPayload` fields** — atomic or single-field data read or updated by other parts of the kernel: `signal_mask`, `signal_summary`, `thread_pending`, `stop_state`, `regs` (userspace context), `child_tid_clear`. These are accessed by signal producers (via `deliver_posix_signal` per SIGNAL_v1 §12), by the reactor (regs save/restore at dispatch), by procfs (projections). Their visibility to other code is the reason they live on payload rather than inside the future.

- **The future's state machine** (owned by the reactor) — in-kernel async execution state: which script is running, which `.await` it is at, what reactor wait is pending, the current step's locals (if any). This state is private to the task and mutated only when the reactor polls.

`ThreadPayload.task` is the reactor's opaque handle on the task. Thread-runtime references the reactor's future through this handle; it does not access the future's memory directly.

#### Preemption interaction

Timer-based preemption during userspace execution (per REACTOR_v0) is **transparent to all three layers of this composition**:

- thread_future's state machine does not advance (it is at `.await` of `request_userspace_run`).
- No script is active during userspace (scripts run between userspace traps, not during).
- No step is executing.

Between any two polls of thread_future, the reactor may grant userspace any number of slices on any number of harts. The state machine is untouched throughout.

This is the same property observed in Linux and Zircon: userspace preemption is scheduler machinery, invisible to the thread from the kernel side.

#### Fatal-signal termination during a script

If a fatal signal is posted while a script is awaiting a reactor wait:

1. `check_interrupt` at wait resolution time (per STEP-3 / §5.3) returns `Killed`.
2. The script's state machine receives `Killed`; its matching code returns an error (`Err(EINTR)` or similar).
3. The error propagates up through the script's state machine via ordinary `?` or match-return paths.
4. The script's future resolves with an error value.
5. thread_future's `.await` on `dispatch_trap` completes with this error.
6. thread_future writes the error to userspace regs (it will not reach userspace — the next iteration's `handle_pending_ast` will observe termination and initiate exit).
7. Alternatively, thread_future's outer-loop AST handling detects termination immediately and initiates `step_thread_exit` at the loop head.

Unwinding is via state-machine resolution with error values, **not** via stack unwinding. There is no kernel stack to unwind (stackless coroutines). Drop impls on the state machine's held resources (Caps, pending wait registrations, reservations) release them as the state machine resolves.

This matches the general reservation-rollback discipline: uncommitted reservations drop cleanly; committed mutations persist; fatal termination between steps is a clean step boundary.

#### Why this layering

- **Steps must be synchronous** (STEP-2) so that they run as atomic transitions observable from the outside only at their completion. A step that yielded could leave partial state visible.
- **Scripts must be async** so that they can compose multiple synchronous steps with intervening waits for external events (signals, I/O completion, wakeups).
- **thread_future must be a long-lived async** so that the thread's entire kernel-side activity is one reactor task with one identity, across the lifetime of the thread.

The three layers are not interchangeable. Collapsing any two produces either a blocking-in-step violation or a loss of the per-syscall composition boundary.

---

## 5. Signal delivery mechanics
<!-- txdoc:THREAD-5-SIGNAL-DELIVERY-MECHANICS -->

This section specifies *how signals become observable userspace events* — not their catalog, not their default actions, not their interaction with ptrace. Those live in the signal spec and observation spec respectively.

### 5.1 State placement
<!-- txdoc:THREAD-5-1-STATE-PLACEMENT -->

Per the pinned positions:

| State | Location | Rationale |
|---|---|---|
| `signal_mask` | `ThreadPayload` | Execution-delivery state; no post-exit semantics |
| `thread_pending` | `ThreadPayload` | Thread-directed queue; dies with thread |
| `group_pending` | `ProcessPayload` | Process-directed queue; survives individual threads |
| `sig_actions` | `Frame` (on `ProcessPayload`) | Dispositions shared across thread group per CLONE_SIGHAND; `Shared<SigActionTable>` for COW semantics (per PROCESS_v1 §3) |
| `signal_summary` | `ThreadPayload` | Atomic; read by wait-adapt |

### 5.2 The interrupt summary
<!-- txdoc:THREAD-5-2-THE-INTERRUPT-SUMMARY -->

`ThreadPayload.signal_summary` is an atomic value that answers, cheaply:

> Does this thread have a deliverable event that should interrupt an interruptible wait?

It is a **summary**, not the authoritative pending state. Authoritative state lives in `thread_pending`, `group_pending`, and the current mask; the summary is a denormalized view kept current by thread-runtime.

```rust
pub struct InterruptSummary {
    pub deliverable_signal: bool,   // ∃ signal in (thread_pending ∪ group_pending) \ mask
    pub termination: bool,          // SIGKILL or equivalent fatal-exit condition set
    pub stop_requested: bool,       // SIGSTOP pending, not yet stopped
}
```

It is updated by:

- `deliver_posix_signal` (per SIGNAL_v1 §12) — when a signal is queued to `thread_pending` or `group_pending`, the internal helpers (`step_post_signal` here, `post_to_group_pending` in PROCESS_v1 §7.6) set `has_deliverable`;
- `sigprocmask` (when the mask changes, a newly-unblocked pending signal sets `has_deliverable`);
- `step_thread_exit` (when termination is committed, sets `termination`);
- `deliver_posix_signal` for Gewalt routings (sets `termination` on fan-out threads for SIGKILL; sets `stop_requested` for SIGSTOP; clears on SIGCONT);
- ptrace intercepts that order stops (future observation subsystem).

Each update is a read-modify-write on the atomic summary, performed inside the step that commits the underlying change.

### 5.3 The interrupt predicate contract
<!-- txdoc:THREAD-5-3-THE-INTERRUPT-PREDICATE-CONTRACT -->

Wait-adapt, on wake, consults the interrupt summary via a predicate:

```rust
pub trait InterruptSource {
    fn deliverable_signal_pending(&self) -> bool;
    fn termination_in_force(&self) -> bool;
    fn stop_requested(&self) -> bool;
}
```

`ThreadPayload` (or a thread-runtime-owned wrapper) implements this trait. Wait-adapt receives it at wait-entry (through the `InterestMask`-bearing channel, or as a separate argument — exact shape is wait-adapt's internal matter).

The predicate is **cheap** — one atomic load plus a branch or two. Wait-adapt calls it on every wake, before returning from the wait.

### 5.4 The two-site discipline
<!-- txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE -->

Signals are evaluated at two distinct sites. This is load-bearing for correctness and for the ability of interruptible waits to unblock without racing against step code.

**Site A: wait-adapt (on wake, while parked).**

- Condition: thread is parked in wait-adapt on a `Blocked(channel, mask)` step outcome.
- Trigger: any wake (waker fire, timeout, signal-post-triggered waker).
- Action: wait-adapt calls the interrupt predicate. If `deliverable_signal && protocol.is_interruptible()`, return `Interrupted`. If `termination && protocol.is_killable()`, return `Killed`. Otherwise return `Ready` (which will cause the script to re-poll the step, which may re-block).

This is the *only* way a blocked thread observes a signal. Steps do not check signals. Scripts, while parked, are inside wait-adapt — the check is wait-adapt's.

**Site B: AST (at return-to-userspace).**

- Condition: the thread's future has finished processing a syscall and is about to return to userspace.
- Trigger: always runs between script completion and userspace re-entry.
- Action: consult the interrupt summary. If `deliverable_signal`, build a signal frame and redirect userspace entry to the handler (or deliver default action: ignore, terminate, stop). If `termination`, initiate exit. If `stop_requested`, park on the stop channel.

This is where signals arriving during a synchronous (non-blocking) portion of a syscall — or during ordinary userspace execution interrupted by a non-signal trap — get delivered.

**What about signals arriving mid-step?** Steps are synchronous and bounded (STEP-2). A signal arriving during a step's execution does not interrupt the step. The step completes, the script observes the step's outcome, and either:

- the step returned a terminal outcome — the script returns to the future's outer loop, which hits AST — site B delivers the signal; or
- the step returned `Blocked` — the script calls wait-adapt — site A delivers the signal (the waker fires because `deliver_posix_signal`'s thread-level helper wakes the task).

There is no third site because there is no third moment. This is a direct consequence of STEP-2: steps don't yield within themselves, so there's no intra-step window for signal delivery.

### 5.5 post_signal (internal helper)
<!-- txdoc:THREAD-5-5-POST-SIGNAL-INTERNAL-HELPER -->

The canonical signal-producer entry point is `deliver_posix_signal(target, signum, siginfo)` in [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md) §12. All signal production (kill, tgkill, raise, kernel-internal SIGCHLD, SIGSEGV-on-fault, timer expiry, tty input, etc.) goes through that single entry point.

`step_post_signal` is a **thread-runtime-internal helper** invoked by `deliver_posix_signal` when the target is `SignalTarget::Thread(_)` with a catchable disposition resolving to a handler. It performs the per-thread part of the work: enqueue into the thread's `thread_pending`, update `signal_summary`, wake the thread's reactor task.

```rust
// Internal helper; not an external API. Called by SIGNAL_v1's
// deliver_posix_signal when routing determines that a specific thread
// should receive a catchable signal into its per-thread pending queue.
fn step_post_signal(target: Cap<ThreadIdentity>, sig: Signum, siginfo: SigInfo) -> StepOutcome<()> {
    // 1. Observe: authorization has been done by deliver_posix_signal's caller (cred check).
    // 2. Upgrade: obtain Cap<ThreadPayload>; if payload is None (thread exiting), abort.
    // 3. Reserve: queue slot for the signal (bounded ring for RT, bitset slot for standard).
    // 4. Commit: atomically enqueue signal into thread_pending; update signal_summary.
    // 5. Publish: wake the task's reactor waker (if thread is parked).
}
```

The waker wake is what causes a parked thread to re-enter wait-adapt's check. Without the waker fire, a blocked thread would not observe the new signal until some unrelated wake event. The fire guarantees timely delivery.

If the target thread is not parked — running, stopped, exiting — the wake is a no-op (the task is not in a wait). The signal is still queued; it will be observed at the next AST kernel→user transition (if running) or when the thread resumes (if stopped).

**Analogous internal helpers** for other target kinds are in PROCESS_v1 §7.6 (`post_to_group_pending`, `process_group_fanout`). All are invoked by `deliver_posix_signal` after it performs routing (classification as Gewalt vs catchable, disposition lookup, target-kind dispatch). The single external API is `deliver_posix_signal`.

### 5.6 Signal target selection for process-directed signals
<!-- txdoc:THREAD-5-6-SIGNAL-TARGET-SELECTION-FOR-PROCESS-DIRECTED-SIGNALS -->

`kill(pid)` targets the process; the kernel must choose a thread within the process to handle the signal. v1 rule:

> A process-directed signal is handled by some thread in the group whose mask does not block the signal. If all threads in the group mask the signal, it remains in `group_pending` until some thread unmasks.

Selection is made at site B for whichever thread next hits AST (or at site A if that thread is currently parked interruptibly). Multiple threads need not race — each thread, on its next delivery-site evaluation, observes group_pending and attempts to claim the signal via atomic dequeue. First claim wins; others see the queue empty.

The full Linux dispatch policy (prefer threads that have unmasked vs. will unmask, fairness, etc.) is a later refinement. v1's "first claimant wins" is sufficient for POSIX correctness.

---

## 6. Stop state
<!-- txdoc:THREAD-6-STOP-STATE -->

Stop semantics in POSIX are **process-scoped**: `SIGSTOP` stops all threads in the process; `SIGCONT` resumes them all. The kernel models this with a per-thread parked wait, because each thread is an independent reactor task that must individually park. But the *trigger* is group-scoped: the signal delivery pass observes the process-directed stop/continue and cascades it to every thread in the group.

ptrace is different: ptrace stops and resumes target a specific tid, bypassing the process-scoped cascade. Both mechanisms share the same underlying stop-channel machinery on the thread side.

### 6.1 The stop channel
<!-- txdoc:THREAD-6-1-THE-STOP-CHANNEL -->

Each thread has a thread-runtime-owned **stop channel** (bus carrier). The channel is per-thread because each thread must individually park and be individually woken. The channel is fired by:

- process-scoped `SIGCONT` cascade (one fire per thread in the group);
- per-thread `ptrace_continue` (single fire at the targeted thread);
- `step_thread_exit` with termination (to unpark the thread for exit processing).

It is awaited by:

- the thread's own outer loop when it transitions to Stopped at site B.

### 6.2 Stop state transitions
<!-- txdoc:THREAD-6-2-STOP-STATE-TRANSITIONS -->

`stop_state: AtomicStopState` on `ThreadPayload` has values:

- **Running** — thread is not stopped.
- **StopPending** — a stop has been ordered for this thread (via group cascade or ptrace); thread has not yet reached a site-B pass.
- **Stopped** — thread is parked on its stop channel.
- **ContinuePending** — a continue has been ordered; stop channel has been fired; thread has not yet fully resumed.

The step that realizes each transition:

- `deliver_posix_signal(target, SIGSTOP, ...)` with `target` a Process or ProcessGroup: Gewalt routing (per SIGNAL_v1 §12.1) fans out over the group's threads, flipping each thread's `stop_state` Running → StopPending, setting `signal_summary.stop_requested`, and waking each task. The fan-out is a single process-level step iterating the thread DLL; publication fires per-thread wakers during the commit phase. SIGSTOP is uncatchable; sig_actions is not consulted.
- Site B observes `stop_requested`: the thread's own outer loop transitions `stop_state` StopPending → Stopped and parks on its stop channel.
- `deliver_posix_signal(target, SIGCONT, ...)`: similarly fans out — for every thread in the group in `stop_state ∈ {StopPending, Stopped}`, transition to ContinuePending and fire the thread's stop channel. If the process has a handler installed for SIGCONT, it also routes into `group_pending` for post-wake handler delivery.
- The thread's parked wait on the stop channel returns; `stop_state` ContinuePending → Running at the top of the next outer-loop iteration.

The catchable stop signals (SIGTSTP, SIGTTIN, SIGTTOU) route through `deliver_posix_signal` the same way SIGTERM does: if a handler is installed, enqueue into pending for handler delivery; if SIG_DFL, the default action is Stop, which invokes the same fan-out path as SIGSTOP; if SIG_IGN, drop.

ptrace stop/continue follows the same stop_state machinery but targets a single thread:

- `step_ptrace_stop(tid)`: flip that thread's stop_state to StopPending; wake.
- `step_ptrace_continue(tid)`: fire that thread's stop channel.

The stop_state enum value is the same whether the order came from a process-scoped signal or from ptrace; what differs is only which thread(s) were targeted and by which step.

### 6.3 Observability
<!-- txdoc:THREAD-6-3-OBSERVABILITY -->

While any thread is in Stopped state, it is visible to `waitid(WUNTRACED)` / `wait4` on the process, and to ptrace on its tid. Observation mechanism: `ThreadPayload.stop_state` for direct reads, process-level aggregated state for wait, and per-observer bookkeeping in the observation subsystem for ptrace. `ThreadIdentity.payload.is_some()` remains true in Stopped — a stopped thread is still addressable for signal delivery and ptrace commands.

Whole-process stop is observable when *all* threads in the group are Stopped; a process with some threads running and some stopped is in a transient state and should resolve quickly (stop cascades are not throttled).

### 6.4 Why stop is not a reactor primitive
<!-- txdoc:THREAD-6-4-WHY-STOP-IS-NOT-A-REACTOR-PRIMITIVE -->

Stop is realized as a wait on a thread-runtime-owned channel, not as a reactor "freeze task" API. The reactor sees a blocked task. The thread-runtime's view is that the task is stopped. Both are consistent.

This keeps REACTOR_v0 untouched. If stop needed reactor support, the contract would need to grow; it does not. The two-site delivery discipline plus per-thread stop channels is sufficient to implement POSIX stop/continue semantics without special reactor cases.

---

## 7. Thread exit
<!-- txdoc:THREAD-7-THREAD-EXIT -->

### 7.1 Exit triggers
<!-- txdoc:THREAD-7-1-EXIT-TRIGGERS -->

A thread exits when:

- **`exit(status)`** is called — ordinary thread exit (or whole-process exit if last thread).
- **`exit_group(status)`** is called — group-exit cascade; all threads in the group exit.
- **Fatal signal** is delivered — termination-by-default-action (SIGKILL, uncaught SIGSEGV, etc.).
- **Parent process exit** triggers reparent; does not directly cause thread exit but may subsequently trigger exit_group.

### 7.2 step_thread_exit
<!-- txdoc:THREAD-7-2-STEP-THREAD-EXIT -->

```rust
fn step_thread_exit(thread: Cap<ThreadIdentity>, status: ExitStatus) -> StepOutcome<()> {
    // Phase 1: observe — acquire payload cap
    // Phase 2: upgrade
    // Phase 3: reserve — any needed zone reservations
    // Phase 4: commit:
    //   - write exit_status to ThreadIdentity
    //   - update signal_summary for any waiters on thread death (e.g., waitid)
    //   - if child_tid_clear is Some:
    //       write 0 to *child_tid_clear (best-effort; ignore failure)
    //       issue FUTEX_WAKE on child_tid_clear (wake count 1)
    //   - transition payload: Some(PayloadCap) → None (drop thread-runtime's direct retention)
    //   - consume TaskHandle to signal reactor drain
    //   - fire thread-exit wake events (parent waitid, tracer wait)
    // Phase 5: publish
    //   - tracepoint
}
```

The child_tid_clear step implements the exit-notification mechanism described in §2.6. Ordering: the clear-and-wake happens before the payload transition to None, so that pthread_join waiters observing the cleared tid see a consistent snapshot (the thread is definitively exiting by the time they're woken).

After commit:

- Thread is Exiting/Dead from thread-runtime's view.
- `ThreadIdentity.payload == None`; tgkill to this tid fails with ESRCH.
- Reactor still holds the future; drain is in progress.
- Parent waitid on this tid can now resolve.

### 7.3 Last-thread-exit → process exit
<!-- txdoc:THREAD-7-3-LAST-THREAD-EXIT-PROCESS-EXIT -->

When the last thread in a group exits, the process must also exit. This is not a thread-runtime concern *internally*, but it is thread-runtime's responsibility to trigger it: at `step_thread_exit`, if the thread group now has zero running threads, thread-runtime invokes the process-exit step.

Exact trigger mechanism is a PROCESS_v1 concern; what thread-runtime commits to is that the trigger fires synchronously within the last-thread exit step's commit phase.

### 7.4 exit_group
<!-- txdoc:THREAD-7-4-EXIT-GROUP -->

`exit_group(status)` semantically exits all threads in the group. In implementation (see PROCESS_v1 §5 for GroupExit coordination):

1. The calling thread CASes `ProcessPayload.group_exit.state` from None to Some(GroupExitState{status, ...}). If the CAS fails (another thread is already initiating): this thread is not the initiator; it joins the collapse as a non-initiator.
2. For every other thread in the group, the initiator sets `signal_summary.termination = true` and fires its reactor waker. This is the Gewalt-termination fan-out — not a catchable-signal post; it bypasses sig_actions and pending queues.
3. Each other thread, at its next delivery site (A or B), observes termination-in-force, initiates its own `step_thread_exit`.
4. Each non-initiator thread, in its `step_thread_exit` commit phase, decrements `group_exit.remaining_threads`. When it hits zero, the completion_channel waker fires.
5. The initiator, after completing step 2, waits on completion_channel. On wake (all non-initiators exited), it proceeds to its own `step_thread_exit`, which triggers `step_process_exit` as the last thread.

This is a compositional multi-step operation. No single step commits the whole cascade; each thread's exit is a separate step sharing the group-exit coordination state.

**On the relationship to SIGKILL:** `kill(pid, SIGKILL)` from another process follows the same flow (`deliver_posix_signal` with SIGKILL invokes GroupExit initiation per SIGNAL_v1 §12.1). The only difference is origin: exit_group is self-initiated with an explicit status; SIGKILL is externally-initiated with status encoding the signal. GroupExit's coordination machinery handles both identically after initiation.

### 7.5 Fatal signal → exit
<!-- txdoc:THREAD-7-5-FATAL-SIGNAL-EXIT -->

When site B observes a signal whose default disposition is terminate (and no handler is installed), it initiates `step_thread_exit` with the signal-derived status. If the signal is process-directed and its default is core-dump-group-terminate (SIGSEGV et al.), this becomes an exit_group cascade.

---

## 8. The thread-future skeleton in detail
<!-- txdoc:THREAD-8-THE-THREAD-FUTURE-SKELETON-IN-DETAIL -->

Pulling §4 and §5 together, here is the thread's outer loop in more detail:

```rust
async fn thread_future(payload: Cap<ThreadPayload>) -> ExitStatus {
    'outer: loop {
        // --- SITE B: AST / delivery at userspace re-entry ---
        loop {
            let summary = payload.signal_summary.load();

            if summary.termination {
                // Fatal: exit path.
                break 'outer run_exit_sequence(payload).await;
            }

            if summary.stop_requested {
                // Stop: park on stop channel.
                payload.stop_state.transition_to_stopped();
                let _ = reactor::wait(
                    payload.stop_channel(),
                    StopMask::CONTINUE,
                    WaitProtocol::Killable,
                ).await;
                payload.stop_state.transition_to_running();
                continue;
            }

            if summary.deliverable_signal {
                // Deliverable signal: dequeue, build frame, redirect userspace entry.
                deliver_next_signal(&payload);
                // Loop back: delivery itself may have mutated summary.
                continue;
            }

            break; // Nothing to deliver; fall through to userspace re-entry.
        }

        // --- USERSPACE EXECUTION ---
        //
        // Per REACTOR_v0 §Preemption: timer-based preemption during userspace
        // is reactor-internal and transparent to this await. The await resolves
        // only on an interesting trap (syscall, page fault, fatal hardware).
        let trap_info = reactor::request_userspace_run(&payload).await;

        // --- SCRIPT EXECUTION (steps + wait-adapt) ---
        //
        // Inside the script, wait-adapt handles SITE A (interruption on wake).
        // If wait-adapt returns Interrupted, the script unwinds with EINTR /
        // partial progress per syscall semantics; control returns here.
        // If wait-adapt returns Killed, the script propagates termination;
        // the next SITE B pass will observe termination and exit.
        dispatch_trap(trap_info, &payload).await;

        // Top of loop: SITE B evaluates again before next userspace entry.
    }
}
```

Properties this shape guarantees:

- **Every userspace entry is preceded by a site-B pass.** A pending signal cannot be ignored past a userspace transition.
- **Every blocked wait is monitored at site A.** Wait-adapt's check fires on every wake.
- **Steps are never interrupted mid-execution.** The two sites bracket step execution; no intra-step delivery is possible.
- **Exit and stop are handled by the same delivery machinery.** No separate "check for exit" pass; termination and stop-request live in the same summary.
- **Timer preemption is invisible to this loop.** Per REACTOR_v0 §Preemption, timer-based preemption of userspace is reactor-internal; the outer loop sees only interesting traps. A thread may have its userspace execution fragmented across many scheduler slices without the loop iterating — each loop iteration corresponds to one interesting trap.

---

## 9. Interaction with scripts
<!-- txdoc:THREAD-9-INTERACTION-WITH-SCRIPTS -->

This section restates script rules from the thread-runtime vantage point. Nothing new is claimed; the point is to make the contract visible from the thread side.

- **Scripts are state-blind (SCRIPT-2).** Scripts do not read signal state. The two sites handle signals; scripts do not.
- **Scripts on `Blocked` invoke wait-adapt.** Scripts do not invent waits; they call the reactor service. Wait-adapt handles site A.
- **Scripts on `Err(EINTR)` from wait-adapt propagate errno.** The script does not decide what EINTR means — POSIX semantics for the specific syscall govern whether partial progress is reported, whether EINTR is returned, or whether the syscall is restartable (ERESTARTSYS).
- **Scripts on `Err(EKILLED)` from wait-adapt unwind.** Termination in force means the thread is exiting; the script returns without further steps; the outer loop catches termination at site B and exits.

The script layer is thread-agnostic in the sense that it doesn't know the interruption came from a signal vs. timeout vs. wake-then-predicate-still-false. It sees classified wait outcomes and translates them.

---

## 10. Cross-doc corrections implied by this document
<!-- txdoc:THREAD-10-CROSS-DOC-CORRECTIONS-IMPLIED-BY-THIS-DOCUMENT -->

The following prior docs have state placement inconsistent with the pinned positions in this doc. These should be corrected in subsequent passes:

- **Cred doc v1.2.** `ProcessPolicy.signal_mask: SigMask` — incorrect; signal_mask is per-thread and lives on `ThreadPayload`. Remove from cred's `ProcessPolicy` sketch.
- **Archived architecture drafts.** `PolicyBag` was shown with `signal_mask` alongside `cred` and `rlimits` — same issue; those drafts pre-date the thread-runtime split.

No deep restructuring is required — both are one-line corrections. They happen when the process-policy surface is finalized.

---

## 11. POSIX alignment
<!-- txdoc:THREAD-11-POSIX-ALIGNMENT -->

THREAD_RUNTIME_v1 commits to the **placement and runtime mechanics** that make POSIX-compatible signal delivery and thread lifecycle possible. It does **not** yet claim the full POSIX signal surface. This section makes that split explicit so readers are not left guessing whether gaps are intentional or accidental.

### 11.1 What this document commits to (POSIX-aligned)
<!-- txdoc:THREAD-11-1-WHAT-THIS-DOCUMENT-COMMITS-TO-POSIX-ALIGNED -->

- **Per-thread signal mask.** `sigprocmask` modifies the calling thread's mask only. `pthread_sigmask` has identical semantics. Mask lives on `ThreadPayload`.
- **Per-thread pending queue.** `tkill` and `tgkill` target a specific thread; the signal enqueues to that thread's `thread_pending`. Handled at the targeted thread's next site-A or site-B pass.
- **Process-shared pending queue.** `kill(pid)` enqueues to `ProcessPayload.group_pending`; the first thread in the group whose mask does not block the signal and which reaches a delivery site claims it. Matches POSIX's "some thread in the group" rule.
- **Process-shared signal dispositions.** `sigaction` modifies a shared table (`ProcessPolicy.sig_actions` or equivalent process-attached structure). All threads in the group see the same dispositions. `SA_SIGINFO` / handler-vs-default selection is disposition-owned, not mask-owned.
- **Two-site delivery.** Signals become userspace-observable only at site A (wait-adapt, while parked) or site B (AST, at return-to-userspace). No intra-step delivery.
- **Blocked-wait interruption.** `EINTR` is returned through wait-adapt's classified outcome, not by step or script introspection. Scripts translate to POSIX errnos per syscall semantics.
- **Process-scoped stop and continue.** `SIGSTOP` / `SIGTSTP` / `SIGTTIN` / `SIGTTOU` stop all threads in the group; `SIGCONT` resumes all. Realized per §6: group-scoped trigger, per-thread parked wait on stop channel.
- **Fatal-signal termination.** Uncaught signals with `default = Terminate` action initiate `step_thread_exit` (or `exit_group` cascade for process-wide fatal signals) at site B.
- **Thread exit bypasses group exit.** `pthread_exit` / `exit(2)` from a non-leader thread terminates only that thread; `exit_group(2)` terminates the whole group. POSIX-equivalent.
- **Thread exit notification via CLONE_CHILD_CLEARTID.** Clone registers an optional userspace address; thread exit clears it and issues FUTEX_WAKE. `set_tid_address(2)` updates the registered address at any time. Supports pthread_join (§2.6).

### 11.2 What was deferred at draft time — now specified by SIGNAL_v1
<!-- txdoc:THREAD-11-2-WHAT-WAS-DEFERRED-AT-DRAFT-TIME-NOW-SPECIFIED-BY-SIGNAL-V1 -->

When THREAD_RUNTIME was initially drafted, the full POSIX signal semantics were left for a later spec. That spec is now [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md). The items below were placement-compatible but not fully realized in THREAD_RUNTIME; all are now specified in SIGNAL_v1:

- **Realtime signal FIFO queue order.** SIGNAL_v1 §8 specifies `PendingSignalQueue` as a composite — bitset + per-signum siginfo slot for standard signals (merge-on-duplicate, keep-first), per-signum FIFO ring for RT signals with siginfo preservation.
- **`SA_RESTART` and ERESTARTSYS.** SIGNAL_v1 §15 and §19 specify the internal errno codes (ERESTARTSYS, ERESTARTNOHAND, ERESTARTNOINTR, ERESTART_RESTARTBLOCK) and the HAL trap-PC-rewind mechanism invoked on handler return.
- **`sa_mask` and `SA_NODEFER`.** SIGNAL_v1 §15.4 and §18 specify mask computation at handler entry (old_mask ∪ sa_mask ∪ {this signal unless SA_NODEFER}) and restoration at sigreturn via the saved signal frame.
- **`sigreturn` as a syscall.** SIGNAL_v1 §17 specifies the full sigreturn path: frame layout (abstract; HAL owns per-arch details), context restore, mask restore, stack trampoline (Phase 1) and VDSO entry (Phase 2 upgrade).
- **Synchronous-fault signal delivery path.** SIGNAL_v1 §20 specifies `deliver_synchronous_fault` as a distinct entry point from `deliver_posix_signal`, bypassing the pending queue and applying mask-bypass rules (force-Term for uncaught synchronous faults regardless of mask). The HAL's fault handler invokes this in the trap context of the faulting thread.
- **Full disposition-to-action semantics.** SIGNAL_v1 §6 catalogs default actions per signal; §12 specifies the routing algorithm per disposition (Handler / SigInfoHandler / Default / Ignore); §17.2, §17.3 detail SIGCHLD-on-stop and SIGCHLD-on-continue with SA_NOCLDSTOP semantics.

THREAD_RUNTIME provides the state placement (ThreadPayload's signal fields, ProcessPayload's group_pending, Frame's sig_actions); SIGNAL_v1 specifies how those fields are written to and consulted during signal delivery.

### 11.3 What readers can now assume
<!-- txdoc:THREAD-11-3-WHAT-READERS-CAN-NOW-ASSUME -->

With SIGNAL_v1 specified, all POSIX signal semantics named in §11.2 are committed. Subsystems integrating with signal delivery should cite SIGNAL_v1 directly for:

- Realtime signal ordering (SIGNAL_v1 §8, §11).
- SA_RESTART / ERESTARTSYS mechanics (SIGNAL_v1 §15, §19).
- Synchronous fault mask-bypass (SIGNAL_v1 §20).
- sigreturn path (SIGNAL_v1 §17).
- Default-action catalog (SIGNAL_v1 §6).
- `deliver_posix_signal` entry point (SIGNAL_v1 §12).
- `KernelTrapSink<P>` plus `SignalFrameIf` for HAL signal integration (SIGNAL_v1 §15).

THREAD_RUNTIME remains the authority on state placement (§5.1), thread state machine (§3), and wait-adapt integration (§5.3). SIGNAL_v1 references these; producer subsystems should cite both when needed.

---

## 12. What this document does not specify
<!-- txdoc:THREAD-12-WHAT-THIS-DOCUMENT-DOES-NOT-SPECIFY -->

- **Full signal catalog and dispositions.** Specified in [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md) §6.
- **sigaction, sigprocmask, sigsuspend syscall details.** Specified in SIGNAL_v1 §20 (sigaction), §21 (sigprocmask), §22 (sigsuspend).
- **ptrace runtime mechanics.** Three intercepts, tracer-tracee relationship, stop-notification delivery. Observation spec (future).
- **Scheduler policy.** Specified in [`SCHEDULER_v0.md`](./SCHEDULER_v0.md).
- **execve interaction with threads.** Non-leader thread termination, exec-by-non-leader re-leadership, CLONE_FILES/CLONE_SIGHAND-on-exec resolution. PROCESS_v1 §7.2 (leader-only; non-leader exec deferred to Phase 2).
- **Thread-local storage (TLS).** ABI concern; architecture-specific setup lives in HAL / startup.
- **Performance of signal-wake paths.** Implementation concern.

---

## 13. Open questions
<!-- txdoc:THREAD-13-OPEN-QUESTIONS -->

- **How many interrupt-summary fields?** Current proposal has three (deliverable_signal, termination, stop_requested). Whether other wait-interrupting conditions (e.g., ptrace-single-step ready) warrant their own bit or piggyback on existing ones is not pinned.
- **Signal queue data structure.** thread_pending and group_pending are shown as `PendingSignalQueue`; structure settled in SIGNAL_v1 §8 (bitset + per-signum siginfo slot for standard; per-signum bounded FIFO ring for RT). Question resolved.
- **Wake-on-signal granularity.** `deliver_posix_signal` currently wakes the task unconditionally if parked; whether mask-aware wake suppression (don't wake if the signal is masked) is worth the complexity is a later optimization.
- **Thread-group-leader specifics.** Whether the leader thread has any distinguished state beyond being first-created is a PROCESS_v1 question. This doc treats all threads symmetrically.

---

## Short version
<!-- txdoc:THREAD-SHORT-VERSION -->

> A thread is one reactor task carrying a thread-runtime future, plus a `ThreadIdentity`/`ThreadPayload` pair that carries its semantic state. The reactor owns the future; `ThreadPayload` holds a task handle. Signals are delivered at two sites: wait-adapt (on wake, while parked) and AST (at return-to-userspace). Both sites consult a thread-runtime-owned interrupt summary. Per-thread signal state (mask, pending, summary) lives on `ThreadPayload`. Process-directed pending signals live on `ProcessPayload.group_pending`. Stop is realized as a wait on a thread-runtime-owned stop channel, preserving REACTOR_v0's contract without modification. Thread exit drops payload from identity, signals reactor drain, and defers physical reclamation to the drain's completion.
