# Reactor — v0 Contract

<!-- txdoc:02-EXECUTION-REACTOR-V0 -->

## Status
<!-- txdoc:REACTOR-STATUS -->

Draft v0.3.

This document pins the **reactor boundary** that the rest of the txKernel architecture has already been implicitly depending on. It is a contract document, not a full specification:

- It names what the reactor provides, at the level other documents need to cite.
- It names what the reactor refuses to provide, so other documents do not quietly reinvent those responsibilities.
- It does **not** specify scheduler policy, runqueue structure, idle-loop behavior, wake aggregation, per-CPU state, or any other implementation-layer concern.

The larger semantic story — thread state machine, in-flight future ownership, signal-interrupt mechanics across blocked waits, ptrace runtime stops, and per-thread vs. process-shared signal state — lives in later documents (THREAD_RUNTIME_v1, PROCESS_v1, observation) and is explicitly out of scope here.

---

## Purpose
<!-- txdoc:REACTOR-PURPOSE -->

REACTOR_v0 exists because several other documents already cite a reactor boundary as though it were specified:

- `SUBSYSTEM_ANATOMY` lists `scheduler, wait/submit, ast, sync_coord` as reactor-owned and classifies substrate as knowing nothing about subsystems or semantics;
- `BUS_v1` states explicitly that wakers are reactor-provided and that the bus does not schedule, sleep, or own timers;
- `STEP_MODEL_v1` assumes classified wait outcomes (`Interrupted`, `Killed`, `TimedOut`) the step model itself does not provide;
- `ADR-resolution-half` declares coroutines to be the exchange object between semantic code and reactor and names userspace threads as reactor tenants;
- `INVARIANTS` / `CONCEPTS` name wait-adapt as a reactor-provided service (SCRIPT-4) and require scripts to remain state-blind (SCRIPT-2).

Writing this contract stops those references from being implicit. Other documents may now cite REACTOR_v0 rather than speculating about what the reactor does.

---

## Position in the architecture
<!-- txdoc:REACTOR-POSITION-IN-THE-ARCHITECTURE -->

The reactor is one of the four runtime roles (per CONCEPTS §2.5):

| Role | Responsibility |
|---|---|
| step | Construct one candidate transition observer-safely. |
| reactor | **Schedule coroutines, mediate wait/wake, own AST and cross-core sync carve-outs.** |
| semantics | Own objects; define legal transitions. |
| signifier | Reverse-map userspace names to object witnesses. |

The reactor is **infrastructure**, not a semantic owner. It does not own any entity with projections, retention, or zombie semantics. It spans the semantic / execution / publication planes without owning any of them: it is the machinery that lets scripts compose transitions across time without being asked to solve waiting themselves.

**Zone-derived type policy.** Reactor tasks are temporal handles, not
zone-backed semantic entities. The reactor may hold role-shaped evidence such
as `Cap<ThreadPayload>` when running a userspace thread future, but the owner
of that cap is THREAD_RUNTIME. `TaskId`, `TaskHandle`, wakers, wait tokens, and
AST bookkeeping do not derive `Cap<T>`, `Weak<T>`, or payload evidence and do
not select zone policies.

The reactor sits **above substrate and below scripts**:

```
foundation / HAL
    ↑
substrate (zone, index, credit, mutation, bus, epoch, shootdown)
    ↑
reactor (this document)  ← wait/submit, waker delivery, AST, sync coord
    ↑
subsystems (structure / checks / execution / project)
    ↑
scripts (per-syscall programs; invoke reactor on Blocked)
```

Scripts invoke the reactor when a step returns `Blocked` or `AdvancedThenBlocked`. Steps themselves never call the reactor — they return outcomes that scripts translate into reactor invocations.

---

## The Task concept
<!-- txdoc:REACTOR-THE-TASK-CONCEPT -->

The reactor's atomic unit of work is a **task**.

A task is:

- what the reactor schedules;
- what the reactor parks;
- what the reactor wakes;
- what the reactor polls.

