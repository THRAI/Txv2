# Scheduler — v0 Policy Contract

<!-- txdoc:02-EXECUTION-SCHEDULER-V0 -->

## Status
<!-- txdoc:SCHED-STATUS -->

Draft v0.4.

This document pins the **scheduler policy boundary** that REACTOR_v0 explicitly defers and THREAD_RUNTIME_v1 / PROCESS_v1 implicitly depend on. It is a policy-and-interface document, not an exhaustive algorithmic specification:

- It names the `SchedulerPolicy` trait the reactor consults for task selection and slice decisions.
- It pins the data structures that hold per-task scheduling state.
- It specifies a Phase 1 MVP scheduler (round-robin, fixed slice, two-queue dispatch, initial affinity placement) that suffices for Phase 1 targets (busybox, gcc, nginx, top, gdb).
- It sketches Phase 2 extension points (priority classes, cgroup CPU quotas, fair-share, RT classes, deadline).

It does **not** specify algorithmic details of Phase 2+ policies (full fair-share weight mathematics, EDF admission control, cross-hart load balancing). Those are later specs that cite this one.

Companion documents:

- [`REACTOR_v0.md`](REACTOR_v0.md) — reactor contract; preemption mechanism.
- [`THREAD_RUNTIME_v1.md`](THREAD_RUNTIME_v1.md) — thread_future and its composition with scripts.
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) — process model; nice/setpriority are deferred to this scheduler spec.
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — architectural homes and reactor/scheduler split.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — STEP-2 bounds; SCRIPT-* rules.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §6 — layering; policy consults data, doesn't own entities.

### What this document pins
<!-- txdoc:SCHED-WHAT-THIS-DOCUMENT-PINS -->

- `SchedulerPolicy` trait — the interface the reactor calls.
- `TaskSchedMeta` — per-task scheduling state owned by the scheduler.
- `HartSched` — per-hart scheduling state (internal to reactor+scheduler).
- `SliceConfig` — how the reactor programs the preemption timer.
- `StopReason` — how the reactor tells the scheduler why a task stopped.
- Phase 1 scheduler: round-robin with fixed 10ms slice, cooperative for kernel tasks, two-queue (new vs preempted) heuristic, and initial affinity-aware queue placement.
- Budget placement decision: per-task metadata, travels across sleep/wake (Zircon-style).
- Extension contract for Phase 2 additions without interface breakage.

### Zone-derived type policy
<!-- txdoc:SCHED-ZONE-DERIVED-TYPE-POLICY -->

The scheduler is a policy object, not an entity-owning subsystem:

| Scheduler declaration | Public type | Reclamation role |
|---|---|---|
| runnable task membership | `TaskId` / `TaskHandle` from reactor | temporal handle, no zone |
| `TaskSchedMeta` | scheduler-owned table value | policy metadata, no zone |
| runqueue entries | task identifiers | derived scheduling materialization |
| userspace thread being scheduled | `Cap<ThreadPayload>` only when handed through reactor interfaces | owned by THREAD_RUNTIME, not scheduler |

Scheduler implementations must not introduce `Cap<Task>` or
`Zone<TaskSchedMeta, Policy>`. Retention for semantic work belongs to the
subsystem whose task is running.

### What this document defers
<!-- txdoc:SCHED-WHAT-THIS-DOCUMENT-DEFERS -->

- **Full fair-share algorithms.** CFS-style red-black tree with vruntime, Zircon-style WFQ with virtual-finish-time. Phase 2.
- **RT scheduling classes.** SCHED_FIFO, SCHED_RR, SCHED_DEADLINE. Phase 2.
- **Cgroup CPU controller.** Weight-based, quota-based. Phase 2.
- **Cross-hart load balancing.** Work stealing, per-hart run-queue imbalance detection. Phase 2.
- **NUMA awareness.** Phase 3+ (RISC-V NUMA is uncommon in Phase 1 target hardware).
- **Dynamic CPU affinity.** `sched_setaffinity` syscall, live affinity-mask updates, and forced migration. Phase 2. Phase 1 consumes an initial affinity mask for queue placement and wake homing.
- **Priority inheritance.** PI futex, rtmutex-style inheritance. Needs RT classes first.
- **Group scheduling.** Scheduling whole process groups or cgroups as units.
- **Energy-aware scheduling.** Big.little, P-state coordination. Phase 3+.

---

## 1. Position in the architecture
<!-- txdoc:SCHED-1-POSITION-IN-THE-ARCHITECTURE -->

The scheduler is a **policy consulted by the reactor**, not a subsystem with its own entities or runqueues. It owns decisions; it does not own mechanism.

```
┌────────────────────────────────────────────────┐
│ thread_future / scripts / steps                │
│ (see THREAD_RUNTIME, SUBSYSTEM_ANATOMY)        │
├────────────────────────────────────────────────┤
│ Reactor (REACTOR_v0)                           │
│   - Task table, TaskHandle, waker delivery     │
│   - Per-hart runqueues                         │
│   - Timer programming, trap handling           │
│   - Context save/restore (ThreadPayload.regs)  │
│   - Consults SchedulerPolicy for picks         │
├────────────────────────────────────────────────┤
│ SchedulerPolicy (this document)                │
│   - Decides task selection                     │
│   - Decides slice length                       │
│   - Maintains per-task scheduling state        │
│   - No entity ownership; no mechanism          │
└────────────────────────────────────────────────┘
```

