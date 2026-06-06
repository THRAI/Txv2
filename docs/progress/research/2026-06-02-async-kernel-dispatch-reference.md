# 2026-06-02: Async kernel and runtime dispatch reference

## Purpose

After the SMP4 child-spread gate started completing `pthread-minimal1`, the next
question is not only "can we dispatch work to APs", but which dispatch model we
should be converging toward.

This note compares several async runtimes and kernels that are relevant to
txKernel's current shape:

- txKernel has stackless thread futures, `ThreadPayload`-owned userspace state,
  and reactor-owned task scheduling.
- The current experiment gives pthread children wide affinity plus
  `spread_on_submit`, but keeps them pinned after entry.
- The fixed SMP4 run validates AP observe and child-spread wiring, while the
  capture remains CPU0-heavy and does not prove broad AP userspace execution.

The goal is to extract practical patterns for the next scheduling slice, not to
pick a wholesale scheduler algorithm.

## Current Tx Baseline

Relevant local code:

- `crates/tx-reactor/src/scheduler.rs`
  - `InitialSchedMeta` carries `affinity`, `MigrationPolicy`,
    `spread_on_submit`, and `preempted_on_submit`.
  - initial placement can spread pinned tasks only when `spread_on_submit` is
    explicitly set.
  - `try_steal` and rebalance still require `can_migrate`.
- `crates/tx-kernel/src/init.rs`
  - first userspace pins to CPU0 when CPU0 is online.
  - default userspace policy remains submit-hart pinned.
  - gated child policy uses the online CPU mask plus `spread_on_submit`.
- `crates/tx-kernel/src/init/reactor_submit.rs`
  - pthread children submit through the child helper and start in the preempted
    queue.
- `docs/Txv3/10_SCHED_SMP_v1.md`
  - success target: secondary harts pull user tasks, affinity is respected,
    cross-hart wake is safe, and affinity updates can migrate tasks.

The current working posture is therefore:

| property | current status |
| --- | --- |
| AP observe | proven |
| first userspace | CPU0 pinned |
| pthread child initial placement | cfg-gated wide affinity + spread-on-submit |
| post-entry migration | disabled |
| stealing/rebalance of these children | disabled |
| proof of AP userspace payload execution | not yet established |

## Reference Survey

### Tokio

Sources:

- [Tokio runtime scheduling docs](https://docs.rs/tokio/latest/tokio/runtime/index.html)

Tokio's multi-thread runtime uses a fixed worker pool, one global queue, and one
local queue per worker. Workers prefer their local queue, periodically check the
global queue, and steal half of another worker's local queue only when local and
global queues are empty. Tokio also has a LIFO slot for wake-locality: when a
task wakes another task, the woken task can run immediately after the current
task, but the optimization is bounded and not stealable by other workers.

Spawn/dispatch shape:

1. `spawn` turns a future into a runtime task.
2. local wakes stay local when possible.
3. external wakes go through the global queue.
4. idle workers steal only when they have no local or global work.

Tx lessons:

- Tx already mirrors part of the useful shape: local queues plus stealing from a
  safe queue class.
- The LIFO-slot idea maps to Tx's `WakeHandoff`/boosted placement, but should
  stay bounded and observable. It should not bypass userspace-slot safety.
- Tokio's local/global split suggests a future Tx "inject queue" for external
  producers, but pthread child submission is currently a same-thread syscall
  producer and can keep direct target placement.
- "Steal half" is not the immediate Tx goal. Tx's current "do not steal pinned
  userspace" is correct until trap handoff and userspace slots are migration
  safe.

### async-task / smol-style primitive

Sources:

- [async-task crate docs](https://docs.rs/async-task/latest/async_task/)
- [async-task runnable source docs](https://docs.rs/async-task/latest/src/async_task/runnable.rs.html)

`async-task` is useful because it separates the primitive task object from the
executor policy. Spawning produces a `Runnable` and a `Task`. The executor
provides a schedule function, and waking the task invokes that function to put
the runnable back into an executor queue. The runnable is polled once by
`run()`. `ScheduleInfo` exposes whether a task woke itself while running, so
the scheduler can choose a special self-wake or LIFO path.

Spawn/dispatch shape:

1. allocate task state around a future,
2. bind it to a schedule function,
3. enqueue `Runnable`,
4. poll once,
5. re-enqueue only through the waker path.

Tx lessons:

- This is the cleanest conceptual match for Tx's reactor boundary: one
  canonical wake/schedule function should own requeueing.
- Tx should add observe data for "woken while polling" or "self wake" before
  optimizing wake handoff. Without that, LIFO-like tuning can hide duplicate
  queueing bugs.
- Scheduler metadata should stay separate from `ThreadPayload`; the task owns
  poll mechanics, while thread runtime owns saved userspace state.

### Embassy

Sources:

- [Embassy executor crate docs](https://docs.embassy.dev/embassy-executor/0.9.1/cortex-m/index.html)
- [Embassy Spawner docs](https://docs.rs/embassy-executor/latest/embassy_executor/struct.Spawner.html)

Embassy is not an SMP Unix kernel, but its constraints are relevant to kernel
hot paths. Tasks are statically allocated with no heap, `pool_size` controls how
many concurrent instances of a task can exist, and the executor polls only the
woken task. Multiple executors can represent priority levels; idle execution
sleeps via interrupts instead of busy polling.

Spawn/dispatch shape:

1. a `#[task]` function generates a spawn token,
2. `Spawner::spawn` publishes it into an executor,
3. the executor polls only tasks that are woken,
4. static allocation makes capacity failures explicit at build/link time or
   spawn time.

Tx lessons:

- Kernel-internal workers and AP boot tasks should prefer static or
  pre-reserved task storage where possible.
- `pool_size` is a useful mental model for bounded pthread-child stress tests:
  capacity should be visible and fail cleanly rather than creating hidden
  allocation pressure in the scheduling path.
- Multiple executors as priority levels maps to Tx's boosted/new/preempted
  queues and later scheduling classes.

### Glommio

Sources:

- [Glommio crate docs](https://docs.rs/glommio/latest/glommio/)
- [Glommio task docs](https://docs.rs/glommio/latest/glommio/task/index.html)

Glommio is a shared-nothing, thread-per-core async runtime. It supports pinning
executors to CPUs and exposes multiple task queues inside an executor. Queues
can have static shares, and tasks can be spawned into a specific queue. A task
stores state with the future; when it is woken, its schedule function pushes it
back into the queue.

Spawn/dispatch shape:

1. create one executor per thread/core,
2. optionally pin the executor to a CPU,
3. create task queues with shares and latency hints,
4. spawn tasks into a chosen queue,
5. keep scheduling local to that executor.

Tx lessons:

- Tx's current children-only `spread_on_submit` experiment is closer to
  Glommio/Seastar than to Tokio: decide placement at submit, then avoid
  post-entry movement.
- If Tx keeps some userspace classes pinned long term, that should be explicit:
  "thread-per-hart shard mode", not an accidental limitation.
- Task queue shares are a good future shape for separating foreground
  userspace, kernel maintenance, and background reclaim.

### Seastar

Sources:

- [Seastar tutorial scheduling groups](https://docs.seastar.io/master/tutorial.html)
- [Seastar futures and promises docs](https://docs.seastar.io/master/group__future-module.html)

Seastar uses per-shard reactors and continuation queues. A future becoming
ready makes its continuation ready to run. By default, each shard runs ready
continuations in readiness order, but scheduling groups give independent ready
lists and CPU shares. The shares solve a starvation problem where a component
with more ready continuations would otherwise dominate the shard.

Spawn/dispatch shape:

1. continuations become runnable when promises resolve,
2. each shard runs its own ready continuation queues,
3. scheduling groups account work independently,
4. remote-shard work is explicit.

Tx lessons:

- Tx should not measure only "number of runnable tasks"; it should measure the
  owning class or group. Pthread child count can dominate a hart even if each
  child is short.
- If AP dispatch works but only one class floods the queues, Tx needs
  per-class accounting before tuning steal or rebalance.
- Remote work should be explicit and observable; hidden migration is the wrong
  first step.

### Zircon / Fuchsia

Sources:

- [Zircon scheduler overview](https://fuchsia.dev/fuchsia-src/concepts/kernel/kernel_scheduling)
- [Fuchsia thread object reference](https://fuchsia.dev/fuchsia-src/reference/kernel_objects/thread)

Zircon's scheduler runs independently on each logical CPU, with per-CPU run
queues coordinated by IPIs. It supports fair and deadline scheduling. CPU
placement on wake considers affinity first, then last CPU/cache locality, then
idle states. Idle CPUs can steal from busy CPUs. Thread creation and execution
start are separate: a thread object is created first, then started with an
entrypoint; the first process thread is started through the process-start path.

Spawn/dispatch shape:

1. create a thread object associated with a process,
2. start it separately with entry state,
3. place on CPU at wake/start using affinity, last CPU, and idle state,
4. use per-CPU queues and IPIs for coordination,
5. support steal/load balance after thread state is safe to move.

Tx lessons:

- Tx should keep "create/publish thread identity" separate from "make runnable".
  The child must not be schedulable before `ThreadIdentity`, `ThreadPayload`,
  tid bindings, futex clear state, and task registration are published.
- Placement should not be only "least queue depth"; it needs affinity, last
  hart, and idle/AP availability as separate observable reasons.
- Zircon validates Tx's direction: per-hart queues plus IPI coordination are
  the right kernel-level architecture, but migration must be tied to thread
  state correctness.

### Linux CFS and sched_ext

Sources:

- [Linux CFS scheduler docs](https://kernel.org/doc/html/v6.0/scheduler/sched-design-CFS.html)
- [Linux sched_ext docs](https://docs.kernel.org/6.15/scheduler/sched-ext.html)
- [sched_ext overview](https://sched-ext.com/docs/OVERVIEW)

CFS models fair CPU sharing with per-task virtual runtime and a per-runqueue
time-ordered tree; it picks the runnable entity with the smallest virtual
runtime. This is a mature general-purpose scheduler with multiprocessing,
weights, sleeper handling, and group scheduling layered in. `sched_ext` is
interesting for Tx because it allows scheduler policy experiments behind a
loaded/gated mechanism; if a BPF scheduler fails or stalls runnable tasks, the
kernel aborts it and reverts to CFS.

Spawn/dispatch shape:

1. fork/clone creates a schedulable task entity,
2. wakeup chooses a CPU/runqueue,
3. CFS accounts runtime into `vruntime`,
4. pick-next chooses the leftmost/least-served runnable entity,
5. scheduler-class experiments can be gated and reverted.

Tx lessons:

- Do not rush into CFS-like trees before the trap handoff is safe. Tx is still
  validating where userspace can run.
- The sched_ext model is immediately useful: keep experimental scheduling
  behind a gate, make fallback explicit, and require stall/trap observability
  before promotion.
- When Tx does add fairness, it should account per thread and per class/group,
  not only per queue depth.

### Tock

Sources:

- [Tock overview](https://book.tockos.org/doc/overview)
- [Tock scheduling](https://book.tockos.org/doc/scheduling)

Tock uses a scheduler trait that the main kernel loop consults. The trait has
separate calls for choosing the next process, reporting why the last process
stopped and how long it ran, and deciding when to execute kernel work. Boards
can choose different schedulers. The default scheduler is preemptive
round-robin.

Spawn/dispatch shape:

1. process states distinguish runnable from yielded/waiting/faulted,
2. scheduler policy is trait-shaped and board-selectable,
3. stop feedback is passed back to policy,
4. kernel work can be interleaved by policy.

Tx lessons:

- Tx's scheduler-policy boundary is directionally right: the reactor should own
  mechanism, while policy owns placement and queue choice.
- `task_stopped` feedback should become more valuable: include "entered
  userspace", "trapped", "preempted", "blocked", and runtime in observe data.
- Keep policy swappable or gateable. This fits the current cfg experiment.

### Theseus

Sources:

- [Theseus task management](https://www.theseus-os.com/Theseus/book/subsystems/task.html)

Theseus is a Rust single-address-space OS. Tasks are closer to threads than
POSIX processes. Spawning uses a task builder; the caller supplies a function
and argument, then `spawn()` creates the task and adds it to runqueues. The task
does not execute immediately. A wrapper function is the real initial entrypoint:
it sets stack state, calls the user's entry, catches unwinding, and handles
cleanup.

Spawn/dispatch shape:

1. build task state,
2. publish it to one or more runqueues,
3. first execution enters a common wrapper,
4. wrapper calls the task entry and owns cleanup,
5. scheduler state is kept outside the task object where possible.

Tx lessons:

- This strongly matches Tx's need for a common `run_thread`/`PerHartSlotted`
  trampoline: every userspace thread should enter through one wrapper that
  sets and clears hart-local state consistently.
- A newly created child must become runnable only after semantic publication is
  complete.
- Scheduler state should remain derived policy metadata, not a field inside
  `ThreadPayload`.

## Common Patterns Worth Copying

### 1. Separate create, publish, and runnable

Zircon, Theseus, async-task, and Tx all benefit from a three-stage distinction:

1. construct semantic/task state,
2. publish identity and lifecycle facts,
3. enqueue as runnable.

For Tx pthread children, this means `step_fork`/clone must finish identity,
payload, tid binding, clear-tid/futex state, and reactor task registration
before the scheduler can make the child visible to an AP.

### 2. Keep one canonical wake-to-run path

async-task makes this explicit with the schedule function. Tokio, Glommio, and
Seastar all route readiness through executor-owned queueing. Tx should preserve
that property: all wake, clone-submit, futex wake, signal wake, and timer
preempt paths should converge on one scheduler placement function with
different `WakeHint`/metadata, not open-code queue pushes.

### 3. Use per-hart queues, but make remote work explicit

Tokio and Zircon both use per-worker/per-CPU queues with cross-thread
coordination. Glommio and Seastar go further and make sharding explicit. Tx
should expose remote placement in observe records:

- submit hart,
- target hart,
- affinity mask,
- reason (`cpu0-first-entry`, `spread_on_submit`, `last_hart`, `idle_ap`,
  `forced_affinity`, `wake_remote`),
- queue kind.

### 4. Start with initial placement before migration

Glommio/Seastar show that no-migration designs can be valid if sharding is
explicit. Tx's current child-spread gate is a good first step because it tests
AP userspace entry without exposing stale per-hart slots to arbitrary steal and
rebalance.

### 5. Add migration only at a well-defined safe point

Zircon and Linux can migrate threads because their saved execution context and
runqueue ownership model are built around that. Tx must first prove:

- `PerHartSlotted` clears old hart slots on every poll exit,
- trap return fully unwinds before a task can be placed elsewhere,
- saved userspace context is the only cross-hart resume state,
- no per-hart `KernelResumeCtx` or trap stack can be reused after migration.

### 6. Gate scheduler experiments and define fallback

Linux `sched_ext` is the right model for risky policy changes. Tx should keep
experiments behind cfg or boot parameters, and promote only after observe proves
no trap/fault/panic, no lost wakeups, no stuck runnable tasks, and acceptable
latency.

## Tx Recommendation

### Phase A: Children-only AP entry proof

Keep the current cfg-gated shape:

- first userspace stays CPU0 pinned,
- pthread children get wide affinity plus `spread_on_submit`,
- post-entry migration remains disabled.

Add observe markers:

- `debug.sched.submit.target_hart`
- `debug.sched.submit.affinity`
- `debug.sched.submit.reason`
- `debug.userspace.entry.hart`
- `debug.userspace.exit_or_trap.hart`
- `debug.child_submit.target_hart`

Success criteria:

- AP harts show `debug.userspace.entry.hart`, not only AP init counters.
- pthread-minimal1 exits 0 under SMP4.
- no loss/overwrite/repairs.
- no stale active userspace request remains on the submitting hart after a child
  is polled on another hart.

### Phase B: Wake placement without steal

Once AP child entry is proven, allow parked children to wake on their last hart
or an idle allowed hart. Do not yet steal a running/preempted userspace task.

Useful policy:

1. if affinity excludes current hart, place on first allowed/idle hart;
2. else prefer last hart for cache locality;
3. if last hart is busy and another allowed AP is idle, place on idle AP;
4. emit reason code.

This copies Zircon's affinity/last-CPU/idle ordering without enabling arbitrary
post-entry migration.

### Phase C: Migration-safe preempted queue steal

Only after Phase B proves trap and wake safety, open stealing from preempted
userspace tasks.

Required proof:

- a task is not inside `enter_userspace_with_context`;
- no active userspace request is installed on the old hart;
- `ThreadPayload.saved_user_context` is coherent;
- VM/ASID residency and remote shootdown are observed under `mmap`/`munmap`;
- steal source and destination harts both log owner transitions.

### Phase D: Real policy work

After correctness, decide fairness:

- Tokio-like local-first plus bounded steal for throughput.
- Seastar/Glommio-like scheduling groups for class isolation.
- Linux/Zircon-like virtual runtime only after there is enough workload
  diversity to justify it.

For OSComp pthread first, queue depth and wake latency probably matter more
than CFS-style fairness.

## Immediate Next Work Items

1. Add the submit/entry/exit observe markers listed above.
2. Rerun gated SMP4 `pthread-minimal1` and require at least one AP userspace
   entry marker.
3. Add a host scheduler test that `spread_on_submit` reports the selected hart
   and reason.
4. Add a `PerHartSlotted` host test for "poll on hart A, yield, then poll on
   hart B" once the test harness can switch current CPU.
5. Add a boot/cfg policy enum rather than one-off cfg names before broadening:
   `first-cpu0`, `children-spread-pinned`, `wake-spread-pinned`,
   `preempted-steal`.

## Bottom Line

Other systems converge on the same boundary: task creation is separate from
making work runnable, wakeups flow through one scheduler-owned queueing path,
and cross-core movement is safe only when execution state is explicitly
handoff-safe.

For Tx, the right next step is not full work stealing. It is a measured
children-only AP entry proof with richer observe data, followed by wake-time
placement, and only then preempted-queue stealing.