A task is **not**:

- a userspace thread (that mapping is THREAD_RUNTIME_v1's concern);
- a kernel worker;
- an entity in the semantic sense (no projections, no bindings, no retention in the object-model sense).

### Temporal, not semantic
<!-- txdoc:REACTOR-TEMPORAL-NOT-SEMANTIC -->

Reactor task identity is **temporal**, not semantic. It exists only for the purposes of scheduling, parking, waking, and polling. Semantic identity — what *thing* the task is running on behalf of — lives in subsystems.

This matters because a single userspace thread, a kernel maintenance job, or a background reclaim worker are all tasks *to the reactor*. The reactor does not distinguish them. Distinguishing them is semantic work done by subsystems that hold whatever identity those tasks carry (ThreadPayload for userspace threads, subsystem-specific handles for kernel work).

### Consequences
<!-- txdoc:REACTOR-CONSEQUENCES -->

- A task carries a reactor-authored future (or an authored coroutine realized as such).
- A task's lifetime is bounded by that future's lifetime; when the future resolves or is dropped, the task ceases.
- The reactor does not own the future's construction or semantics — it receives the future from whoever submitted the task.
- Task identity is an opaque handle; subsystems that need to reason about task identity over time hold the handle, not its internals.

---

## Submission / polling contract
<!-- txdoc:REACTOR-SUBMISSION-POLLING-CONTRACT -->

### Submission
<!-- txdoc:REACTOR-SUBMISSION -->

Semantic code does not touch scheduling internals. The ways a task enters the reactor are limited to:

- Initial system bring-up submits the root set of tasks (idle workers, per-CPU housekeeping, etc.).
- A running task may cause a new task to be submitted — for example, `fork` / `clone` producing a new userspace thread as a new task.
- The reactor may submit internal maintenance tasks as part of its own responsibilities.

Submission is the reactor's concern. Other code hands a future (or an authored coroutine) to the reactor; the reactor decides when that future is polled.

### Polling
<!-- txdoc:REACTOR-POLLING -->

The reactor polls tasks at times of its own choosing. Specifically:

- A task is polled only when the reactor considers it runnable.
- A task becomes runnable either on initial submission, or when a waker associated with the task fires.
- The reactor may batch, reorder, defer, or preempt polls per its own scheduling policy.

**The reactor does not guarantee any particular polling order, latency, or fairness in v0.** Those properties follow from scheduling policy, which is a later document's concern. What REACTOR_v0 states is an expectation of liveness *under a valid scheduler policy*: a runnable task is not a terminal state from the reactor's perspective, and any scheduler policy is expected to eventually poll runnable tasks. This is a constraint on scheduler policies, not an unconditional promise by the reactor.

### Polling and guard discipline
<!-- txdoc:REACTOR-POLLING-AND-GUARD-DISCIPLINE -->

Each poll of a task is a discrete unit. Guards (epoch guards, RAII resources, step-local state) do not cross polls. A future that yields (by returning `Poll::Pending`) has released its guards by that point; the next poll acquires fresh ones.

This is a consequence, not a new rule: STEP-2 already requires steps to be synchronous and bounded, and guards are step-scoped. The reactor's polling boundary is exactly where that bounding is observable.

### No reactor calls from steps
<!-- txdoc:REACTOR-NO-REACTOR-CALLS-FROM-STEPS -->

Steps are synchronous and bounded (STEP-2). Steps must not call reactor services directly — they return outcomes (`Blocked`, `AdvancedThenBlocked`) that the enclosing script translates into reactor invocations. This preserves the invariant that steps do not yield inside themselves.

---

## Wait contract
<!-- txdoc:REACTOR-WAIT-CONTRACT -->

When a step returns `Blocked(channel, mask)` or `AdvancedThenBlocked(progress, channel, mask)`, the enclosing script invokes the reactor's wait service.

### The wait primitive
<!-- txdoc:REACTOR-THE-WAIT-PRIMITIVE -->

The reactor exposes a wait-adapt service whose public contract is expressed in terms of `Channel`, `Mask`, `WaitProtocol`, and `WaitOutcome`. The full behavioral specification of this service is given in CONCEPTS §14 and STEP_MODEL §5; this document pins only its boundary.

### Protocol selection is script responsibility
<!-- txdoc:REACTOR-PROTOCOL-SELECTION-IS-SCRIPT-RESPONSIBILITY -->

The step reports *what* it is waiting on (channel, mask). The script decides *how* to wait (protocol). This is a real boundary:

- Steps know the semantic condition — they return a channel and mask naming the condition.
- Scripts know the syscall's nature — nonblocking flags, cancellation semantics, timeout arguments. They choose the `WaitProtocol`.
- The reactor knows the mechanism — it parks the task until wakeup or classified interruption.

Steps do not choose protocol. Scripts do not invent channels. Reactor does neither.

This maps onto the closed driver modes from CONCEPTS §14.2 (nonblocking, waiting, selecting) and the closed script-phase classes from SCRIPT-4: drive is in-script control flow that composes `Blocked*` outcomes, wait-adapt is reactor-provided.

### Classified outcomes
<!-- txdoc:REACTOR-CLASSIFIED-OUTCOMES -->

Wait returns a `WaitOutcome`:

- `Ready` — a wake event relevant to this wait has fired; the caller should re-run the step's observe sub-phase to determine whether the condition actually holds. This is not a truth claim (SIG-1: wake is not truth; fresh observation authorizes action).
- `Interrupted` — a signal interrupted the wait; driver translates to `EINTR` or partial progress per syscall semantics.
- `Killed` — the task is being terminated; driver unwinds cleanly.
- `TimedOut` — the wait's timeout elapsed; driver translates to the syscall's timeout semantics.

The reactor never returns `Ready` as a guarantee that the waited-on condition holds. `Ready` says only that the reactor has observed a wake event and the caller should re-check. The caller re-invokes the step's observe sub-phase under a fresh guard (STEP-7) to establish actual truth.

### What the contract does not specify
<!-- txdoc:REACTOR-WHAT-THE-CONTRACT-DOES-NOT-SPECIFY -->

The reactor does not specify:

- How `Interrupted` is raised (signal delivery mechanics live in THREAD_RUNTIME_v1 / PROCESS_v1);
- How `Killed` is raised (thread-exit mechanics live in THREAD_RUNTIME_v1);
- How timeouts are measured (timer infrastructure is an implementation concern);
- How multi-channel selecting composes internally (that is wait-adapt's implementation, cited from STEP_MODEL §5).

---

## Wake contract
<!-- txdoc:REACTOR-WAKE-CONTRACT -->

### Wakers are reactor-owned
<!-- txdoc:REACTOR-WAKERS-ARE-REACTOR-OWNED -->

A `Waker` is an opaque reactor-owned handle. Bus primitives (RawQueue, RawPort, RawTrace) invoke wakers when state transitions trigger publication per SIG-4. The bus does not schedule; it fires the waker and moves on.

### Waker invocation is cheap
<!-- txdoc:REACTOR-WAKER-INVOCATION-IS-CHEAP -->

Waker invocation must be cheap and non-blocking. Bus fire paths call it from within step publish sub-phases, which are synchronous and bounded (STEP-2). A waker that blocks, allocates, or performs I/O violates the bus's contract with the step model.

### Wake does not grant truth
<!-- txdoc:REACTOR-WAKE-DOES-NOT-GRANT-TRUTH -->

On waker firing, the reactor marks the associated task runnable. Eventually the reactor polls the task. The task's future re-enters its wait loop, re-observes under a fresh guard, and decides whether to proceed (SIG-1 / SIG-9).

The waker is the *hint* that something might have changed. The fresh observation on the subsequent poll is what authorizes action. This is load-bearing for the framework: if wake granted truth, SCRIPT-2 state-blindness would be inconsistent with correctness.

### Spurious wakes are allowed
<!-- txdoc:REACTOR-SPURIOUS-WAKES-ARE-ALLOWED -->

The reactor may poll a task whose wait condition has not actually been satisfied (stale waker firings, aggregated wake events, internal scheduler choices). The framework's re-observation discipline ensures this is safe: a spurious wake produces a re-observation that returns `Blocked` again and re-parks.

The reactor may choose to minimize spurious wakes for performance; it is not required to eliminate them.

---

## Carve-outs
<!-- txdoc:REACTOR-CARVE-OUTS -->

Two coordination concerns cannot go through the ordinary submit / wait / wake cycle. They are reactor responsibilities, named here so other subsystems cite them rather than reinvent them.

### AST (asynchronous trap)
<!-- txdoc:REACTOR-AST-ASYNCHRONOUS-TRAP -->

The AST carve-out handles delivery of events that must fire **between two polls of the same task**, not inside either poll. Canonical examples include:

- Signal delivery at return-to-userspace;
- Fault injection following a structural transition;
- Pre-emption marks that affect the task's next poll entry.

AST belongs to the reactor because only the reactor observes the "between two polls" moment. No subsystem can name that moment from its own vantage point.

**REACTOR_v0 names AST as a reactor carve-out.** The exact shape (how AST events are queued, prioritized, and consumed by the task on its next poll) is deferred to THREAD_RUNTIME_v1 and signal-delivery specifications. This document claims the territory and refuses to let other subsystems colonize it.

### Cross-core synchronous coordination
<!-- txdoc:REACTOR-CROSS-CORE-SYNCHRONOUS-COORDINATION -->

Some operations require a synchronous rendezvous across CPUs — the canonical case is TLB shootdown, where the initiating CPU must block until remote CPUs have acknowledged invalidation. Per EXC-2, cross-core barriers are **not signals**; they are synchronous coordination events.

The reactor provides a synchronous coordination primitive for these cases. Its exact shape (issuing, awaiting completion, completion reporting) is implementation-layer detail. The contract point is that such coordination is reactor-owned, not bus-owned, not subsystem-owned.

---

## Preemption and userspace execution
<!-- txdoc:REACTOR-PREEMPTION-AND-USERSPACE-EXECUTION -->

The reactor supports **preemptive execution of userspace-running tasks**. This section names the mechanism as a reactor-owned commitment so other documents can cite it. Scheduling policy — the budget, the selection algorithm, the priority model — remains deferred (see §Non-goals).

### Userspace-run as a wait
<!-- txdoc:REACTOR-USERSPACE-RUN-AS-A-WAIT -->

A task whose future represents a userspace thread needs periods in which the hart runs its userspace code. The reactor models this as a **wait primitive** at the task's future level: the future awaits a resolution that arrives only when an **interesting trap** occurs — a syscall, page fault, or fatal hardware event. The await does **not** resolve on timer interrupts or scheduler dispatch events.

```rust
/// Resolution reported to a userspace-run wait when it resolves.
pub enum TrapInfo {
    Syscall(SyscallRequest),
    PageFault { addr: UserAddr, cause: FaultCause },
    FatalHardware(HwTrapCause),
    // ... extensible per HAL
}
```

The reactor exposes the userspace-run primitive as a method on its main trait — see §Public interface types for the full signature.

A task at this await is marked "wants-userspace" in reactor-internal state. The reactor's scheduler may grant the hart to this task any number of times (via scheduling dispatches and timer preemptions) before the await resolves; the future sees none of those dispatches.

### Preemption transparency
<!-- txdoc:REACTOR-PREEMPTION-TRANSPARENCY -->

**The future's state machine does not advance on preemption.** Concretely: while a task is at `request_userspace_run`'s await:

- The task's future is **not polled** when the reactor dispatches userspace.
- The task's future is **not polled** when a timer interrupt preempts userspace.
- The task's future is **not polled** when the reactor picks a different task to run.
- The task's future **is polled** when an interesting trap resolves the await.

Between any two polls of this task, userspace may have run for any number of scheduler-granted slices, been preempted any number of times, and been dispatched to any number of harts. The await sees only the single interesting trap that finally resolves it.

This has two important consequences:

- **Scheduling policy can be arbitrarily sophisticated** without affecting task future shape. Budget, priority, affinity, load balancing — all live below the future abstraction.
- **Costs are proportional to interesting events.** State-machine advance cost is paid per syscall/fault, not per preemption.

### Mechanism sketch
<!-- txdoc:REACTOR-MECHANISM-SKETCH -->

A userspace-run dispatch proceeds as follows:

1. Scheduler picks this task for this hart. Reactor reads the task's scheduling metadata (budget remaining, priority, etc.).
2. Reactor programs the hart's preemption timer for the task's slice budget.
3. Reactor reads `ThreadPayload.regs` and loads the userspace context into the hart.
4. Reactor drops to userspace (e.g., `sret` on RISC-V).
5. Userspace executes until a trap.
6. Hart re-enters the kernel; trap handler runs.
7. Trap handler saves userspace regs back to `ThreadPayload.regs`.
8. Trap handler identifies the trap:
   - **Timer (slice expired)**: scheduler returns the task to the runqueue; picks next task; dispatch resumes at step 1 with a potentially different task.
   - **Syscall / fault / fatal**: scheduler marks the await as resolvable with the trap info; task becomes runnable for its future's next poll.
9. Reactor proceeds to its next scheduling decision.

Steps 1–8 are reactor mechanism. The task's future is untouched throughout; it participates only when step 8 resolves the await.

### Budget placement
<!-- txdoc:REACTOR-BUDGET-PLACEMENT -->

Per-task scheduling metadata (budget, priority, scheduling class) is **reactor-owned**, stored on or adjacent to `TaskHandle`. It is **not** on `ThreadPayload` — ThreadPayload is thread-runtime state (userspace context, signal masks, etc.); scheduling accounting is a separate concern.

Remaining budget may be held per-task (travels across sleep/wake) or recomputed per-dispatch depending on scheduler policy choice. REACTOR_v0 does not pin this; a later scheduler spec does.

### Kernel-mode preemption
<!-- txdoc:REACTOR-KERNEL-MODE-PREEMPTION -->

Kernel tasks (tasks not awaiting `request_userspace_run`) are not preempted by the hart timer. Kernel work runs cooperatively between poll boundaries:

- A step is bounded (STEP-2) and runs synchronously in a single poll.
- Between polls, guards are released and the reactor is free to schedule any task.
- Timer interrupts that fire while a kernel-mode step is executing **do not preempt the step**. The scheduler defers its rescheduling decision until the current poll completes.

This preserves STEP-2 (no yield inside a step) and eliminates the lock-boundary preemption discipline of Linux (preempt_count). Safe reschedule points in our kernel are poll boundaries, not lock boundaries.

Concretely: if a timer interrupt arrives during kernel-mode execution, the trap handler:

1. Records a "need_resched" marker on the reactor's per-hart scheduling state.
2. Returns from the interrupt back to the kernel-mode code that was running.
3. That code runs its step to completion, returns from the poll.
4. Before starting the next poll, the reactor checks "need_resched"; if set, it invokes the scheduler to pick the next task rather than re-polling the prior one.

This is Linux's need_resched flag adapted for poll boundaries. The scheduler never interrupts a running step.

### What's deferred
<!-- txdoc:REACTOR-WHAT-S-DEFERRED -->

- The exact slice policy (fixed, target-latency, priority-weighted, cgroup-quota) is a scheduler-spec concern.
- Whether preemption uses timer interrupts only, or also cross-core IPIs (for reschedule hints from other harts), is a scheduler-spec concern.
- How `request_userspace_run` resolution interacts with signal delivery (the AST carve-out) is THREAD_RUNTIME's concern.
- Whether kernel tasks ever need cooperative yield points beyond step boundaries (e.g., for long-running compositional scripts) is a subsystem concern; REACTOR_v0 commits only to "poll boundaries are safe points."

REACTOR_v0 commits to the **mechanism**: userspace preemption exists; it is transparent to task futures; kernel code runs cooperatively between polls. Everything above this is policy.

---

## Public interface types
<!-- txdoc:REACTOR-PUBLIC-INTERFACE-TYPES -->

The following types are the reactor's public surface. Their exact Rust spelling is flexible; their existence and role are not.

```rust
/// Opaque reactor task identity. Temporal, not semantic.
pub struct TaskId(/* opaque */);

/// A handle on a submitted task. Ownership and lifetime semantics
/// are deferred to THREAD_RUNTIME_v1; at v0 this is an opaque
/// reactor-side reference.
pub struct TaskHandle(/* opaque */);

/// The channel a step reports waiting on. Realized by a bus primitive
/// (RawQueue / RawPort) on the subsystem side; opaque to the reactor.
pub struct Channel(/* opaque */);

/// The set of events the caller cares about on a channel.
/// Subsystem-typed; see BUS_v1 for the static-declaration machinery.
pub trait InterestMask: Copy + Send + Sync + 'static { /* ... */ }

/// Protocol selected by the script based on syscall nature.
pub enum WaitProtocol {
    Uninterruptible,
    Interruptible,
    Killable,
    InterruptibleTimeout(Duration),
    KillableTimeout(Duration),
}

/// Classified wait outcome returned to the driver.
pub enum WaitOutcome {
    Ready,
    Interrupted,
    Killed,
    TimedOut,
}

/// Opaque reactor-owned wake handle. Bus primitives invoke; reactor
/// marks task runnable and polls at its discretion.
pub struct Waker(/* opaque */);

/// Minimal reactor surface.
pub trait Reactor {
    /// Submit a new task carrying an author-supplied future.
    fn submit(&self, task: TaskHandle);

    /// Wait for a condition on a channel, under a script-chosen protocol.
    /// Invoked by scripts on Blocked* step outcomes; never by steps directly.
    fn wait(
        &self,
        channel: Channel,
        mask: impl InterestMask,
        protocol: WaitProtocol,
    ) -> impl Future<Output = WaitOutcome>;

    /// Request userspace execution for this task. Returns a future that
    /// resolves on the next interesting trap (syscall / fault / fatal).
    /// Timer-based preemption during userspace execution does not resolve
    /// the returned future — it is reactor-internal.
    /// Invoked only by tasks representing userspace threads.
    fn request_userspace_run(
        &self,
        payload: &Cap<ThreadPayload>,
    ) -> impl Future<Output = TrapInfo>;

    /// Mark a task runnable. Normally invoked through a Waker, not called directly.
    fn wake_task(&self, task: TaskId);

    // AST queueing and synchronous coordination entry points exist
    // but are named as carve-outs, not specified in v0.
}
```

These shapes are contract-level. A particular implementation may split `wait` into separate services per protocol, may inline `TaskHandle` / `TaskId`, may expose `wake_task` only through `Waker`, and so on. What it may not do is add semantic concerns to these types (no subsystem-owned state on `Task`, no policy on `WaitOutcome`).

---

## Non-goals
<!-- txdoc:REACTOR-NON-GOALS -->

REACTOR_v0 **does not** specify:

- **Scheduling policy.** Fairness, priorities, cgroup-driven weights, per-CPU affinity, NUMA considerations, slice budget computation, target latency. These live in `policy::cgroup` and a later scheduler-spec document. (Note: preemption **mechanism** is committed in §Preemption and userspace execution; only policy is deferred.)
- **Thread state machine.** Runnable / Blocked / Stopped / Zombie semantics, stopped-by-ptrace vs stopped-by-SIGSTOP, thread-group-exit, thread-vs-process exit interaction. These live in THREAD_RUNTIME_v1.
- **In-flight future ownership and lifetime.** Who constructs a thread's future, who holds it, when it is dropped, how ThreadPayload relates to future lifetime. THREAD_RUNTIME_v1.
- **Signal-interrupt mechanics across blocked waits.** The `check_interrupt` / AST two-site discipline, per-thread vs process-directed signal routing, interruptible-vs-uninterruptible sleep semantics. THREAD_RUNTIME_v1 and PROCESS_v1.
- **Signal state layout.** SigActionTable, PendingSignalSet, group_pending, signal_mask placement. PROCESS_v1.
- **ptrace runtime stops.** The three orthogonal intercepts (reactor layer, lifecycle gates, check_interrupt). Observation subsystem.
- **Immediate fast path.** An optimization where simple syscalls (getpid, time-query, already-ready I/O) bypass reactor park/poll round-trips. This is a performance concern, not part of the v0 contract. Other subsystems may not assume its existence.
- **Epoch quiescence mechanics.** How the epoch substrate advances epochs, detects quiescence, and drains reclaim queues is the epoch substrate's concern (`tx-fnd/epoch`). REACTOR_v0 **does** pin one epoch-facing fact — guards do not cross polls, and poll boundaries are where that bounding is observable — but the reclamation machinery built on top of that fact lives in the epoch substrate, not here.

Subsystems that need these concerns must wait for their governing document or raise the gap as a question. They must not work around the gap by reinventing reactor responsibilities.

---

## What depends on this contract
<!-- txdoc:REACTOR-WHAT-DEPENDS-ON-THIS-CONTRACT -->

The following existing documents cite the reactor boundary and will be anchored by this contract:

- **BUS_v1 §7, §10.** Waker invocation, no-scheduling-in-bus, no-timers-in-bus.
- **STEP_MODEL_v1 §5, §3.** Classified wait outcomes, the wait primitive as reactor service.
- **SUBSYSTEM_ANATOMY §6.** Reactor in the dependency graph.
- **INVARIANTS SCRIPT-4, SCRIPT-6.** Wait-adapt as reactor-provided; scripts state-blind.
- **CONCEPTS §2.5, §12.4, §14.** Four runtime roles; wait-adapt placement; driver modes.
- **ADR-resolution-half Part VI.** Reactor responsibilities in the architecture-vs-implementation split.

THREAD_RUNTIME_v1 will be the primary new document that builds on this contract.

---

## Open questions (deferred to later docs)
<!-- txdoc:REACTOR-OPEN-QUESTIONS-DEFERRED-TO-LATER-DOCS -->

- **Task-to-thread mapping.** Does every userspace thread get exactly one reactor task? What about kernel workers that serve multiple userspace threads? THREAD_RUNTIME_v1.
- **Task identity stability.** Can a task be reassigned to a different underlying future (e.g., across exec)? THREAD_RUNTIME_v1.
- **AST event shape.** What data structure queues pending AST events? How does the task consume them on next poll? THREAD_RUNTIME_v1 + signal-delivery spec.
- **Synchronous coordination primitive shape.** What API does VM call for TLB shootdown? VM subsystem spec + reactor implementation.
- **Scheduler hooks.** Where does `policy::cgroup` attach to the reactor? Scheduler-policy doc (post-THREAD_RUNTIME_v1).

These are named so that the gaps are visible. They are not part of v0.

---

## Short version
<!-- txdoc:REACTOR-SHORT-VERSION -->

> The reactor is the runtime infrastructure that schedules tasks, mediates wait and wake, owns AST and cross-core sync carve-outs, and provides the userspace-run primitive that userspace-carrying tasks await. A task is its atomic unit — temporal, not semantic. Scripts invoke the reactor on `Blocked` step outcomes, supplying the wait protocol; steps do not call the reactor. Wakes are hints; fresh observation authorizes action. **Userspace preemption is a committed mechanism**: timer-based preemption is reactor-internal and transparent to task futures; kernel work runs cooperatively between poll boundaries. Scheduling **policy**, thread state machine, signal mechanics, and ptrace stops are **not** in this contract — they belong to later documents that cite this one.