Reactor is mechanism; scheduler is policy. The interface between them is a trait (`SchedulerPolicy`). Different policies can be plugged in (MVP round-robin in Phase 1; fair-share + RT classes in Phase 2; etc.) without changing the reactor.

This mirrors Tock's scheduler-as-trait design (with lessons applied from Fuchsia's per-hart run queues and Aspen-KB's two-queue dispatch heuristic).

### 1.1 What the scheduler is not
<!-- txdoc:SCHED-1-1-WHAT-THE-SCHEDULER-IS-NOT -->

- **Not a subsystem in the CONCEPTS §2.5 sense.** No entities with projections or bindings. Its state is derived from task-lifecycle events.
- **Not the only scheduler that can exist.** Multiple `SchedulerPolicy` implementations can coexist (each handling a class of tasks); a composite scheduler dispatches among them by class.
- **Not in the critical path of steps.** The scheduler runs at task-lifecycle events (task becomes runnable, task stops, task completes) and at preemption timer events. It does not run during step execution.

### 1.2 Why layer-consulted rather than layer-owned
<!-- txdoc:SCHED-1-2-WHY-LAYER-CONSULTED-RATHER-THAN-LAYER-OWNED -->

The reactor could own scheduling decisions directly. It chooses to defer to a separate policy for three reasons:

- **Testability.** Policy implementations can be unit-tested in isolation by feeding task-lifecycle events.
- **Extensibility.** New policies for new workloads don't require reactor changes.
- **Measurability.** Policy decisions can be instrumented and swapped at boot without rebuilding the reactor.

This is a standard pattern in modern kernels (Fuchsia separates fair vs deadline schedulers; Linux has pluggable scheduler classes; Tock uses a trait).

---

## 2. Core types
<!-- txdoc:SCHED-2-CORE-TYPES -->

### 2.1 SchedulerPolicy trait
<!-- txdoc:SCHED-2-1-SCHEDULERPOLICY-TRAIT -->

```rust
pub trait SchedulerPolicy: Send + Sync {
    /// Pick the next task to run on this hart, and the slice configuration.
    /// Returns a TaskHandle (dispatchable) plus slice config.
    /// Called by the reactor when the hart needs a task: at startup, after
    /// a task stops, on preemption timer, on wake-induced reschedule.
    ///
    /// Returns None if no runnable task exists for this hart (reactor idles).
    fn pick_next(&self, hart: HartId) -> Option<(TaskHandle, SliceConfig)>;

    /// A task stopped running. Report why and how much it consumed.
    /// The scheduler updates metadata and requeues the task if appropriate.
    fn task_stopped(
        &self,
        task: TaskId,
        reason: StopReason,
        consumed_ns: u64,
        hart: HartId,
    );

    /// A task became runnable. Called from a waker path or submission path.
    /// The scheduler enqueues the task to the appropriate runqueue.
    fn task_runnable(
        &self,
        task: TaskId,
        hint: WakeHint,
    );

    /// A task was submitted for the first time. Distinct from task_runnable
    /// because it provides initial scheduling metadata (priority, class)
    /// and a fresh TaskHandle the scheduler must store for later pick_next.
    fn task_submitted(
        &self,
        task: TaskId,
        handle: TaskHandle,
        initial_meta: InitialSchedMeta,
    );

    /// A task is being dropped (future resolved, task exit). Cleanup.
    fn task_dropped(&self, task: TaskId);

    /// Optional: asked whether to defer interrupt bottom-halves.
    /// Default: false (don't defer; handle interrupts eagerly).
    fn defer_kernel_work(&self) -> bool {
        false
    }
}
```

Key points:

- **`TaskId`** is the lightweight, copyable identifier used for tables, queue entries, and lifecycle-event messaging between reactor and scheduler. The scheduler stores these as map keys and in its runqueues.
- **`TaskHandle`** is the dispatchable reference the reactor needs to actually run a task. It is provided once at `task_submitted` (the scheduler stores it, typically in `TaskSchedMeta`) and returned to the reactor at `pick_next`.
- **`pick_next`** is called at every scheduling decision. Returns both the task (as TaskHandle) and how to run it (cooperatively or with a slice budget).
- **`task_stopped`** is the feedback loop — the scheduler learns how much each task actually consumed and why it stopped.
- **`task_runnable`** is the wake path — a task is now eligible to run; the scheduler decides where it goes in its runqueues.
- **`task_submitted`** and **`task_dropped`** bracket the task's lifecycle with the scheduler.
- **`defer_kernel_work`** is Tock-inspired: lets the scheduler defer interrupt handling if latency-critical userspace is pending. Default no (conservative).

### 2.2 SliceConfig
<!-- txdoc:SCHED-2-2-SLICECONFIG -->

```rust
pub enum SliceConfig {
    /// Run cooperatively. No preemption timer. Task runs until it yields
    /// (kernel-only task awaiting internally) or until an interesting trap
    /// (userspace task).
    Cooperative,

    /// Run with a preemption budget. Reactor programs the hart's timer for
    /// this many nanoseconds; task is preempted if slice expires.
    Preemptive { slice_ns: u64 },
}
```

- **Cooperative** is used for:
  - Kernel-only tasks (driver work, reactor maintenance).
  - Real-time tasks where the scheduler trusts the task to yield (SCHED_FIFO in Phase 2).
  - Idle task (no budget needed; runs until displaced).
- **Preemptive** is used for:
  - Ordinary userspace threads in Phase 1.
  - Fair-share and RT-RR tasks in Phase 2.

The reactor programs the timer per this config at each dispatch.

### 2.3 StopReason
<!-- txdoc:SCHED-2-3-STOPREASON -->

