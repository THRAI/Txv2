# Scheduler — SMP behavior — v1

<!-- txdoc:TXV3-SCHED-SMP-V1 -->

**Status.** v1 (Txv3 refresh, 2026-05).
**Purpose.** Specify the cross-hart behavior of the scheduler and reactor under SMP: task placement, cross-hart wake, work stealing, IPI rescheduling, the lock-and-recheck pattern that closes the wake-vs-steal race, and the atomic-ordering rules that compose them. Lifts the discussion from "secondary harts boot but don't run user work" to "secondary harts genuinely run user tasks, with bounded latency, and cpuset-compatible affinity."
**Audience.** Implementation lead for `tx-reactor` / `tx-substrate/src/sync`; reviewers of the SMP-bringup PR series; subsystem authors writing kthread workers.
**Companion documents.** `03_STEP_MODEL_v2.md`, `Reactor_concept_v5_RefactorSpec v4.md`, the v4 `SCHEDULER_v0.md` (which §5.3-5.5 promises this work; this doc supersedes those subsections), `06_EXECUTION_SCOPE_v1.md`.

---

## 1. Status & scope

<!-- txdoc:SCHED-SMP-V1-SCOPE-1 -->

In scope:

- Cross-hart task ownership (`current_hart` discipline).
- Cross-hart wake routing through `TaskMailbox.post`.
- Work-stealing scheduler with the lock-and-recheck invariant.
- IPI-driven reschedule and the `need_reschedule` flag plumbing.
- Initial placement policy (parent-hart + threshold fallback).
- Forced migration on affinity change (the cpuset prerequisite).
- Boot-time secondary bringup contract.

Out of scope (deferred):

- Real-hardware NUMA (FDT `numa-node-id`, distance-aware steal). Covered by separate `09_TOPOLOGY_v1.md` for `numa=fake=N`.
- Cgroup-v2 CPU controller (cpu.weight / cpu.max). Cpuset (as a process attribute) lands here; cgroup wiring comes later.
- Periodic load balancer (push-based). Work stealing covers v1.
- PMU integration for scheduler accounting (cycles, instret per task).
- Dynamic CPU hotplug.
- Real-time scheduling classes (SCHED_FIFO/RR/DEADLINE).
- Priority inheritance, donation lattice, `OnHandoff` integration.

The v1 success bar is: **secondary harts pull user tasks with parent-hart locality, affinity respected, cross-hart wake works without lost wakeups, and `sched_setaffinity` triggers correct migration.** That suffices for LTP CPU-affinity tests and unblocks cpuset.

---

## 2. Invariants

<!-- txdoc:SCHED-SMP-V1-INVARIANTS-1 -->

The numbered SCHED-SMP-* identifiers below are cite-stable for grep and review.

### SCHED-SMP-1 — current_hart is atomic and mutated only under per-hart lock

**`TaskSchedMeta.current_hart: AtomicCpuId`** identifies which hart owns the task at any instant. Mutations happen only under the lock of either the source or destination `per_hart` queue (work stealing) or the destination queue (migration on wake). Readers use `Acquire`; writers use `Release`. The field is the linearization point between physical queue membership and logical hart ownership.

### SCHED-SMP-2 — Cross-hart wake must lock destination queue and re-check current_hart under lock

The wake path closes the wake-vs-steal race by:

1. Reading `target = task.current_hart.load(Acquire)`.
2. Acquiring `per_hart[target].lock` (a `SpinMutex`; use `try_lock` with retry to avoid serializing if many wakers contend).
3. **Re-reading** `task.current_hart.load(Acquire)` under the lock; if changed, drop and retry from step 1.
4. Inserting `task_id` into `per_hart[target].new_queue`.
5. Storing `task.lifecycle = Runnable` (Release).
6. Releasing the lock.
7. Sending IPI(target, Reschedule) iff `target != current_hart()`.