```rust
pub enum StopReason {
    /// Preemption timer expired. Task used its full slice.
    SliceExpired,

    /// Task voluntarily yielded (reactor::yield_now call or equivalent).
    Yielded,

    /// Task blocked on a wait (step returned Blocked; reactor::wait await
    /// registered a waker and returned Pending).
    Blocked,

    /// Task is in userspace; it was preempted by an interesting trap
    /// (syscall/fault) but was not blocking.
    UserspaceTrap,

    /// Task's future completed.
    Completed,

    /// Task was preempted by an external reason (higher-priority task woke
    /// on this hart; cross-hart reschedule request).
    PreemptedExternal,
}
```

The scheduler uses this to decide what to do with the task:

- `SliceExpired` → requeue to back (or preempted-queue in two-queue scheduling).
- `Yielded` → requeue to back.
- `Blocked` → do not requeue; task will return via `task_runnable` when waker fires.
- `UserspaceTrap` → requeue if it still has remaining budget; else SliceExpired-like.
- `Completed` → forget the task.
- `PreemptedExternal` → requeue to front (expedited resumption of remaining slice).

### 2.4 WakeHint
<!-- txdoc:SCHED-2-4-WAKEHINT -->

```rust
pub enum WakeHint {
    /// Routine wake from an ordinary event (I/O completion, condition variable).
    Normal,

    /// Waker fired in response to a signal (delivery-relevant).
    SignalDelivery,

    /// Waker fired because a higher-priority task unblocked something.
    PriorityBoost,

    /// Scheduler internal hint — no preference.
    None,
}
```

Hints are advisory. A scheduler may use them to decide which hart to migrate to (e.g., keep the task on the hart where the waker fired to preserve cache warmth) or to decide queue placement (e.g., boost signal-delivery wakes to reduce latency).

MVP scheduler ignores hints; Phase 2 schedulers may use them.

### 2.5 InitialSchedMeta
<!-- txdoc:SCHED-2-5-INITIALSCHEDMETA -->

```rust
pub struct InitialSchedMeta {
    pub class: SchedClass,
    pub nice: i8,               // -20..+19, ignored in Phase 1
    pub rt_priority: u8,        // 1..99 for RT classes; ignored in non-RT
    pub affinity: AffinityMask, // set of allowed harts for initial placement/wake homing
}

pub enum SchedClass {
    /// Ordinary fair-share. Phase 1 MVP uses this for everything.
    Fair,

    /// RT FIFO: run until yield/block. Phase 2.
    RtFifo,

    /// RT RR: run with priority-sized slice. Phase 2.
    RtRoundRobin,

    /// Deadline: EDF. Phase 3.
    Deadline,

    /// Idle class: lowest priority, runs when nothing else.
    Idle,
}
```

Phase 1 treats all tasks as `SchedClass::Fair` with nice=0, but it does
honor the initial affinity mask for per-hart queue placement. A zero mask is
normalized to the boot hart. Phase 2 expands the class space and adds dynamic
affinity updates.

### 2.6 TaskSchedMeta (internal to scheduler)
<!-- txdoc:SCHED-2-6-TASKSCHEDMETA-INTERNAL-TO-SCHEDULER -->

Per-task scheduling state, owned by the scheduler. Typically stored in a scheduler-internal table keyed by TaskId.

```rust
pub struct TaskSchedMeta {
    /// Dispatchable handle, stored at task_submitted, returned at pick_next.
    pub handle: TaskHandle,

    pub class: SchedClass,
    pub nice: i8,
    pub rt_priority: u8,
    pub affinity: AffinityMask,

    /// Remaining budget in the current slice. Zircon-style: travels across
    /// sleep/wake, so a task that blocks early keeps the remainder.
    pub remaining_budget_ns: u64,

    /// Full slice size for this task's class.
    pub full_slice_ns: u64,

    /// Cumulative CPU time consumed (lifetime of task).
    pub total_runtime_ns: AtomicU64,

    /// Which hart most recently ran this task. Used for affinity hints.
    pub last_hart: Option<HartId>,

    /// Phase 2: virtual runtime for fair-share.
    /// Phase 3: deadline parameters.
    /// Phase 1: unused.
    pub policy_specific: PolicySpecific,
}
```

The scheduler maintains one of these per known task, keyed by TaskId in an internal map. The reactor does not see inside; it interacts only through the trait.

### 2.7 HartSched (internal to reactor, passed to scheduler)
<!-- txdoc:SCHED-2-7-HARTSCHED-INTERNAL-TO-REACTOR-PASSED-TO-SCHEDULER -->

Per-hart scheduling state. Lives in the reactor's per-hart structure but is consulted by the scheduler.

```rust
pub struct HartSched {
    pub hart_id: HartId,

    /// Currently-running task on this hart, if any.
    pub current: Option<TaskId>,

    /// Cycle count when the current slice started; used to charge consumed time.
    pub slice_start_cycle: u64,

    /// Absolute cycle count when the current slice expires. Reactor programs
    /// timer to match this.
    pub slice_end_cycle: u64,

    /// Need-resched marker: set if reschedule is deferred past current poll.
    /// (See REACTOR_v0 §Kernel-mode preemption.)
    pub need_resched: AtomicBool,
}
```

The reactor maintains HartSched per hart. When the scheduler calls `pick_next`, it uses this state to make hart-local decisions.

---

## 3. Reactor ↔ scheduler interaction
<!-- txdoc:SCHED-3-REACTOR-SCHEDULER-INTERACTION -->

### 3.1 Dispatch lifecycle
<!-- txdoc:SCHED-3-1-DISPATCH-LIFECYCLE -->

Every dispatch cycle follows this sequence:

```
[reactor wants a task for hart H]
     │
     ├─→ scheduler.pick_next(H) → Option<(task, slice_config)>
     │
     ├─→ if None: idle on this hart (wait for next interrupt)
     │
     ├─→ if Some:
     │     program hart timer per slice_config
     │     record slice_start_cycle, slice_end_cycle
     │     load task's context
     │     - if userspace task: load ThreadPayload.regs, sret
     │     - if kernel task: poll the future
     │
     │  ... hart executes task ...
     │
     ├─→ [trap / yield / block / completion]
     │
     ├─→ consumed_ns = now() - slice_start_cycle
     │   charge TaskSchedMeta.total_runtime
     │   scheduler.task_stopped(task, reason, consumed_ns, H)
     │
     └─→ loop back to top (pick next task)
```

### 3.2 Userspace dispatch details
<!-- txdoc:SCHED-3-2-USERSPACE-DISPATCH-DETAILS -->

For a task at `reactor::request_userspace_run` (per REACTOR_v0 §Preemption):

1. `pick_next` returns this task with `SliceConfig::Preemptive { slice_ns }`.
2. Reactor programs hart timer at `slice_end = now + slice_ns`.
3. Reactor loads ThreadPayload.regs, drops to userspace.
4. Userspace runs until trap.
5. Trap handler identifies the trap:
   - **Timer expired**: `task_stopped(task, SliceExpired, consumed, hart)`. Reactor does not resolve the userspace-run future. Back to `pick_next`.
   - **Syscall/fault/fatal**: Resolve the userspace-run future with TrapInfo. `task_stopped(task, UserspaceTrap, consumed, hart)`. Task's future is now runnable (next poll will advance its state machine). `pick_next` may return this task again if it's the most eligible.

In the SliceExpired case, the future is untouched — the task's state machine stays at the userspace-run await. It will be picked up again later when the scheduler's `pick_next` chooses it.

### 3.3 Kernel task dispatch details
<!-- txdoc:SCHED-3-3-KERNEL-TASK-DISPATCH-DETAILS -->

For a kernel task (not at `request_userspace_run`):

1. `pick_next` returns this task, typically with `SliceConfig::Cooperative`.
2. Reactor does not program a preemption timer.
3. Reactor polls the future.
4. Future runs forward, executing steps and awaiting reactor waits.
5. When the future returns Pending: `task_stopped(task, Blocked, consumed, hart)`. Waker will later fire `task_runnable`.
6. When the future returns Ready: `task_stopped(task, Completed, consumed, hart)`. `task_dropped` called eventually.

Kernel tasks run to their next `.await` or completion. No timer preemption mid-step (STEP-2 contract).

**Note on long-running kernel scripts:** A script composing many steps without yielding can run for extended time. REACTOR_v0 §Kernel-mode preemption sets a need_resched marker if a timer fires during kernel-mode, but that marker is only checked at poll boundaries, not between steps in the same poll. Subsystem authors are expected to yield between logically-distinct steps. If this becomes a latency issue in practice, scripts can add explicit `reactor::yield_now().await` between independent steps.

### 3.4 Wake path
<!-- txdoc:SCHED-3-4-WAKE-PATH -->

When a waker fires:

1. Reactor identifies the associated task.
2. Reactor calls `scheduler.task_runnable(task, hint)`.
3. Scheduler enqueues the task appropriately.
4. If this hart was idle: immediately call `pick_next` and dispatch.
5. If this hart was running a lower-priority task: set need_resched; next poll boundary will reschedule.
6. If this hart was running a higher-priority or same-priority task: no immediate action; the task waits for a slot.

### 3.5 Cross-hart reschedule dispatch
<!-- txdoc:SCHED-3-5-CROSS-HART-RESCHEDULE-DISPATCH -->

In multi-hart systems, waking a task onto hart H' can require a reschedule IPI
from the current hart. The scheduler still does not send IPIs. Phase 1 reports
the selected runqueue through `RunnablePlacement`; reactor dispatch state marks
the target hart `need_resched`, and a kernel runtime adapter translates remote
wakes into `SmpIf::send_ipi(..., IpiKind::Reschedule)`. The current RV64 QEMU
runtime can wake an AP-side kernel loop, consume the target hart's
`need_resched` marker, and drain a real shared-reactor runqueue under a
temporary reactor lock. Per-hart reactor shards, lock-free dispatch, full
priority-aware cross-hart preemption, and load balancing remain Phase 2 policy.

---

## 4. Phase 1 scheduler: MVP
<!-- txdoc:SCHED-4-PHASE-1-SCHEDULER-MVP -->

A concrete Phase 1 implementation of `SchedulerPolicy`.

### 4.1 Goals
<!-- txdoc:SCHED-4-1-GOALS -->

- **Simple.** Round-robin with fixed slice. No priorities. No cgroups.
- **Correct.** Must work for all Phase 1 target workloads (busybox, gcc, nginx, gdb, top). Must not starve any runnable thread.
- **Per-hart independent.** Each hart has its own runqueues. No load balancing.
- **Two-queue dispatch.** Aspen-KB insight: short-running tasks get fast dispatch; long-running CPU-bound tasks get throughput-friendly longer slices.
- **Cooperative for kernel tasks.** Kernel work runs uninterrupted between poll boundaries.