Without step 3, hart B (steal) can move the task off hart A *after* hart C reads `current_hart = A` but *before* hart C acquires hart A's lock, leading to hart A and hart B both running the task.

### SCHED-SMP-3 — SchedClass::Kernel is globally unstealable

Tasks with `meta.class == SchedClass::Kernel` are pinned to their submitting hart (or the hart specified at submission). They are unstealable regardless of which queue they happen to be in. `try_steal` checks `meta.class == Kernel` **before** the affinity check and **before** acquiring the victim's lock; if Kernel, skip immediately.

Rationale: kernel work is often hart-affine for reasons orthogonal to affinity_mask (per-hart timer tail-half, per-hart reclaim worker, per-hart softirq). The framework does not model these affinities individually; `SchedClass::Kernel` is the conservative blanket rule.

### SCHED-SMP-4 — `preempted_queue` is stealable; `new_queue` and `kernel_queue` are not

`per_hart[H]` holds three queues:

- `kernel_queue`: kernel work (SchedClass::Kernel). Unstealable by SCHED-SMP-3.
- `new_queue`: freshly-submitted tasks awaiting first dispatch. Unstealable so that initial placement intent (§7) is preserved.
- `preempted_queue`: tasks that have run, yielded, and been re-queued. Stealable.

Only `preempted_queue` is exposed to `try_steal`. If a hart's `preempted_queue` is empty and `new_queue` is non-empty, the hart's local `pick_next` will draw from `new_queue` directly; thieves cannot.

### SCHED-SMP-5 — Tokio-shape LIFO-local + FIFO-steal on `VecDeque<TaskId>`

Within `preempted_queue` (a `VecDeque<TaskId>`):