### 4.2 Constants
<!-- txdoc:SCHED-4-2-CONSTANTS -->

```rust
// Slice durations
const BASE_SLICE_NS: u64 = 10_000_000;          // 10ms, ordinary fair slice
const NEW_QUEUE_SLICE_NS: u64 = 1_000_000;      // 1ms, short check for new tasks
const PREEMPTED_QUEUE_SLICE_NS: u64 = 10_000_000; // 10ms, after first preemption

// Idle
const IDLE_CHECK_INTERVAL_NS: u64 = 100_000_000; // 100ms, for wakeup polling
```

### 4.3 Data structures
<!-- txdoc:SCHED-4-3-DATA-STRUCTURES -->

```rust
pub struct Phase1Scheduler {
    /// Per-hart scheduling state.
    per_hart: [HartSchedLocal; MAX_HARTS],

    /// Per-task scheduling metadata, keyed by TaskId.
    task_meta: BTreeMap<TaskId, TaskSchedMeta>,
}

struct HartSchedLocal {
    /// Newly runnable tasks that haven't yet been preempted. Short slice.
    new_queue: SchedQueue<TaskId>,

    /// Tasks that have been preempted at least once (they filled a slice).
    /// Ordinary slice.
    preempted_queue: SchedQueue<TaskId>,

    /// Cooperative kernel tasks.
    kernel_queue: SchedQueue<TaskId>,
}
```

`SchedQueue<T>` is a scheduler-internal FIFO type. It is **distinct from** `DllContainer<T>` used by BINDING_v1 and PROCESS_v1: DllContainer carries Cap-retaining bindings and enforces epoch-safe walks; scheduler queues hold plain identifiers (TaskId) for dispatch with no retention semantics. The task's actual retention is handled by the reactor's task table; the scheduler only tracks scheduling membership. Either an intrusive list (low-overhead) or a bounded ring (if total task count is bounded per hart) is an acceptable realization; this is implementation-layer detail.

### 4.4 pick_next implementation
<!-- txdoc:SCHED-4-4-PICK-NEXT-IMPLEMENTATION -->

```rust
fn pick_next(&self, hart: HartId) -> Option<(TaskHandle, SliceConfig)> {
    let local = &self.per_hart[hart as usize];

    // Priority order: kernel queue → new queue → preempted queue.
    // (Kernel tasks are typically driver/reactor work; small latency benefit
    // to dispatching them first. In practice, they're rarely present at the
    // same time as userspace tasks on the same hart.)
    if let Some(task_id) = local.kernel_queue.pop_front() {
        let meta = self.task_meta.get(&task_id).unwrap();
        return Some((meta.handle.clone(), SliceConfig::Cooperative));
    }

    if let Some(task_id) = local.new_queue.pop_front() {
        let meta = self.task_meta.get(&task_id).unwrap();
        return Some((meta.handle.clone(), SliceConfig::Preemptive {
            slice_ns: NEW_QUEUE_SLICE_NS,
        }));
    }

    if let Some(task_id) = local.preempted_queue.pop_front() {
        let meta = self.task_meta.get(&task_id).unwrap();
        // Use remaining_budget if nonzero; else a fresh slice.
        let slice_ns = if meta.remaining_budget_ns > 0 {
            meta.remaining_budget_ns
        } else {
            PREEMPTED_QUEUE_SLICE_NS
        };
        return Some((meta.handle.clone(), SliceConfig::Preemptive { slice_ns }));
    }

    None  // Hart idles
}
```

### 4.5 task_stopped implementation
<!-- txdoc:SCHED-4-5-TASK-STOPPED-IMPLEMENTATION -->

```rust
fn task_stopped(
    &self,
    task: TaskId,
    reason: StopReason,
    consumed_ns: u64,
    hart: HartId,
) {
    let meta = self.task_meta.get_mut(&task).unwrap();
    meta.total_runtime_ns.fetch_add(consumed_ns, Relaxed);
    meta.last_hart = Some(hart);

    // Subtract consumed from remaining budget.
    meta.remaining_budget_ns = meta.remaining_budget_ns.saturating_sub(consumed_ns);

    let local = &self.per_hart[hart as usize];

    match reason {
        StopReason::SliceExpired => {
            // Task used its full slice. Move to preempted queue with fresh slice.
            meta.remaining_budget_ns = 0;  // Slice fully consumed
            local.preempted_queue.push_back(task);
        }
        StopReason::UserspaceTrap => {
            // Task took a trap but didn't necessarily use full slice.
            // Keep remaining_budget; requeue.
            if meta.remaining_budget_ns > 0 {
                // Still has budget; front of preempted queue (expedited).
                local.preempted_queue.push_front(task);
            } else {
                // Budget exhausted; back of queue.
                local.preempted_queue.push_back(task);
            }
        }
        StopReason::Yielded => {
            // Voluntary yield; treat like slice expired.
            meta.remaining_budget_ns = 0;
            local.preempted_queue.push_back(task);
        }
        StopReason::Blocked => {
            // Task is waiting; do not requeue. task_runnable will handle.
            // remaining_budget preserved (Zircon-style).
        }
        StopReason::Completed => {
            // Task done; cleanup happens in task_dropped.
        }
        StopReason::PreemptedExternal => {
            // External preemption (rare in Phase 1, e.g., from cross-hart wake).
            // Front of preempted queue to resume quickly.
            local.preempted_queue.push_front(task);
        }
    }
}
```

### 4.6 task_runnable implementation
<!-- txdoc:SCHED-4-6-TASK-RUNNABLE-IMPLEMENTATION -->

```rust
fn task_runnable(&self, task: TaskId, _hint: WakeHint) {
    let meta = self.task_meta.get(&task).unwrap();

    // Phase 1: no load-balancing migration. Prefer the last hart if it is
    // still allowed by the task's initial affinity; otherwise use the first
    // allowed hart. A zero affinity mask is normalized to the boot hart.
    let hart = home_hart_for_meta(meta);
    let local = &self.per_hart[hart as usize];

    match meta.class {
        SchedClass::Fair => {
            if meta.remaining_budget_ns > 0 {
                // Had leftover budget from before blocking. Expedited.
                local.preempted_queue.push_front(task);
            } else {
                // Fresh wake; new queue for short-slice check.
                local.new_queue.push_back(task);
            }
        }
        _ => {
            // Phase 1 treats everything as Fair.
            local.new_queue.push_back(task);
        }
    }
}
```

The concrete Phase 1 implementation also exposes a placement-returning helper
for the multi-hart reactor dispatcher:

```rust
pub struct RunnablePlacement {
    pub target_hart: HartId,
    pub wake_remote: bool,
}

fn task_runnable_from(
    &mut self,
    task: TaskId,
    hint: WakeHint,
    current_hart: HartId,
) -> Option<RunnablePlacement>;
```

`None` means the task was not newly enqueued (for example, duplicate wake
coalescing). `Some` reports the selected runqueue and whether the current hart
must notify a remote hart. The scheduler does not send IPIs itself; reactor
dispatch state records `need_resched`, and kernel runtime code translates remote
placement into the platform `SmpIf`.

### 4.7 task_submitted and task_dropped
<!-- txdoc:SCHED-4-7-TASK-SUBMITTED-AND-TASK-DROPPED -->

```rust
fn task_submitted(&self, task: TaskId, handle: TaskHandle, initial: InitialSchedMeta) {
    let meta = TaskSchedMeta {
        handle,
        class: initial.class,
        nice: initial.nice,
        rt_priority: initial.rt_priority,
        affinity: initial.affinity,
        remaining_budget_ns: 0,  // Fresh task; gets new-queue slice on first pick
        full_slice_ns: BASE_SLICE_NS,
        total_runtime_ns: AtomicU64::new(0),
        last_hart: None,
        policy_specific: PolicySpecific::None,
    };

    self.task_meta.insert(task, meta);

    // Enqueue as new task on the first allowed hart.
    let hart = first_hart_in_mask(initial.affinity);
    let local = &self.per_hart[hart as usize];
    local.new_queue.push_back(task);
}

fn task_dropped(&self, task: TaskId) {
    self.task_meta.remove(&task);
    // Task has been removed from all queues by prior task_stopped(Completed) or
    // by task_runnable never re-enqueuing a dropped task.
}
```

### 4.8 Kernel task submission
<!-- txdoc:SCHED-4-8-KERNEL-TASK-SUBMISSION -->

Kernel tasks (non-userspace reactor tasks) are identified at submission. They go directly into the kernel_queue and are always run with `SliceConfig::Cooperative`.

A submission marker (e.g., `InitialSchedMeta.class = SchedClass::Idle` for the idle task, plus a `kernel_only: bool` flag) distinguishes kernel tasks. This is a small extension to InitialSchedMeta. For Phase 1 we add:

```rust
pub struct InitialSchedMeta {
    pub class: SchedClass,
    pub nice: i8,
    pub rt_priority: u8,
    pub affinity: AffinityMask,
    pub kernel_only: bool,        // If true, always Cooperative
}
```

### 4.9 Properties Phase 1 MVP guarantees
<!-- txdoc:SCHED-4-9-PROPERTIES-PHASE-1-MVP-GUARANTEES -->

- **No starvation.** Any runnable fair task eventually reaches the front of one of the queues and is dispatched. Round-robin semantics ensure this.
- **Preemption works.** A CPU-bound thread is preempted every 10ms; other runnable threads get their turn.
- **I/O-bound threads get fast dispatch.** Thanks to new-queue's short slice, a thread that typically blocks quickly gets low-latency dispatch on wake.
- **Kernel work runs cooperatively.** Kernel tasks never preempted; matches REACTOR_v0's preempt-at-poll-boundary discipline.
- **Budget travels across sleep/wake.** A thread that uses 3ms of its 10ms slice before blocking will, on wake, get the 7ms remainder at front of preempted queue. Incentivizes good I/O behavior.
- **Initial affinity is honored.** Task submission and wake placement choose an allowed hart from the task's initial mask. There is no load-balancing migration.

### 4.10 What Phase 1 MVP does not do
<!-- txdoc:SCHED-4-10-WHAT-PHASE-1-MVP-DOES-NOT-DO -->

- **No priority.** `nice` is stored but ignored.
- **No cgroups.** No quota or weight logic.
- **No cross-hart load balancing.** Tasks stay on the hart they were last on; imbalance is possible and accepted.
- **No RT classes.** All tasks are fair.
- **No dynamic affinity syscall semantics.** Initial `AffinityMask` is consulted for queue placement and wake homing, but `sched_setaffinity`-style live updates, forced migration, and load-balancing movement are Phase 2.

---

## 5. Phase 2 extensions
<!-- txdoc:SCHED-5-PHASE-2-EXTENSIONS -->

Phase 2 adds priority classes, fair-share weights, cgroup CPU controllers. This section sketches what changes and what stays stable.

### 5.1 New SchedClass values become active
<!-- txdoc:SCHED-5-1-NEW-SCHEDCLASS-VALUES-BECOME-ACTIVE -->