- Producers (yield-resume from the local hart) push via `push_back`.
- Local `pick_next` pops via `pop_back` (most-recently-preempted task, cache-warm).
- `try_steal` pops via `pop_front` (oldest preempted task, cache-cold for everyone but least disruptive to victim's hot working set).

This minimizes cache disruption: victim keeps its hot tasks; thief takes a task whose cache state was already lost. The pattern matches Tokio's "LIFO local, FIFO steal" but on `VecDeque` rather than a Treiber stack.

### SCHED-SMP-6 — need_reschedule check at trap return and cooperative yield points

`per_hart[H].need_reschedule: AtomicBool` is the cross-hart preemption signal. Set by `on_ipi(IpiKind::Reschedule)`; checked at:

1. **Trap return** (any trap exit: IPI, page fault, syscall return, timer interrupt). Read with `Acquire`. If true, clear with `Release` and tail-call into the scheduler loop rather than resuming the interrupted context.
2. **Timer-tick handler** (slice expiry). The timer handler is itself a trap, so technically subsumed by §1, but worth naming because slice-bound reschedule lives here.
3. **Cooperative yield points in the drive loop**. Between `StepOp::step` invocations in `drive()`, the loop checks `need_reschedule` with `Relaxed` (frequent hot-path check; eventual consistency is acceptable). If true, the script's async future yields, returning `Pending` to the reactor; the reactor reschedules.

Site #3 is load-bearing for the "cooperative kernel + preemptive userspace" model: kernel-mode tasks are not interrupted by IPI, so they must check between steps. Userspace tasks are interrupted via trap, so site #1 covers them.

### SCHED-SMP-7 — Lifecycle is advisory; per-hart queue is truth

`TaskSlot.lifecycle: AtomicLifecycle` has variants `Parked / Runnable / Running / Dead`. It is **advisory** outside the per-hart lock and **truth** inside it.

Concretely: outside the lock, `lifecycle.load(Relaxed)` is a hint useful for debugging, observability, and short-circuit checks (e.g., "is this task even alive?"). Inside `per_hart[target].lock`, lifecycle and queue membership are consistent. Cross-hart wake (SCHED-SMP-2) uses the lock to gate both lifecycle transitions and queue inserts.

Implication: tools that read lifecycle without taking the lock (debug printers, perf samplers, /proc/PID/stat-equivalents) get the most-recent-published state but may be momentarily inconsistent with queue contents. This is intentional.

### SCHED-SMP-8 — WaitGeneration is monotonic-per-mailbox; increments inside park-entry critical section

`TaskMailbox.generation: AtomicU64` increments via `fetch_add(1, Release)` at every park-entry. Increment and `register_prepared(generation)` happen in the same park-entry critical section so that wakes against stale generations are correctly rejected (`runtime spec §10.2`).

Generation transitions are independent of lifecycle: lifecycle may cycle `Parked → Runnable → Parked` multiple times while generation increments monotonically.

### SCHED-SMP-9 — IPI handler is idempotent

`on_ipi(IpiKind::Reschedule)` performs exactly: `per_hart[self].need_reschedule.store(true, Release)`. Nothing else. The handler is invoked from trap context (interrupt enable not required); concurrent invocations are safe (`Release` store is idempotent; `true` stays `true`).

The handler **does not** read scheduler queues, walk task tables, or perform any allocation. All scheduling decisions are deferred to the trap-return path (SCHED-SMP-6 site #1).

### SCHED-SMP-10 — `try_steal` uses try_lock; never blocks on a victim

`try_steal` acquires victim's per-hart lock via `try_lock()`, not `lock()`. If contended (another thief in flight, or victim hart is itself manipulating the queue), the thief skips that victim and tries the next. This avoids two thieves serializing on one victim and also avoids a thief blocking the victim's own progress.

If no victim is stealable (all locks contended or all preempted_queues empty), `try_steal` returns `None` and the thief hart enters WFI (SCHED-SMP-11).

### SCHED-SMP-11 — Idle harts wait via `pause_for_ipi()`

When `pick_next` returns `None` (kernel_queue empty, new_queue empty, preempted_queue empty, all `try_steal` attempts failed), the hart calls `P::pause_for_ipi()`. On RISC-V this is `asm!("wfi")`. Hart wakes on any incoming interrupt; the trap-return path checks `need_reschedule` and re-enters `pick_next`.

Spin-idle is forbidden in production paths. (Spin is acceptable inside `try_lock` retries with `core::hint::spin_loop()`, bounded by the retry budget.)

---

## 3. Atomic ordering table

<!-- txdoc:SCHED-SMP-V1-ATOMICS-1 -->

| Field | Writer ordering | Reader ordering | Notes |
|---|---|---|---|
| `TaskSchedMeta.current_hart` | Release on migration (under lock) | Acquire on cross-hart wake; Acquire on re-check under lock | Reader-writer cross-hart |
| `TaskSlot.lifecycle` | Release inside per-hart lock | Acquire inside per-hart lock; Relaxed outside (advisory) | SCHED-SMP-7 |
| `TaskMailbox.queue.try_push` | Release on slot publish | Acquire on dequeue | Single-producer-many-consumer is impossible (one owner); MPSC bounded queue |
| `TaskMailbox.overflow_flag` | Release on set | AcqRel on swap-clear | Sticky bit |
| `TaskMailbox.generation` | Release on `fetch_add` (park-entry critical section) | Acquire on snapshot at register | SCHED-SMP-8 |
| `per_hart[H].need_reschedule` | Release on set (in `on_ipi`) | Acquire on trap return; Relaxed on cooperative yield | SCHED-SMP-6 / SCHED-SMP-9 |
| `per_hart[H].lock` | (SpinMutex; lock ordering implicit) | (SpinMutex; lock ordering implicit) | Guards new_queue, preempted_queue, and lifecycle transitions on tasks in those queues |

**Banned**: `SeqCst` outside the EBR algorithm. The EBR substrate uses `SeqCst` on its epoch-counter sequence for a specific correctness proof; the scheduler/wake path does not transitively require it. AcqRel suffices for happens-before across all scheduler-domain reads.

**Mixing ordering**: any field accessed both inside and outside a lock uses the strongest required ordering for the outside-lock case. Lifecycle outside lock is Relaxed; inside lock the lock's release/acquire semantics dominate.

---

## 4. Cross-hart wake protocol

<!-- txdoc:SCHED-SMP-V1-WAKE-PROTO-1 -->

The full protocol for `TaskMailbox::post`:

```rust
impl TaskMailbox {
    pub fn post(&self, hint: WakeHint) {
        // Step 1: enqueue WakeHint into mailbox.
        self.queue.try_push_release(hint);  // returns Err on full → set overflow flag

        // Step 2: schedule the owning task on its current_hart.
        let task_id = self.owner_task_id;  // immutable since mailbox creation
        let meta = scheduler.meta(task_id);

        'retry: loop {
            let target = meta.current_hart.load(Ordering::Acquire);

            match scheduler.try_lock_per_hart(target) {
                None => {
                    core::hint::spin_loop();
                    continue 'retry;
                }
                Some(guard) => {
                    // Recheck current_hart under lock.
                    let recheck = meta.current_hart.load(Ordering::Acquire);
                    if recheck != target {
                        drop(guard);
                        continue 'retry;
                    }

                    // Check lifecycle; avoid double-queue.
                    let lifecycle = meta.lifecycle.load(Ordering::Acquire);
                    match lifecycle {
                        Lifecycle::Parked => {
                            scheduler.per_hart[target]
                                .new_queue
                                .push_back(task_id);
                            meta.lifecycle.store(Lifecycle::Runnable, Ordering::Release);
                        }
                        Lifecycle::Runnable | Lifecycle::Running => {
                            // Already runnable / running; the hint will be
                            // observed by the drive() loop. No queue insert.
                        }
                        Lifecycle::Dead => {
                            // Defensive; mailbox is upgraded Weak so this
                            // shouldn't reach. Drop the hint silently.
                        }
                    }

                    drop(guard);
                    break;
                }
            }
        }

        // Step 3: IPI if cross-hart.
        if target != current_hart() {
            P::send_ipi(target, IpiKind::Reschedule);
        }
    }
}
```

Key points:

- **Two responsibilities**: enqueue the hint + insert into scheduler queue. Both must succeed for the task to make forward progress.
- **`try_lock` with retry**: bounded busy-wait on lock contention. The lock is held briefly (just the recheck + push + lifecycle store) so retry latency is small.
- **Recheck under lock**: closes the wake-vs-steal race (SCHED-SMP-2).
- **Lifecycle check avoids double-queue**: if the task is already runnable/running, the hint will be consumed by the drive loop without a second queue insert. This prevents the same TaskId appearing twice in the per-hart queue.

The IPI is sent **after** dropping the lock (the per-hart lock should not be held across a cross-hart synchronous SBI call).

---

## 5. Work stealing protocol

<!-- txdoc:SCHED-SMP-V1-STEAL-PROTO-1 -->

```rust
fn pick_next(scheduler: &Scheduler, hart: CpuId) -> Option<(TaskId, SliceConfig)> {
    // 1. Local kernel work (highest priority).
    if let Some(t) = scheduler.per_hart[hart].kernel_queue.pop_back() {
        return Some((t, SliceConfig::Cooperative));
    }

    // 2. Local new tasks.
    if let Some(t) = scheduler.per_hart[hart].new_queue.pop_front() {
        return Some((t, slice_for(scheduler.meta(t))));
    }

    // 3. Local preempted tasks (LIFO — newest first for cache locality).
    if let Some(t) = scheduler.per_hart[hart].preempted_queue.pop_back() {
        return Some((t, slice_for(scheduler.meta(t))));
    }

    // 4. Try to steal.
    if let Some((t, slice)) = try_steal(scheduler, hart) {
        return Some((t, slice));
    }

    // 5. Nothing to do — caller goes WFI.
    None
}

fn try_steal(scheduler: &Scheduler, thief: CpuId) -> Option<(TaskId, SliceConfig)> {
    let hart_count = scheduler.hart_count();
    let start = (thief.0 + 1) % hart_count;  // round-robin to avoid always-the-same victim

    for offset in 0..hart_count {
        let victim_idx = (start + offset) % hart_count;
        if victim_idx == thief.0 { continue; }
        let victim = CpuId(victim_idx);

        let guard = match scheduler.try_lock_per_hart(victim) {
            None => continue,  // contended — try next victim
            Some(g) => g,
        };

        // FIFO steal: take oldest preempted task.
        let candidate = scheduler.per_hart[victim].preempted_queue.pop_front();
        let task_id = match candidate {
            None => { drop(guard); continue; }
            Some(t) => t,
        };

        let meta = scheduler.meta(task_id);

        // SCHED-SMP-3: kernel tasks are unstealable.
        if meta.class == SchedClass::Kernel {
            scheduler.per_hart[victim]
                .preempted_queue
                .push_front(task_id);  // put it back at the head
            drop(guard);
            continue;
        }

        // Affinity check.
        if !meta.affinity_mask.contains(thief) {
            scheduler.per_hart[victim]
                .preempted_queue
                .push_front(task_id);  // put it back
            drop(guard);
            continue;
        }

        // Update current_hart under the victim's lock (SCHED-SMP-1).
        meta.current_hart.store(thief, Ordering::Release);

        drop(guard);
        return Some((task_id, slice_for(meta)));
    }

    None
}
```

The `pop_front` / `push_front` choice is deliberate: stealing from the front is FIFO (oldest first); putting back at the front when the steal fails (kernel/affinity) preserves the FIFO position so the victim's own next `pick_next` finds the task where it expected.

Round-robin victim start position (line 4 of `try_steal`) avoids the pathological case where all idle harts simultaneously target the same victim.

---

## 6. Lifecycle state machine

<!-- txdoc:SCHED-SMP-V1-LIFECYCLE-1 -->

```
                  ┌────────────────────────┐
                  │   task_submitted       │
                  └──────────┬─────────────┘
                             │
                             ▼
                       ┌──────────┐
                       │  Runnable │←────────────────────┐
                       └─────┬────┘                       │
                             │                             │
                pick_next    │                             │
                takes task   │            mailbox.post     │ task yields,
                             ▼            wakes parked     │ Continue or
                       ┌──────────┐       task             │ Yield
                       │  Running │                        │
                       └─────┬────┘                        │
                  ┌──────────┼──────────┐                  │
                  │          │          │                  │
                step      .step       step                 │
            returns    returns   returns                   │
              Done      Yield     Continue                 │
                  │          │          │                  │
                  ▼          ▼          └──────────────────┘
            ┌──────┐    ┌──────┐
            │  Dead│    │Parked│────► waits on YieldShape
            └──────┘    └──────┘      until mailbox.post
                              ▲              │
                              │              ▼
                              └──── re-queue on wake (SCHED-SMP-2)
```

| State | Meaning | Storage location |
|---|---|---|
| `Runnable` | TaskId is in some `per_hart[H].{new,preempted}_queue` and not currently being polled | per-hart queue |
| `Running` | The reactor is currently polling this task's future on some hart | implicit (no queue) |
| `Parked` | Future returned `Pending`; task is subscribed to some YieldShape's wait source | wait source's subscriber list (via `Weak<TaskMailbox>`); TaskSlot.future still in TaskRegistry |
| `Dead` | Task has terminated (`Done` or `Err`); TaskRegistry will reclaim slot | TaskRegistry slot, pending reclamation |

Transitions inside per-hart lock:
- `Runnable → Running`: pick_next dequeues TaskId; sets lifecycle.
- `Running → Runnable`: drive loop's `Continue` arm; task is re-pushed onto local preempted_queue.
- `Running → Parked`: drive loop sees future return `Pending`; sets lifecycle.
- `Parked → Runnable`: `mailbox.post` (SCHED-SMP-2).

Transitions outside per-hart lock (advisory writes):
- `Running → Dead`: drive loop sees `Done(T)` or `Err(E)`; sets lifecycle Release. Subsequent TaskRegistry reclamation is gated by EBR.

---

## 7. Initial placement policy

<!-- txdoc:SCHED-SMP-V1-INITIAL-PLACEMENT-1 -->

When a task is submitted (via `task_submitted` from clone/fork/kthread_spawn/etc.):

```rust
fn task_submitted(scheduler: &mut Scheduler, meta: InitialSchedMeta) -> CpuId {
    // 1. Kernel tasks have explicit hart binding.
    if meta.class == SchedClass::Kernel {
        return meta.preferred_hart.unwrap_or_else(|| pick_least_loaded(scheduler));
    }

    // 2. OnBehalfOf scope: place on the borrowed process's current hart, not the kthread's.
    if let Some(borrowed) = meta.on_behalf_of {
        let target = scheduler.meta(borrowed).current_hart.load(Acquire);
        if meta.affinity_mask.contains(target) {
            return target;
        }
        // borrowed hart is outside affinity — fall through to default.
    }

    // 3. Parent-hart preference with new_queue burst threshold.
    let threshold = scheduler.hart_count();  // SUBMIT_BURST_THRESHOLD = hart count
    if let Some(parent_hart) = meta.parent_hart {
        if meta.affinity_mask.contains(parent_hart)
            && scheduler.per_hart[parent_hart].new_queue.len() < threshold {
            return parent_hart;
        }
    }

    // 4. Fallback: least-loaded hart within affinity.
    pick_least_loaded_with_affinity(scheduler, meta.affinity_mask)
}
```

`SUBMIT_BURST_THRESHOLD = hart_count` because that means the parent hart can absorb roughly its share of new submissions before spillover begins. OBS counter `sched_smp.placement_fallback_rate` reports how often path 4 is taken; if it stays >5% under representative load, the threshold needs tuning.

For kthread/SQPOLL/AIO-worker submissions: `meta.class = SchedClass::Kernel` and `meta.preferred_hart` is the subsystem's choice. `OnBehalfOf<P>` scopes follow path 2 — the kthread lives on whatever hart its borrowed process is currently on.

---

## 8. Migration paths

<!-- txdoc:SCHED-SMP-V1-MIGRATION-1 -->

Two migration triggers exist:

**Forced migration (affinity change).** When `sched_setaffinity(tid, mask)` or a cpuset update changes a task's affinity such that its `current_hart` is no longer in the new mask:

1. The script issuing the change acquires the task's home per-hart lock.
2. Pop TaskId from whatever queue it's in (if Runnable) — if Running, set a "migrate after current slice" flag instead.
3. Pick a new hart within the new affinity (least-loaded).
4. Push TaskId onto destination per-hart's new_queue.
5. Update `current_hart` under destination lock.
6. Send IPI(destination, Reschedule).
7. If was Running: send IPI(source, Reschedule) so source picks something else.

Forced migration is synchronous from the requesting script's POV; it may cross-hart-IPI but does not wait for the IPI to be acked.

**Opportunistic migration (work stealing).** Covered in §5. The stealing hart pulls a task from a victim's preempted_queue; `current_hart` updates atomically under the victim's lock.

Migration semantics:
- `current_hart` is the only authoritative location.
- After migration, the task's home is the destination. Subsequent wakes route to destination.
- The task's mailbox does **not** move (`Cap<TaskMailbox>` stays put); `Weak<TaskMailbox>` references in wait sources are unaffected.

---

## 9. Boot protocol

<!-- txdoc:SCHED-SMP-V1-BOOT-1 -->

BSP path:

```
_start (boot trampoline)
  install_early_percpu(cpu_id = 0)   // tp ← &RV64_PERCPU_AREAS[0]
  parse_fdt() → BootInfo (with hart_count)
  parse_boot_args() → BootArgs (including numa=fake=N)
  build_topology(BootArgs, BootInfo) → publish TOPOLOGY
  init_page_allocator(per-node if numa=fake)
  init_substrate (EBR, zones, mailbox machinery)
  init_reactor (per-hart structures, scheduler)
  bring_up_secondaries(BootInfo.hart_count)    ← all at once
    for each secondary in 1..hart_count:
      SBI sbi_hart_start(secondary, entry = tx_rv64_qemu_secondary_start)
  wait_for_all_online()                         ← BSP waits until each AP marks itself online
  submit_init_task()                            ← first user task
  enter_reactor_loop()
```

Secondary path:

```
tx_rv64_qemu_secondary_start
  install_early_percpu(cpu_id)
  install_trap_vector
  P::mark_cpu_online(cpu_id)                    ← BSP waits on this
  init_on_ap (substrate-side per-hart init)
  secondary_reactor_loop()                      ← enters pick_next loop
```

**No dynamic CPU hotplug.** All harts brought up at boot; no offline/online runtime transitions.

**MAX_HARTS = 8** is the compile-time cap on per-hart static arrays. `BootInfo.hart_count` is the FDT-detected count; bringup loops use `min(BootInfo.hart_count, MAX_HARTS)`. Exceeding MAX_HARTS at boot logs a warning and clamps to MAX_HARTS; harts beyond the cap are parked in their entry trampoline's WFI loop (already implemented at `boards/tx-hal-riscv64-qemu-virt/src/boot_trampoline.rs:185`).

---

## 10. Deferred items

<!-- txdoc:SCHED-SMP-V1-DEFERRED-1 -->

The following are intentionally deferred from v1 with stated trigger criteria:

**`recently_stolen` anti-ping-pong flag.** Land if OBS detects a TaskId being stolen ≥3 times in 1 second across distinct hart-pairs. Counter: `sched_smp.task_steal_per_second` keyed on TaskId; alert event: `sched_smp.ping_pong_detected`. Until triggered, no flag is maintained.

**NUMA-aware steal preference.** When `numa=fake=N > 1`, steal first from within-node harts, then cross-node. Requires §9_TOPOLOGY's hart-to-node table. Lands with the cpuset memory-policy work.

**Periodic load balancer.** A push-based balancer running on a designated hart (or rotating) could complement work stealing under sustained imbalance. Defer until benchmarks show stealing alone is insufficient.

**RT class support (SCHED_FIFO / SCHED_DEADLINE).** Requires `OnHandoff` for PI futex; out of v1 scope. v1 has `SchedClass::{Normal, Kernel}` only.

**`OnHandoff` integration.** Reserved YieldShape for ownership transfer (PI futex, rt-mutex). When landed, scheduler donates priority across the OnHandoff dependency graph. Not in v1.

**PMU-driven accounting.** Per-task cycles/instret/etc. via Sscofpmf or SBI-PMU. Independent axis; lands separately.

**Dynamic CPU hotplug.** No CPU offline/online runtime transitions. Static bringup at boot only.

**Migration latency targets.** v1 has no SLA on migration latency; opportunistic stealing reaches the new hart "eventually." If RT/latency-sensitive workloads land, latency targets get measured and possibly forced-migration latency bounded.

---

## 11. Test acceptance criteria

<!-- txdoc:SCHED-SMP-V1-TESTS-1 -->

Per-week milestones for the three-week landing plan:

**Week 1 (Foundation).** `IPI Reschedule` handler sets `need_reschedule`; trap-return checks and reschedules; `current_hart` field added; per-hart SpinMutex on queues.

- Acceptance test: QEMU `-smp 2`, two independent CPU-bound busybox processes; both harts at ~100% utilization for ≥1 second.
- Failure mode: hart 1 idle the entire time → IPI Reschedule handler not wired correctly.

**Week 2 (Cross-hart wake).** `mailbox.post` wires `scheduler.task_runnable_on`; lock-and-recheck pattern implemented; lifecycle transitions under per-hart lock.

- Acceptance test: pipe-pair test. Hart 0 reader blocked on `read(pipe_fd)`; hart 1 writer issues `write(pipe_fd, ...)`; reader resumes on hart 0 with the written bytes.
- Failure mode: reader resumes on hart 1 (didn't honor current_hart) or never resumes (wake didn't enqueue).

**Week 3 (Work stealing).** `try_steal` implemented with lock-and-recheck; stealable scope restricted (preempted_queue only, non-kernel, affinity-respected); LIFO-local + FIFO-steal direction.

- Acceptance test: load-skew test. Submit 4 CPU-bound tasks all to hart 0; after ≤10ms, observe at least 2 distinct harts running tasks; after ≤100ms, observe all 4 harts loaded.
- Failure mode: tasks remain concentrated on hart 0 → stealing not triggered or affinity check failing incorrectly.

**Integration acceptance (post-week-3).** `sched_setaffinity(tid, {hart 2 only})` then `read /proc/<tid>/stat`'s `processor` field → reports 2 (or, equivalently, observe via OBS that the task executes only on hart 2 over the next N slices).

---

## 12. Cross-references

<!-- txdoc:SCHED-SMP-V1-XREF-1 -->

| Concept | Definition lives in |
|---|---|
| `TaskMailbox`, `WakeHint`, `WaitGeneration`, `WaitSource` | `Reactor_concept_v5_RefactorSpec v4.md` §4–5 |
| `StepOp`, `StepOutcome`, `Yield`, `Continue`, `Done`, `Err` | `03_STEP_MODEL_v2.md` §2, §3 |
| `YieldShape::OnWaitSource / OnAgent / OnTimer` | `03_STEP_MODEL_v2.md` §2.3 |
| `SubjectContext`, `OnBehalfOf<P>` (kthread on-behalf placement) | `04_SYSCALL_SHAPE_v1.md`, `06_EXECUTION_SCOPE_v1.md` |
| `WaitProtocol`, `Interruptibility`, `AbortReason` | runtime spec §8.3, §12 |
| `WAIT-1` (conditional registration linearization point) | `02_INVARIANTS_v5.md` |
| `DTOK-3` (token state machine, reply-vs-timeout race) | `02_INVARIANTS_v5.md`, runtime spec §11.7 |
| `HartLocal<T>` | `crates/tx-hal/src/hart_local.rs:126-156` |
| `Rv64PerCpuArea` | `boards/tx-hal-riscv64-qemu-virt/src/lib.rs:79-105` |
| `Topology`, `numa=fake=N` parsing | `09_TOPOLOGY_v1.md` (forthcoming) |

The v4 `SCHEDULER_v0.md` §5.3 (NUMA awareness), §5.4 (dynamic CPU affinity), §5.5 (load balancing) are superseded by this doc for their respective phase-2 commitments.

---

## 13. Open questions for follow-on revisions

<!-- txdoc:SCHED-SMP-V1-OPEN-1 -->

- **`SUBMIT_BURST_THRESHOLD` exact value.** Hard-coded to `hart_count` in v1; benchmarks may suggest `hart_count * k` for some k.
- **Steal direction parameterization.** v1 commits to LIFO-local + FIFO-steal. If a benchmark shows FIFO-local + LIFO-steal (Tokio's old shape) is better for some workload, revisit.
- **need_reschedule check inside `try_steal`.** Currently not specified. A thief that succeeds in stealing then sees its own `need_reschedule` may want to skip running the stolen task and re-pick. Open: is this worth the complexity?
- **Multi-victim stealing in one `try_steal` call.** v1 picks one victim and one task. Open: scan all victims and pick the busiest? At 4–6 harts the linear scan is cheap; flag whether the policy change is worth the spec churn.
- **Wake without IPI when target is self.** Already specified (`if target != current_hart()` skips IPI). Worth ensuring the trap-return path still picks up the new queue entry without an IPI when the producer is the destination hart itself. Implementation detail; flag for code-review checklist.