- **RtFifo.** Runs until yield or block; not preempted by timer (but can be preempted by higher-priority RT task). Cooperative in the slice-config sense, but with priority-ordered selection.
- **RtRoundRobin.** Like RtFifo but with a slice (default 100ms). Round-robins with other RR tasks at same priority.
- **Fair** (Phase 1 default). Becomes weighted fair-share. Each task has a weight derived from nice value (nice -20 = ~88000; nice 0 = 1024; nice 19 = ~15 per standard table). Runtime accounted in virtual-runtime (vruntime); task with smallest vruntime runs next.
- **Deadline** (Phase 3). EDF with admission control.
- **Idle.** Lowest priority; runs only when nothing else runnable.

Priority order at `pick_next`: RT classes first (by rt_priority descending), then Fair (by vruntime), then Idle.

### 5.2 Fair-share data structure
<!-- txdoc:SCHED-5-2-FAIR-SHARE-DATA-STRUCTURE -->

A per-hart red-black tree (or equivalent) keyed by vruntime. `pick_next` pops the minimum. `task_stopped` updates the task's vruntime based on consumed time and weight. Standard CFS territory.

Alternative: WFQ per Fuchsia's fair scheduler. Simpler math for some cases, similar performance.

The choice between CFS-style and WFQ-style is a Phase 2 design decision. Either fits the trait interface.

### 5.3 Cgroup CPU controller
<!-- txdoc:SCHED-5-3-CGROUP-CPU-CONTROLLER -->

Cgroup v2 CPU controller provides:
- **Weight-based sharing.** cgroup has a weight; sibling cgroups share proportionally.
- **Quota/period.** cgroup has a max CPU-time-per-period (e.g., 50% = 50ms per 100ms).

Implementation: hierarchical scheduling, where each cgroup is itself a virtual task in its parent's runqueue. A cgroup's "vruntime" is computed from its own members' vruntimes.

This is a separate subsystem (policy::cgroup per userMemories) that the scheduler queries. At pick_next time, the scheduler consults the cgroup hierarchy to determine eligibility. Complex; Phase 2.

### 5.4 Cross-hart load balancing
<!-- txdoc:SCHED-5-4-CROSS-HART-LOAD-BALANCING -->

Periodic check (every ~4ms): if any hart is idle while another has >1 runnable task, migrate one. Heuristics: prefer cold tasks (bigger cache miss cost already paid); prefer NUMA-local migrations; avoid ping-pong.

Linux's CFS has this; Fuchsia has a simpler version. Phase 2 can start with "migrate from longest queue to idle hart" and evolve.

### 5.5 Affinity enforcement
<!-- txdoc:SCHED-5-5-AFFINITY-ENFORCEMENT -->

Phase 1 already honors the initial `AffinityMask` for queue placement. Phase 2
adds `sched_setaffinity` and `pthread_setaffinity_np` live updates: `pick_next`
only returns tasks whose affinity includes the calling hart, migration respects
affinity, and any now-disallowed queued/running task is moved or preempted at a
defined boundary.

### 5.6 Backward compatibility
<!-- txdoc:SCHED-5-6-BACKWARD-COMPATIBILITY -->

The Phase 1 trait interface (§2) is **stable across Phase 2 additions**. New StopReasons, new SchedClass values, new WakeHints can be added without breaking existing consumers. Phase 1 implementations of the trait can continue working; Phase 2 just adds more sophisticated implementations.

The `PolicySpecific` field in TaskSchedMeta (§2.6) is designed for this: Phase 1 leaves it empty; Phase 2 populates it with vruntime, deadline params, etc.

---

## 6. Phase 3+ future work
<!-- txdoc:SCHED-6-PHASE-3-FUTURE-WORK -->

Listed for completeness; no commitment.

- **Deadline class (SCHED_DEADLINE).** EDF with admission control. For real-time soft-deadline workloads.
- **Energy-aware scheduling.** On big.little cores or with P-state coordination. Requires hardware-topology awareness.
- **NUMA-aware scheduling.** For systems with NUMA domains. Phase 1 targets don't need it; Phase 3+ hardware might.
- **Group scheduling.** Schedule multiple tasks as one unit (e.g., for KVM vCPUs). Affects cgroup design.
- **Proxy execution.** For PI-style lock holder prioritization. Deep systems work.

---

## 7. Interaction with other subsystems
<!-- txdoc:SCHED-7-INTERACTION-WITH-OTHER-SUBSYSTEMS -->

### 7.1 PROCESS
<!-- txdoc:SCHED-7-1-PROCESS -->

PROCESS_v1 defers `nice`, `setpriority`, `sched_setscheduler`, `sched_setaffinity` to this spec (§11.2 of PROCESS_v1). When these syscalls are implemented:

- `nice(inc)` modifies `TaskSchedMeta.nice` via scheduler-provided API.
- `setpriority(who, value)` sets nice for one or more tasks.
- `sched_setscheduler(pid, policy, param)` changes `TaskSchedMeta.class` and rt_priority.
- `sched_setaffinity(pid, cpuset)` sets affinity.

Phase 1 syscalls may accept these calls and return success, but dynamic
updates have no effect beyond initial scheduler metadata: nice is stored but
ignored, scheduler class changes are ignored, and affinity is only consumed at
submission/wake placement. Phase 2 makes the syscalls effective.

### 7.2 THREAD_RUNTIME
<!-- txdoc:SCHED-7-2-THREAD-RUNTIME -->

THREAD_RUNTIME_v1 §4.5 describes the thread_future / scripts / steps composition. The scheduler operates at the task layer — one task per thread. It doesn't see scripts or steps.

Signal delivery (§5.4) and group-exit cascades (PROCESS_v1 §5) do not interact with scheduling beyond firing wakers. A SIGKILL delivery wakes the target thread; the scheduler's `task_runnable` enqueues it. From the scheduler's perspective, a signal-woken task is just another runnable task.

### 7.3 Cred and rlim
<!-- txdoc:SCHED-7-3-CRED-AND-RLIM -->

`RLIMIT_CPU` (CPU time soft/hard limit) interacts with the scheduler. When a task's consumed CPU time exceeds its soft limit, the scheduler (or a time-accounting service) must send SIGXCPU. Hard limit: SIGKILL.

Phase 1 defers RLIMIT_CPU enforcement. Phase 2 wires it through: on `task_stopped`, the scheduler or a consumer checks cumulative runtime against the process's rlimit and posts SIGXCPU if exceeded.

### 7.4 Observation subsystem (future)
<!-- txdoc:SCHED-7-4-OBSERVATION-SUBSYSTEM-FUTURE -->

Scheduler events (dispatch, preempt, block, wake) are natural tracepoints. The observation subsystem subscribes to these events for ptrace and performance analysis. Phase 1 tracepoints are instrumentation only; Phase 2 formalizes them per the observation spec.

---

## 8. Open questions
<!-- txdoc:SCHED-8-OPEN-QUESTIONS -->

- **How does `task_submitted` get called for the initial thread of a newly-forked process?** PROCESS_v1 §7.1.2's step_clone_process phase 4 "submit child thread's future to reactor" implies the reactor calls something. That something should route through `task_submitted` with appropriate InitialSchedMeta. For Phase 1, InitialSchedMeta inherits the parent's scheduling class and nice; fresh budget.
- **Where does InitialSchedMeta come from?** For fork/clone: inherited from parent. For kernel tasks: provided by the submitting subsystem. For initial boot tasks (init, pid 1): hard-coded defaults.
- **How does the idle task fit in?** Conceptually one idle task per hart, always runnable, SchedClass::Idle, never reaps. When pick_next returns None, reactor runs the idle task (which typically does WFI / halt until interrupt). Could also be modeled as "reactor's default idle path" without being a real task.
- **When does the scheduler trigger a reschedule IPI to another hart?** It never does so directly. The low-level SMP IPI surface exists, Phase 1 reports remote wake placement, and the reactor/kernel dispatch bridge translates that placement into `SmpIf::send_ipi`. Priority-aware preemption policy remains later.
- **Should `task_stopped` be synchronous with the stop event, or can it be batched?** Phase 1: synchronous (simpler to reason about). Phase 2 may batch for performance.
- **How to handle `sched_yield`?** Userspace syscall; script calls `reactor::yield_now`. Task state = Yielded; scheduler requeues at back. Phase 1 fine.
- **Should `defer_kernel_work` be reactor-controlled or scheduler-controlled?** Tock puts it in the scheduler trait; we follow suit. But Phase 1 scheduler can just return false and the reactor handles kernel work eagerly.

---

## 9. What this document does not specify
<!-- txdoc:SCHED-9-WHAT-THIS-DOCUMENT-DOES-NOT-SPECIFY -->

- **Full fair-share mathematics.** Weight tables, vruntime formulas, periodic virtual-time rebasing. Phase 2.
- **RT class admission.** SCHED_DEADLINE admission control. Phase 3.
- **Cgroup hierarchy mechanics.** Weight propagation, quota enforcement. Phase 2 cgroup spec.
- **Full cross-hart preemption policy.** Low-level SMP IPI, the initial reactor dispatch bridge, and serialized AP runqueue draining exist. This spec does not define priority-aware remote preemption, load balancing, or final per-hart reactor sharding.
- **Idle task behavior.** WFI / halt semantics. HAL concern.
- **Exact HAL timer programming.** RISC-V stimecmp interface. HAL spec.
- **Implementation data structures.** Red-black trees vs linked lists vs arrays. Implementation choice for each policy.
- **Performance tuning.** Slice length tuning per workload. Phase 2 with measurement.

---

## 10. Cross-doc corrections implied by this document
<!-- txdoc:SCHED-10-CROSS-DOC-CORRECTIONS-IMPLIED-BY-THIS-DOCUMENT -->

None. SCHEDULER_v0 is additive relative to REACTOR_v0 and PROCESS_v1. It pins what those documents defer; nothing in them needs correction.

---

## 11. Short version
<!-- txdoc:SCHED-11-SHORT-VERSION -->

> The scheduler is a policy consulted by the reactor via the `SchedulerPolicy` trait. It owns per-task scheduling metadata (class, priority, affinity, remaining budget, runtime accounting) and per-hart runqueues. It decides task selection and slice configuration; the reactor mechanizes dispatch, preemption, wake-to-IPI bridging, and context save/restore. Phase 1 is round-robin with fixed 10ms slice, two-queue dispatch (new vs preempted), cooperative for kernel tasks, initial affinity-aware placement, and placement-reported remote wake dispatch. Budget travels across sleep/wake (Zircon-style), incentivizing I/O-bound threads. Phase 2 adds priority classes (RT FIFO/RR), fair-share (weighted vruntime), cgroup CPU controller, dynamic affinity, priority-aware remote preemption, and load balancing. The trait interface is stable across phases; new policies plug in without reactor changes.
