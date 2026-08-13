# SMP PELT Scheduler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:test-driven-development`, `tx-design-reference`,
> `tx-progress-memory`, and `tx-xtask`. Execute task-by-task with
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans`. Do not enable `movable`, active `pelt`, or
> segmented storage before the stage-specific guest and measurement gates pass.

**Goal:** turn the existing AP/reactor SMP bring-up into production-safe
multi-hart userspace scheduling, then improve placement with event-driven
PELT-lite and shard task storage only when measured lock contention justifies it.

**Architecture:** The reactor owns one authoritative `TaskControl` per stable
task slot and one physical runqueue per hart. Wake, queue, owner, affinity, poll
lease, and userspace-run transitions are generation/epoch checked; idle harts
pull only old `Preempted` work and directly acquire its poll lease. PELT is a
Q32, event-driven advisory signal that begins in shadow mode and cannot affect
correctness or slice accounting.

**Tech Stack:** Rust `no_std`, `tx-reactor`, `tx-kernel`, `tx-subsystems`
ThreadRuntime, RV64 QEMU/SBI IPI and trap return, `tx-observe`, test-init guest
witnesses, `cargo xtask`, and JSON progress records.

---

## Authoritative Inputs

- Approved design:
  `docs/superpowers/specs/2026-08-04-smp-pelt-scheduler-design.md`.
- Existing reactor ownership design:
  `docs/superpowers/specs/2026-07-14-reactor-refactor-design.md`.
- Active SMP contract: `docs/Txv3/10_SCHED_SMP_v1.md`.
- Active execution contracts:
  `docs/design/02_execution/{REACTOR_v0,SCHEDULER_v0,reactor_scheduling,THREAD_RUNTIME_v1}.md`.
- Current baseline evidence:
  `docs/progress/research/2026-08-04-smp-scheduler-readiness-audit.md`.

When wording conflicts, update the active docs in Task 1 before changing code.
Do not treat the progress audit or either superpowers document as a replacement
for the active `docs/design/` and `docs/Txv3/` contracts.

## File Map

- `crates/tx-reactor/src/task.rs`: stable slot directory, generation checks,
  terminal drain, and compatibility facade over authoritative transitions.
- `crates/tx-reactor/src/task/control.rs`: `TaskControl`, `RunToken`,
  `PollLease`, queue/lease epochs, and the only lifecycle/owner transition API.
- `crates/tx-reactor/src/scheduler.rs`: pure queue class, placement, budget,
  affinity, victim, and migration decisions; no authoritative owner copy.
- `crates/tx-reactor/src/scheduler/pelt.rs`: Q32 decay and task/hart PELT state.
- `crates/tx-reactor/src/runtime.rs`: per-hart queue application, hart drive,
  direct steal-to-poll, wake routing, and poll transaction integration.
- `crates/tx-reactor/src/wake_ingress.rs`: generation-bearing per-hart MPSC
  fallback, latch clear/recheck, and bounded overflow handling.
- `crates/tx-reactor/src/{dispatch,preempt,hart_loop}.rs`: coalesced reschedule
  state and scheduler-loop arbitration.
- `crates/tx-reactor/src/userspace.rs`: `{TaskKey, run_seq, hart}` userspace run
  tokens and slot validation.
- `crates/tx-reactor/tests/{task_lifecycle,scheduler,reactor_smoke,userspace_run,smp_model,pelt}.rs`:
  host and deterministic race-model gates.
- `crates/tx-kernel/src/{trap,trap_handoff,thread_future}.rs`: Reschedule IPI
  trap action, exact userspace token handoff, and full old-hart unwind.
- `crates/tx-kernel/src/init.rs`, `init/boot_args.rs`, `init/reactor_submit.rs`,
  and `init/tests.rs`: immutable boot modes, idle/WFI handshake, affinity seam,
  and static/movable user-task metadata.
- `crates/tx-subsystems/src/thread_runtime/structure.rs`: per-hart identity,
  payload, and run-token slots owned by ThreadRuntime.
- `crates/tx-subsystems/src/reactor_affinity.rs` and
  `crates/tx-shims/src/linux_syscall/{proc,tests}.rs`: Linux-visible affinity
  errors and forced-migration semantics.
- `schema/txobserve.toml` and generated L0 artifacts:
  stable scheduler event families. Regenerate through the repository schema
  command; do not hand-edit generated files.
- `tools/shell-tests/smp-scheduler-witness.c`: one static RV64 guest witness
  with `static`, `movable`, `affinity`, `pipe`, `timer`, and `mixed` modes.
- `xtask/src/test.rs`: `smp-scheduler-witness` lane, marker parsing, serial
  retention, and fault-decode chaining.
- `tools/sched-pelt-ab.py` and `tools/tests/test_sched_pelt_ab.py`: matched A/B
  validation and S4/S5 promotion calculations.
- `docs/progress/research/2026-08-04-smp-pelt-baseline.md`: exact build/image,
  score, metrics, capture integrity, and promotion receipts.
- `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json` and
  `docs/progress/STATUS.md`: durable stage state, next step, and blockers.

## Fixed Contracts Used By Every Task

```rust
pub const BASE_SLICE_NS: u64 = 10_000_000;
pub const NEW_QUEUE_SLICE_NS: u64 = 1_000_000;
pub const AGING_PROMOTION_TURNS: u64 = 8;
pub const AGED_STREAK_LIMIT: u8 = 1;
pub const BOOST_BURST_LIMIT: u8 = 4;
pub const STEAL_SCAN_LIMIT: usize = 8;
pub const WAKE_LOCK_RETRY_LIMIT: usize = 32;
```

Physical queue endpoints have one meaning throughout the implementation:

```text
front = remote/cold/old end
back  = local/hot end

Kernel local FIFO:       push_back -> pop_front
Boosted local FIFO:      push_back -> pop_front
New local FIFO:          push_back -> pop_front
Preempted local LIFO:    push_back -> pop_back
Preempted remote FIFO:   inspect/pop_front
```

Boot behavior is immutable:

```text
tx.sched.smp=legacy|static|movable
tx.sched.load=depth|pelt-shadow|pelt
```

`legacy + depth` remains the operational rollback until S3 is accepted.
`static` pins each userspace task after initial placement. `movable` requires
the full userspace unwind and migration gates. `pelt` requires S4 evidence.

## Task 1: Align The Canonical Scheduler Contracts

**Files:**

- Modify: `docs/Txv3/10_SCHED_SMP_v1.md`
- Modify: `docs/design/02_execution/REACTOR_v0.md`
- Modify: `docs/design/02_execution/SCHEDULER_v0.md`
- Modify: `docs/design/02_execution/reactor_scheduling.md`
- Modify: `docs/design/02_execution/THREAD_RUNTIME_v1.md`
- Test: `docs/superpowers/specs/2026-08-04-smp-pelt-scheduler-design.md`

- [ ] **Step 1: Add grep-stable ownership and queue tags**

Add these exact anchors to the owning sections:

```markdown
<!-- txdoc:SCHED-SMP-TASK-CONTROL-AUTHORITY -->
<!-- txdoc:SCHED-SMP-QUEUE-ENDPOINTS -->
<!-- txdoc:SCHED-SMP-WAKE-RECHECK -->
<!-- txdoc:SCHED-SMP-USERSPACE-MIGRATION-SAFE -->
<!-- txdoc:SCHED-SMP-PELT-ADVISORY -->
```

State that `TaskControl` owns lifecycle, execution owner, queue epoch, and poll
lease; scheduler state owns policy only; physical run tokens carry generation,
queue epoch, and class.

- [ ] **Step 2: Replace direction and rebalance conflicts**

Replace physical `front` language for local handoff with `local/hot end`, state
the endpoint table from this plan, restrict stealing to `Preempted`, and remove
the active recommendation for the periodic 4 ms push rebalance. Preserve the
10 ms base and 1 ms new-task slices.

- [ ] **Step 3: Add migration and PELT precedence**

Document static-before-movable rollout, complete trap unwind before migration,
forced migration for Parked/Queued/Polling, and PELT as advisory placement and
victim input only. Explicitly exclude VM/pmap and adaptive-slice changes.

- [ ] **Step 4: Verify links and stale vocabulary**

Run:

```bash
cargo xtask lint docs
rg -n "periodic.*4ms|steal.*New|steal.*Boosted|preempted_queue\.pop_front\(\).*local" \
  docs/Txv3/10_SCHED_SMP_v1.md docs/design/02_execution
git diff --check
```

Expected: docs lint passes; the targeted `rg` prints no active conflicting
contract; diff check prints nothing.

- [ ] **Step 5: Commit the contract alignment**

```bash
git add docs/Txv3/10_SCHED_SMP_v1.md \
  docs/design/02_execution/REACTOR_v0.md \
  docs/design/02_execution/SCHEDULER_v0.md \
  docs/design/02_execution/reactor_scheduling.md \
  docs/design/02_execution/THREAD_RUNTIME_v1.md
git commit -m "docs: align SMP scheduler contracts"
```

## Task 2: Introduce Authoritative TaskControl, RunToken, And PollLease

**Files:**

- Create: `crates/tx-reactor/src/task/control.rs`
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/lib.rs`
- Modify: `crates/tx-reactor/src/spin_lock.rs`
- Test: `crates/tx-reactor/tests/task_lifecycle.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`

- [ ] **Step 1: Write failing authority and stale-token tests**

Add focused tests named:

```rust
#[test] fn one_valid_run_token_acquires_exactly_one_poll_lease();
#[test] fn stale_queue_epoch_cannot_acquire_poll_lease();
#[test] fn stale_lease_epoch_cannot_commit_poll();
#[test] fn terminal_slot_cannot_recycle_with_active_lease();
#[test] fn stale_generation_token_cannot_poll_reused_slot();
```

Each test must assert both the returned error and unchanged authoritative
`TaskControl` snapshot.

- [ ] **Step 2: Run the tests and confirm red**

```bash
cargo test -p tx-reactor --test task_lifecycle one_valid_run_token -- --nocapture
cargo test -p tx-reactor --test reactor_smoke stale_queue_epoch -- --nocapture
```

Expected: compile failure because `RunToken`, `PollLease`, and transition APIs
do not exist.

- [ ] **Step 3: Add the exact authority types**

Define these types in `task/control.rs` and re-export only the types needed by
runtime/tests:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunToken {
    pub key: TaskKey,
    pub queue_epoch: u32,
    pub queue: Phase1QueueKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskRunOwner {
    Parked,
    Queued { hart: HartId, queue: Phase1QueueKind, queue_epoch: u32 },
    Polling { hart: HartId, lease_epoch: u32 },
    Terminal,
}

pub(crate) struct PollLease {
    pub(crate) key: TaskKey,
    pub(crate) hart: HartId,
    pub(crate) lease_epoch: u32,
    pub(crate) observed_wake_seq: u64,
    pub(crate) future: TaskFuture,
}

pub(crate) struct TaskControl {
    pub(crate) future: Option<TaskFuture>,
    pub(crate) lifecycle: TaskStatus,
    pub(crate) owner: TaskRunOwner,
    pub(crate) lease_epoch: u32,
    pub(crate) queue_epoch: u32,
    pub(crate) observed_wake_seq: u64,
    pub(crate) cancel_requested: bool,
    pub(crate) active_userspace: Option<UserspaceRunRequest>,
    pub(crate) sched: TaskSchedState,
    pub(crate) last_stop_reason: Option<StopReason>,
}
```

Rename `TaskSchedMeta` to `TaskSchedState` and keep these policy fields under
the control lock in this task: class, affinity, migration policy, kernel/user
classification, submit-spread flag, remaining/current budget, total runtime,
last-hart hint, queued turn, wake class, and scheduling flags. Task 12 adds the
PELT field. Do not keep lifecycle, owner, queued membership, queue epoch, or
lease state in this sidecar.

Use checked epoch increments. Epoch exhaustion transitions the affected task
to a terminal invariant failure and never reuses epoch zero.

- [ ] **Step 4: Put TaskControl behind the stable slot**

Change `TaskSlot` to hold generation/routing hints plus one control lock:

```rust
struct TaskSlot {
    generation: TaskGeneration,
    current_hart: AtomicU16,
    lifecycle_hint: AtomicU8,
    control: SpinLock<Option<TaskControl>>,
    wake_state: Option<Arc<TaskWakeState>>,
    mailbox: Option<Arc<TaskMailbox>>,
}
```

Keep the first implementation's generation-checked `Vec<TaskSlot>` and free
list. Do not segment storage in this task.

- [ ] **Step 5: Replace take/finish calls with one poll transaction**

Add APIs with these signatures:

```rust
pub(crate) fn acquire_poll(
    &self,
    hart: HartId,
    token: RunToken,
) -> Result<PollLease, AcquirePollError>;

pub(crate) fn commit_poll(
    &self,
    lease: PollLease,
    result: PollResult,
) -> Result<CommitAction, CommitPollError>;
```

`acquire_poll` validates key, generation, queue epoch, queue class, owner hart,
and lifecycle under one control lock; increments lease epoch; moves out the
Future; records wake sequence; and sets `Polling`. `commit_poll` validates key
and lease epoch before applying exactly one Ready, Cancelled, Woken, Parked,
Yielded, UserspaceTrap, or SliceExpired transition. Polling occurs after the
lock is released.

Add `SpinLock::try_lock() -> Option<SpinLockGuard<'_, T>>` using one Acquire
compare-exchange. It performs no spinning and is the only lock primitive used
by bounded wake and steal retries.

- [ ] **Step 6: Remove scheduler owner authority**

Delete `owner`, `queued`, and `must_migrate_on_stop` as authoritative fields
from `TaskSchedMeta`. Keep affinity, class, budget, wake class, migration
policy, last-hart hint, queue turn, and later PELT state in `TaskSchedState`
under `TaskControl`.

- [ ] **Step 7: Run focused and compatibility tests**

```bash
cargo test -p tx-reactor --test task_lifecycle -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --test scheduler -- --nocapture
```

Expected: all tests pass; existing lifecycle behavior is preserved; stale
generation/queue/lease tests pass.

- [ ] **Step 8: Commit the authority core**

```bash
git add crates/tx-reactor/src/task.rs crates/tx-reactor/src/task/control.rs \
  crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/runtime.rs \
  crates/tx-reactor/src/spin_lock.rs \
  crates/tx-reactor/src/lib.rs crates/tx-reactor/tests/task_lifecycle.rs \
  crates/tx-reactor/tests/reactor_smoke.rs crates/tx-reactor/tests/scheduler.rs
git commit -m "refactor: centralize reactor task authority"
```

## Task 3: Make Queue Membership And Local Ordering Linearizable

**Files:**

- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/task/control.rs`
- Test: `crates/tx-reactor/tests/scheduler.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`

- [ ] **Step 1: Replace queue-direction tests with the approved contract**

Add or rename tests to assert:

```rust
#[test] fn local_preempted_dispatch_is_lifo();
#[test] fn remote_preempted_candidate_is_fifo();
#[test] fn new_boosted_and_kernel_queues_are_not_stealable();
#[test] fn one_valid_token_matches_owner_and_queue_epoch();
#[test] fn four_boosted_picks_force_one_ready_fair_pick();
#[test] fn aged_and_handoff_picks_do_not_starve_new_queue();
```

Delete the expectation in
`work_stealing_can_take_new_queue_but_respects_affinity`; New work must not be
stealable.

- [ ] **Step 2: Confirm old direction fails**

```bash
cargo test -p tx-reactor --test scheduler local_preempted_dispatch_is_lifo -- --nocapture
cargo test -p tx-reactor --test scheduler new_boosted_and_kernel_queues_are_not_stealable -- --nocapture
```

Expected: the first test sees current `pop_front`; the second sees current New
or Boosted stealing.

- [ ] **Step 3: Store RunToken in every physical queue**

Change `HartRunQueues` to:

```rust
pub(crate) struct HartRunQueues {
    kernel_queue: VecDeque<RunToken>,
    boosted_queue: VecDeque<RunToken>,
    new_queue: VecDeque<RunToken>,
    preempted_queue: VecDeque<RunToken>,
    nr_running: u32,
    boosted_streak: u8,
    aged_streak: u8,
}
```

All enqueue paths increment queue epoch and publish owner while holding the
target shard then task-control lock. All valid dequeue paths remove the token
and acquire the poll lease under that same lock order. Stale tokens are
discarded and counted without changing `nr_running` twice.

- [ ] **Step 4: Implement exact local selection order**

Use:

```text
Kernel FIFO
-> Boosted FIFO, limited to BOOST_BURST_LIMIT while fair work exists
-> aged Preempted oldest, limited to AGED_STREAK_LIMIT while New exists
-> WakeHandoff Preempted oldest, same streak limit
-> New FIFO
-> Preempted LIFO
```

Boost and handoff change order only. They do not replenish
`remaining_budget_ns`. Keep 10 ms/1 ms budget behavior and carry budget across
sleep, remote wake, and migration.

- [ ] **Step 5: Remove periodic push rebalance from the drive path**

Delete calls to `rebalance_at` and `last_balance_ns` from production hart
driving. Keep a compatibility stats field reporting zero only until external
consumers are migrated; do not schedule any 4 ms rebalance timer.

- [ ] **Step 6: Run scheduler/runtime tests**

```bash
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
```

Expected: all tests pass with exact LIFO/FIFO and starvation bounds; no test
expects New/Boosted steal or a periodic rebalance move.

- [ ] **Step 7: Commit queue ownership and ordering**

```bash
git add crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/runtime.rs \
  crates/tx-reactor/src/task/control.rs crates/tx-reactor/tests/scheduler.rs \
  crates/tx-reactor/tests/reactor_smoke.rs
git commit -m "refactor: linearize reactor runqueue ownership"
```

## Task 4: Add Lock-And-Recheck Wake And Per-Hart Ingress

**Files:**

- Create: `crates/tx-reactor/src/wake_ingress.rs`
- Modify: `crates/tx-reactor/src/waker.rs`
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `crates/tx-reactor/src/task/control.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/lib.rs`
- Test: `crates/tx-reactor/tests/task_lifecycle.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`
- Test: `crates/tx-reactor/tests/smp_model.rs`

- [ ] **Step 1: Write wake race tests first**

Create deterministic tests for these orders:

```text
wake -> pending commit
pending commit -> wake
wake reads source owner -> queued migration -> wake lock/recheck
wake reads victim owner -> direct steal -> wake lock/recheck
ingress drain reads seq -> producer increments seq -> drain clears latch
32 failed shard/control try-lock attempts -> one fallback token
terminal drain while ingress owns the generation -> recycle rejected
```

Assert one valid run token, one owner, and no lost wake for every order.

- [ ] **Step 2: Confirm at least the latch-clear test fails**

```bash
cargo test -p tx-reactor --test smp_model wake_latch_clear_race_requeues_one_token -- --nocapture
```

Expected: compile failure because per-hart `HartWakeIngress` does not exist.

- [ ] **Step 3: Add generation-bearing wake state and ingress**

Use this shape:

```rust
pub(crate) struct WakeToken { pub(crate) key: TaskKey }

pub(crate) struct TaskWakeState {
    key: TaskKey,
    wake_seq: AtomicU64,
    ingress_queued: AtomicBool,
    ingress_hart: AtomicU16,
    // existing mailbox/waker fields remain
}

pub(crate) struct HartWakeIngress {
    queue: SpinLock<VecDeque<WakeToken>>,
}
```

Reserve each per-hart queue to the task-slot high-water mark during submit, so
wake enqueue does not allocate. Because `ingress_queued` coalesces each live
task generation, required capacity is the current task-slot count. If reserve
fails during submit, fail the submit before publishing the task.

- [ ] **Step 4: Implement lock/recheck routing**

For at most `WAKE_LOCK_RETRY_LIMIT` attempts:

```text
read current_hart Acquire
try_lock target shard
try_lock TaskControl
recheck current_hart, generation, lifecycle, and owner
if owner changed: release and retry
if Parked: create exactly one new placement/token
if Queued: upgrade class/order without creating a second valid token
if Polling: wake_seq is sufficient; commit observes it
release locks
arm target reschedule after locks
```

After 32 failed attempts, set the task ingress latch and enqueue one
generation-bearing token to the currently observed hart. The consumer repeats
the same lock/recheck protocol.

- [ ] **Step 5: Close the ingress latch-clear race**

Drain in this exact order:

```text
read wake_seq
route authoritative transition
ingress_queued.store(false, Release)
reread wake_seq Acquire
if sequence advanced, CAS latch false -> true and enqueue once
```

A stale generation clears only its own physical token; it cannot consume the
new generation's wake sequence.

- [ ] **Step 6: Keep compatibility raw Wakers explicit**

Before S5, raw Wakers without an owner-aware reactor handle may still enter the
shared compatibility queue. Drain immediately routes them into the per-hart
protocol. Mark this path with `debug.sched.wake.route=compat_shared`; do not
remove it in S1.

- [ ] **Step 7: Run wake and lifecycle gates**

```bash
cargo test -p tx-reactor --test task_lifecycle -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --test smp_model -- --nocapture
```

Expected: all wake-order permutations pass; repeated wakes coalesce; stale
Wakers remain harmless after slot reuse.

- [ ] **Step 8: Commit wake routing**

```bash
git add crates/tx-reactor/src/wake_ingress.rs crates/tx-reactor/src/waker.rs \
  crates/tx-reactor/src/task.rs crates/tx-reactor/src/task/control.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/src/scheduler.rs \
  crates/tx-reactor/src/lib.rs crates/tx-reactor/tests/task_lifecycle.rs \
  crates/tx-reactor/tests/reactor_smoke.rs crates/tx-reactor/tests/smp_model.rs
git commit -m "feat: add owner-checked per-hart wake ingress"
```

## Task 5: Coalesce Reschedule IPIs And Close The WFI Window

**Files:**

- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/dispatch.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/hart_loop.rs`
- Modify: `crates/tx-kernel/src/trap.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Modify: `crates/tx-kernel/src/init/helpers.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`
- Test: `crates/tx-reactor/tests/hart_loop.rs`
- Test: `crates/tx-kernel/src/trap.rs`
- Test: `crates/tx-kernel/src/init/tests.rs`

- [ ] **Step 1: Add failing coalescing and trap-action tests**

Add tests named:

```rust
#[test] fn reschedule_ipi_ack_returns_reschedule();
#[test] fn repeated_remote_publish_sends_one_ipi_until_ack();
#[test] fn polling_idle_suppresses_ipi_but_observes_first_recheck();
#[test] fn wake_after_polling_clear_sends_ipi_before_wfi();
#[test] fn ipi_ack_rearm_race_does_not_lose_reschedule();
```

- [ ] **Step 2: Verify the current Reschedule behavior fails**

```bash
cargo test -p tx-kernel-riscv64-qemu-virt reschedule_ipi_ack_returns_reschedule -- --nocapture
```

Expected: assertion mismatch, current action is `TrapAction::Resume`.

- [ ] **Step 3: Add per-hart reschedule atomics**

Add to the hart-local scheduling state:

```rust
need_resched: AtomicBool,
userspace_preempt: AtomicBool,
ipi_armed: AtomicBool,
polling_idle: AtomicBool,
```

Publishing remote work stores `need_resched=true`. After the required ordering
barrier, suppress the IPI only while `polling_idle` is true; otherwise send only
when `ipi_armed.compare_exchange(false, true, AcqRel, Acquire)` succeeds.

- [ ] **Step 4: Make the trap handler request arbitration**

Change the Reschedule branch to:

```rust
if P::pending_ipi(IpiKind::Reschedule) {
    P::ack_ipi(IpiKind::Reschedule);
    crate::init::ack_boot_reactor_reschedule(P::current_cpu_id());
    action = TrapAction::Reschedule;
}
```

`ack_boot_reactor_reschedule` clears `ipi_armed`, stores `need_resched`, and
does bounded atomic work only. It does not lock, allocate, poll, or send an IPI.

- [ ] **Step 5: Implement the two-recheck idle handshake**

Replace the current clear-then-caller-WFI sequence with:

```text
polling_idle=true Release; barrier
if need_resched || ingress nonempty || valid local work:
    polling_idle=false Release; return WorkPublished
polling_idle=false Release; barrier
if need_resched || ingress nonempty || valid local work:
    return WorkPublished
P::pause_for_ipi()
```

The function that performs the second recheck must also invoke WFI; callers
must not insert work between the recheck and WFI.

- [ ] **Step 6: Run host gates**

```bash
cargo test -p tx-kernel-riscv64-qemu-virt ipi_tests -- --nocapture
cargo test -p tx-kernel-riscv64-qemu-virt init::tests -- --nocapture
cargo test -p tx-reactor --test hart_loop -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
```

Expected: all tests pass; sent/coalesced/suppressed decisions are exact.

- [ ] **Step 7: Commit IPI and idle correctness**

```bash
git add crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/dispatch.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/src/hart_loop.rs \
  crates/tx-reactor/tests/reactor_smoke.rs crates/tx-reactor/tests/hart_loop.rs \
  crates/tx-kernel/src/trap.rs crates/tx-kernel/src/init.rs \
  crates/tx-kernel/src/init/helpers.rs crates/tx-kernel/src/init/tests.rs
git commit -m "fix: close SMP reschedule and idle races"
```

## Task 6: Close The S1 Correctness Gate

**Files:**

- Modify: `crates/tx-reactor/tests/smp_model.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Modify: `xtask/src/qemu.rs`
- Modify: `xtask/src/test.rs`
- Create: `docs/progress/research/2026-08-04-smp-pelt-baseline.md`
- Modify: `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Exhaust bounded model permutations**

Model these actors: poll commit, wake router, queued migration, steal attempt,
cancel/drain, and ingress latch clear. Enumerate all event permutations with a
fixed small state machine and assert:

```text
valid_run_tokens <= 1
active_poll_leases <= 1
wake_seq > observed_wake_seq implies queued || polling_commit_will_queue
terminal implies no valid token and no active lease
```

- [ ] **Step 2: Run the complete host reactor matrix**

```bash
cargo test -p tx-reactor --test task_lifecycle -- --nocapture
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-reactor --test dispatch -- --nocapture
cargo test -p tx-reactor --test hart_loop -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --test sync_coord -- --nocapture
cargo test -p tx-reactor --test smp_model -- --nocapture
```

Expected: every test passes; no invariant counter is nonzero.

- [ ] **Step 3: Run the unchanged legacy RV64 SMP4 witnesses**

```bash
cargo xtask test smoke --target rv64-qemu --timeout-ms 60000
cargo xtask test busybox-boot --target rv64-qemu --timeout-ms 60000
cargo xtask fault-decode --target rv64-qemu \
  --serial target/qemu-rv64-qemu-smoke.serial.log --all --brief
```

Expected: smoke and busybox pass on SMP4; fault-decode reports no unhandled
trap/panic. Legacy mode remains pinned and reports zero periodic rebalance.

- [ ] **Step 4: Record the S1 receipt**

In the baseline research file, record commit, kernel/image hashes, exact
commands, model-test counts, QEMU markers, serial path, and any blocker. Set the
JSON `s1-correctness` step to `completed` only after both host and guest gates
pass.

- [ ] **Step 5: Validate and commit S1 evidence**

```bash
cargo xtask progress validate
cargo xtask lint docs
git diff --check
git add crates/tx-reactor/tests/smp_model.rs crates/tx-kernel/src/init.rs \
  xtask/src/qemu.rs xtask/src/test.rs \
  docs/progress/research/2026-08-04-smp-pelt-baseline.md \
  docs/progress/plans/2026-08-04-smp-pelt-scheduler.json \
  docs/progress/STATUS.md
git commit -m "test: close SMP scheduler S1 correctness"
```

## Task 7: Add Immutable Boot Modes And Static Userspace Placement

**Files:**

- Modify: `crates/tx-kernel/src/init/boot_args.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Modify: `crates/tx-kernel/src/init/reactor_submit.rs`
- Modify: `crates/tx-kernel/src/init/tests.rs`
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `xtask/src/qemu.rs`
- Modify: `xtask/src/test.rs`
- Create: `tools/shell-tests/smp-scheduler-witness.c`

- [ ] **Step 1: Write exact boot parser tests**

Add parser cases for absent values, all valid values, duplicate keys with the
repository's existing first-match rule, invalid values, and profile coexistence:

```rust
assert_eq!(sched_modes_from_cmdline_str(""), Ok((Legacy, Depth)));
assert_eq!(sched_modes_from_cmdline_str("tx.sched.smp=static tx.sched.load=pelt-shadow"), Ok((Static, PeltShadow)));
assert_eq!(sched_modes_from_cmdline_str("tx.sched.smp=movable tx.sched.load=pelt"), Ok((Movable, Pelt)));
assert!(sched_modes_from_cmdline_str("tx.sched.smp=other").is_err());
```

- [ ] **Step 2: Verify parser tests fail before implementation**

```bash
cargo test -p tx-kernel-riscv64-qemu-virt sched_modes_from_cmdline -- --nocapture
```

Expected: compile failure because `SchedSmpMode` and `SchedLoadMode` do not
exist.

- [ ] **Step 3: Add immutable mode enums**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedSmpMode { Legacy, Static, Movable }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedLoadMode { Depth, PeltShadow, Pelt }
```

Parse once in `BootArgs`; store in boot reactor configuration. Invalid explicit
values fail boot with a diagnostic rather than silently selecting another mode.

- [ ] **Step 4: Map user-task metadata by mode**

Use this exact mapping:

```text
legacy:  affinity=submit_hart, migration=Pinned, spread=false
static:  affinity=online_harts, migration=Pinned, spread=true
movable: affinity=online_harts, migration=Movable, spread=true
```

In static mode, initial placement chooses the minimum `(depth, nr_running,
round_robin_distance)` and then records `PinnedAfterPlacement`. An affinity
narrowing that excludes that owner returns `ENOSYS` and leaves the old mask
unchanged until S3 is accepted.

- [ ] **Step 5: Add the static-spread guest witness mode**

The RV64 static binary must create four independent CPU-bound clone children,
wait for all four, read each child's CPU through `getcpu`, and print one line:

```text
smp-sched:static:pass tasks=4 distinct=4 mask=0xf exits=4 errors=0
```

Add `smp-scheduler-witness` to `xtask test`; compile the source as a static
RV64 binary, append it to test-init, boot with `-smp 4
tx.sched.smp=static tx.sched.load=depth`, require exactly one pass marker, and
retain the serial log.

- [ ] **Step 6: Run S2 host and guest gates**

```bash
cargo test -p tx-kernel-riscv64-qemu-virt sched_modes_from_cmdline -- --nocapture
cargo test -p tx-reactor --test scheduler pinned_spread_on_submit -- --nocapture
cargo xtask test smp-scheduler-witness --target rv64-qemu \
  --case static --timeout-ms 60000
cargo xtask fault-decode --target rv64-qemu \
  --serial target/qemu-rv64-qemu-smoke.serial.log --all --brief
```

Expected: parser and scheduler tests pass; the guest reports four distinct
harts, clean exits, and no fault-decode finding.

- [ ] **Step 7: Commit static placement**

```bash
git add crates/tx-kernel/src/init/boot_args.rs crates/tx-kernel/src/init.rs \
  crates/tx-kernel/src/init/reactor_submit.rs crates/tx-kernel/src/init/tests.rs \
  crates/tx-reactor/src/scheduler.rs xtask/src/qemu.rs xtask/src/test.rs \
  tools/shell-tests/smp-scheduler-witness.c
git commit -m "feat: add static SMP userspace placement"
```

## Task 8: Make Userspace Run Tokens Migration-Safe

**Files:**

- Modify: `crates/tx-reactor/src/userspace.rs`
- Modify: `crates/tx-reactor/src/task/control.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/lib.rs`
- Modify: `crates/tx-subsystems/src/thread_runtime/structure.rs`
- Modify: `crates/tx-kernel/src/thread_future.rs`
- Modify: `crates/tx-kernel/src/trap_handoff.rs`
- Test: `crates/tx-reactor/tests/userspace_run.rs`
- Test: `crates/tx-subsystems/src/thread_runtime/tests.rs`
- Test: `crates/tx-kernel/src/thread_future/tests.rs`
- Test: `crates/tx-kernel/src/trap_handoff/tests.rs`

- [ ] **Step 1: Write stale hart/run-sequence tests**

Add tests named:

```rust
#[test] fn userspace_completion_requires_task_run_seq_and_hart();
#[test] fn timer_preempt_clears_old_hart_slots_before_requeue();
#[test] fn stale_old_hart_trap_cannot_resolve_new_run();
#[test] fn migration_safe_requires_inactive_resume_context();
#[test] fn userspace_entry_rejects_affinity_mismatch();
```

- [ ] **Step 2: Confirm current request-only validation fails**

```bash
cargo test -p tx-reactor --test userspace_run userspace_completion_requires_task_run_seq_and_hart -- --nocapture
```

Expected: compile failure because `UserspaceRunToken` does not exist.

- [ ] **Step 3: Replace request identity with the full token**

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct UserspaceRunToken {
    pub task: TaskKey,
    pub run_seq: u64,
    pub hart: HartId,
}
```

`UserspaceRunSlot::start_request(task, hart)` increments `run_seq`; every
dispatch, timer preempt, interesting trap, cancel, and completion validates the
entire token. Keep `UserspaceRunRequest` only as a temporary public alias in the
same commit if workspace consumers require a migration window; remove the alias
before Task 11.

- [ ] **Step 4: Add ThreadRuntime's per-hart run-token slot**

Add set/get/clear APIs parallel to identity and payload slots. One entry
transaction publishes matching identity, payload, and token on the current
hart. A nonempty previous slot is an invariant failure.

- [ ] **Step 5: Unwind every trap path before making the task stealable**

For syscall, fault, timer preempt, and Reschedule IPI:

```text
capture registers into ThreadPayload
resolve/record the exact UserspaceRunToken
return fully to the thread future
close the active userspace slot
clear old hart identity, payload, and token slots
prove the old kernel resume context inactive
mark TaskControl migration_safe=true
only then publish Preempted RunToken
```

Remove the current timer-preempt exception that retains old identity/payload
slots in movable mode. Static/legacy may use the same cleanup protocol.

An exact task/run-sequence/hart mismatch must not return to userspace: debug
builds fail the invariant immediately; release builds terminate the affected
task and emit durable token, expected hart, observed hart, and trap-cause
diagnostics.

- [ ] **Step 6: Gate userspace entry**

Immediately before `enter_userspace_with_context`, validate TaskKey generation,
poll lease, `current_hart`, affinity, absent `MUST_MIGRATE`, and matching local
run token. Failure returns preserve-and-reschedule; it never executes `sret` on
a disallowed/stale hart.

- [ ] **Step 7: Run the userspace ownership matrix**

```bash
cargo test -p tx-reactor --test userspace_run -- --nocapture
cargo test -p tx-subsystems thread_runtime -- --nocapture
cargo test -p tx-kernel-riscv64-qemu-virt thread_future -- --nocapture
cargo test -p tx-kernel-riscv64-qemu-virt trap_handoff -- --nocapture
```

Expected: all tests pass; timer preemption leaves all old-hart userspace slots
empty before a valid Preempted token exists.

- [ ] **Step 8: Commit userspace migration safety**

```bash
git add crates/tx-reactor/src/userspace.rs crates/tx-reactor/src/task/control.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/src/lib.rs \
  crates/tx-reactor/tests/userspace_run.rs \
  crates/tx-subsystems/src/thread_runtime/structure.rs \
  crates/tx-subsystems/src/thread_runtime/tests.rs \
  crates/tx-kernel/src/thread_future.rs crates/tx-kernel/src/trap_handoff.rs \
  crates/tx-kernel/src/thread_future/tests.rs crates/tx-kernel/src/trap_handoff/tests.rs
git commit -m "feat: validate migration-safe userspace runs"
```

## Task 9: Add Idle-First Direct Steal

**Files:**

- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/task/control.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-reactor/src/hart_loop.rs`
- Test: `crates/tx-reactor/tests/scheduler.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`
- Test: `crates/tx-reactor/tests/smp_model.rs`

- [ ] **Step 1: Write direct-steal tests before changing behavior**

Add tests named:

```rust
#[test] fn steal_runs_only_after_local_queues_and_ingress_are_empty();
#[test] fn steal_scans_only_preempted_front();
#[test] fn steal_returns_poll_lease_without_thief_enqueue();
#[test] fn rejected_candidates_keep_original_fifo_order();
#[test] fn steal_preserves_remaining_budget();
#[test] fn wake_racing_direct_steal_routes_to_thief_or_commit();
```

- [ ] **Step 2: Confirm old steal behavior fails**

```bash
cargo test -p tx-reactor --test scheduler steal_returns_poll_lease_without_thief_enqueue -- --nocapture
cargo test -p tx-reactor --test scheduler steal_scans_only_preempted_front -- --nocapture
```

Expected: current code re-enqueues on the thief and/or scans New/Boosted.

- [ ] **Step 3: Separate victim policy from the steal transaction**

Before PELT activation, choose victims by descending
`(preempted_depth, round_robin_distance)`. Skip self, offline harts, and zero
depth. Keep victim selection snapshot-only; correctness is revalidated under
the victim shard and candidate task-control locks.

- [ ] **Step 4: Implement one direct steal transaction**

Use this algorithm:

```text
require local valid queues empty and local ingress empty
try_lock victim shard; on contention continue to next victim
inspect at most STEAL_SCAN_LIMIT tokens from Preempted.front
for each token, try_lock TaskControl; on contention preserve token order
validate generation, queue epoch, owner, lifecycle, affinity, Movable,
         non-kernel, migration_safe, and no active userspace token
on success:
    remove token and decrement victim nr_running
    set owner=Polling { hart=thief, lease_epoch=next }
    current_hart.store(thief, Release)
    move Future into PollLease
    release all locks
    return PollLease directly to hart drive
restore rejected valid tokens in original order
```

Do not acquire or enqueue a thief-side run token. Do not send an IPI to the
thief; it is already running the scheduler loop.

- [ ] **Step 5: Preserve budget and anti-ping-pong state**

The stolen `TaskSchedState` keeps `remaining_budget_ns`, `queued_turn`, and
task PELT history. Replace the old `recently_stolen` queue heuristic with the
stronger fact that a Polling task is not stealable; after commit it may be
stolen again only as a newly published Preempted token.

- [ ] **Step 6: Run direct-steal and race gates**

```bash
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --test smp_model wake_racing_direct_steal -- --nocapture
```

Expected: all tests pass; every successful steal immediately yields one lease,
and no thief queue depth increase occurs.

- [ ] **Step 7: Commit direct stealing**

```bash
git add crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/task/control.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/src/hart_loop.rs \
  crates/tx-reactor/tests/scheduler.rs crates/tx-reactor/tests/reactor_smoke.rs \
  crates/tx-reactor/tests/smp_model.rs
git commit -m "feat: add idle-first direct work stealing"
```

## Task 10: Implement Forced Affinity Migration

**Files:**

- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/task/control.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-subsystems/src/reactor_affinity.rs`
- Modify: `crates/tx-kernel/src/init/reactor_submit.rs`
- Modify: `crates/tx-kernel/src/thread_future.rs`
- Modify: `crates/tx-shims/src/linux_syscall/proc.rs`
- Test: `crates/tx-reactor/tests/scheduler.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`
- Test: `crates/tx-reactor/tests/smp_model.rs`
- Test: `crates/tx-shims/src/linux_syscall/tests.rs`

- [ ] **Step 1: Write state-specific migration tests**

Add tests for:

```rust
#[test] fn empty_effective_affinity_returns_invalid_mask_without_publish();
#[test] fn parked_task_changes_home_without_run_token();
#[test] fn queued_task_moves_one_valid_token_under_ordered_shard_locks();
#[test] fn polling_task_sets_must_migrate_and_moves_at_commit();
#[test] fn self_affinity_change_cannot_return_on_disallowed_hart();
#[test] fn affinity_racing_steal_moves_at_thief_commit();
#[test] fn terminal_affinity_update_returns_esrch();
```

- [ ] **Step 2: Confirm the Parked and self-migration cases fail**

```bash
cargo test -p tx-reactor --test scheduler parked_task_changes_home_without_run_token -- --nocapture
cargo test -p tx-reactor --test reactor_smoke self_affinity_change_cannot_return_on_disallowed_hart -- --nocapture
```

Expected: current metadata-only implementation does not move Parked ownership
and has no userspace-entry exclusion proof.

- [ ] **Step 3: Compute effective affinity before publication**

Use `effective = requested & online_harts`. Empty is `InvalidMask`/`EINVAL`.
Unknown or terminal task is `NoSuchThread`/`ESRCH`. In static mode, excluding
the pinned owner returns `NotInstalled`/`ENOSYS` and leaves the old affinity
unchanged. In movable mode, publish the new mask only as part of the
state-specific transaction below.

- [ ] **Step 4: Implement Parked, Queued, and Polling transactions**

```text
Parked:
  lock old/destination shards in ascending HartId, then TaskControl
  recheck generation, current_hart, lifecycle, and effective mask
  update current_hart and affinity; create no token

Queued:
  lock source/destination shards in ascending HartId, then TaskControl
  recheck token generation/epoch/owner
  remove one valid source token, decrement source nr_running
  update current_hart and affinity, increment queue epoch
  append destination token at the class-appropriate local/hot end
  increment destination nr_running

Polling:
  under TaskControl publish affinity and MUST_MIGRATE
  set owner hart need_resched and userspace_preempt
  poll commit chooses an allowed destination and publishes there
```

Internal lock contention retries/yields. It never becomes Linux-visible
`EBUSY`.

- [ ] **Step 5: Gate userspace return on forced migration**

The userspace-entry checkpoint returns preserve-and-reschedule when affinity
excludes the current hart or `MUST_MIGRATE` is set. Commit preserves remaining
budget and selects the allowed online hart with minimum depth until PELT is
active.

- [ ] **Step 6: Run scheduler, race, and syscall gates**

```bash
cargo test -p tx-reactor --test scheduler set_affinity -- --nocapture
cargo test -p tx-reactor --test reactor_smoke affinity -- --nocapture
cargo test -p tx-reactor --test smp_model affinity_racing -- --nocapture
cargo test -p tx-shims dispatch_sched_setaffinity -- --nocapture
```

Expected: all tests pass with exact EINVAL/ESRCH/ENOSYS mappings and no
post-boundary execution on a disallowed hart.

- [ ] **Step 7: Commit forced migration**

```bash
git add crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/task/control.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/tests/scheduler.rs \
  crates/tx-reactor/tests/reactor_smoke.rs crates/tx-reactor/tests/smp_model.rs \
  crates/tx-subsystems/src/reactor_affinity.rs \
  crates/tx-kernel/src/init/reactor_submit.rs crates/tx-kernel/src/thread_future.rs \
  crates/tx-shims/src/linux_syscall/proc.rs crates/tx-shims/src/linux_syscall/tests.rs
git commit -m "feat: enforce affinity migration at safe points"
```

## Task 11: Close The S3 Movable Userspace Gate

**Files:**

- Modify: `tools/shell-tests/smp-scheduler-witness.c`
- Modify: `tools/test-init/tx-test-init.sh`
- Modify: `xtask/src/test.rs`
- Modify: `xtask/src/qemu.rs`
- Modify: `docs/progress/research/2026-08-04-smp-pelt-baseline.md`
- Modify: `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Complete all movable witness modes**

The single static RV64 witness binary must print exactly one result line per
selected case:

```text
smp-sched:movable:pass first_two_ms_mask=0x7 first_100ms_mask=0xf errors=0
smp-sched:pipe:pass iterations=10000 wakes=10000 lost=0 p50_ns=N p99_ns=N
smp-sched:affinity:pass moves=1000 disallowed_after_boundary=0 errors=0
smp-sched:timer:pass preempts=1000 stale_slot_consumes=0 errors=0
smp-sched:pthread:pass creates=1000 joins=1000 errors=0 elapsed_ns=N
smp-sched:mixed:pass cpu_progress=4 io_progress=10000 starved=0 errors=0
smp-sched:stress:pass tokens=0 double_poll=0 owner=0 generation=0 errors=0
```

`N` is parsed as an unsigned decimal and retained in the receipt. The
movable-skew case starts all four workers on hart 0 and requires at least two
distinct harts by 10 ms and all four by 100 ms.

- [ ] **Step 2: Make xtask validate fields, not just substrings**

Add a case enum and per-case parser. Reject duplicate pass lines, missing
fields, nonzero error counters, affinity leakage, stale slot consumption, and
invariant counters. Preserve serial output at
`target/sched/s3/<case>.serial.log`.

- [ ] **Step 3: Run each movable case on SMP4**

```bash
cargo xtask test smp-scheduler-witness --target rv64-qemu --case movable --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case pipe --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case affinity --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case timer --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case pthread --timeout-ms 120000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case mixed --timeout-ms 120000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case stress --timeout-ms 120000
```

Expected: every case passes under `tx.sched.smp=movable
tx.sched.load=depth`.

- [ ] **Step 4: Fault-decode every serial log**

```bash
for serial in target/sched/s3/*.serial.log; do
  cargo xtask fault-decode --target rv64-qemu --serial "$serial" --all --brief
done
```

Expected: no unhandled trap or kernel panic in any log.

- [ ] **Step 5: Record and validate S3 acceptance**

Record commit/image hashes, host identity, exact marker fields, fault-decode
result, and mode. Mark S2 and S3 complete in JSON only after all required
cases pass. Choose whether the repository default remains `legacy` or becomes
`movable` as a separate documented decision; S3 evidence permits promotion but
does not silently change the default.

- [ ] **Step 6: Commit the S3 witness and receipt**

```bash
cargo xtask progress validate
cargo xtask lint docs
git diff --check
git add tools/shell-tests/smp-scheduler-witness.c \
  tools/test-init/tx-test-init.sh xtask/src/test.rs xtask/src/qemu.rs \
  docs/progress/research/2026-08-04-smp-pelt-baseline.md \
  docs/progress/plans/2026-08-04-smp-pelt-scheduler.json \
  docs/progress/STATUS.md
git commit -m "test: accept movable SMP userspace scheduling"
```

## Task 12: Implement Event-Driven Q32 PELT-Lite

**Files:**

- Create: `crates/tx-reactor/src/scheduler/pelt.rs`
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/task/control.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Create: `crates/tx-reactor/tests/pelt.rs`

- [ ] **Step 1: Write fixed-point vectors first**

Cover zero elapsed time, one period, 16 periods, 32 periods, 64 periods, long
decay to zero, signal saturation, backlog above capacity, and regressing time.
Use exact integer expectations; do not compare with floating-point tolerance.

- [ ] **Step 2: Confirm PELT tests fail before the module exists**

```bash
cargo test -p tx-reactor --test pelt -- --nocapture
```

Expected: compile failure because `scheduler::pelt` is absent.

- [ ] **Step 3: Add constants and the exact Q32 lookup**

```rust
pub const PELT_PERIOD_NS: u64 = 1_024_000;
pub const PELT_HALF_LIFE_PERIODS: u64 = 32;
pub const SCHED_CAPACITY: u64 = 1024;
pub const Q32_ONE: u64 = 4_294_967_296;

pub const DECAY_Q32: [u64; 33] = [
    4294967296, 4202935003, 4112874773, 4024744348, 3938502376,
    3854108391, 3771522796, 3690706840, 3611622603, 3534232978,
    3458501653, 3384393094, 3311872529, 3240905930, 3171459999,
    3103502151, 3037000500, 2971923842, 2908241642, 2845924021,
    2784941738, 2725266179, 2666869345, 2609723834, 2553802834,
    2499080105, 2445529972, 2393127307, 2341847524, 2291666561,
    2242560872, 2194507417, 2147483648,
];
```

- [ ] **Step 4: Implement allocation-free decay and carry**

`PeltAvg` stores `avg`, `last_update_ns`, and `valid`. For monotonic time:

```text
periods = (now - last_update_ns) / PELT_PERIOD_NS
last_update_ns += periods * PELT_PERIOD_NS
groups = periods / 32
partial = periods % 32
factor = groups >= 64 ? 0 : DECAY_Q32[partial] >> groups
avg = (avg * factor + signal * (Q32_ONE - factor)) >> 32
```

Use `u128` products and saturating conversion. A regressing timestamp leaves
the previous average unchanged, marks the decision sample invalid, and causes
that event to use depth fallback.

- [ ] **Step 5: Integrate task and hart state transitions**

Task signals are `Polling=(1024,1024)`, `Queued=(0,1024)`, and
`Parked/Terminal=(0,0)`. `TaskPelt` follows the task. `HartPelt` integrates util
1024 while executing and runnable input equal to 1024 for the executing task
plus 1024 per authoritative queued task. Migration updates source and
destination input but never transfers hart history.

- [ ] **Step 6: Publish advisory snapshots**

Publish `util_avg`, `runnable_avg`, and `nr_running` atomically per hart. Define:

```rust
pub fn pressure(util_avg: u64, runnable_avg: u64) -> u64 {
    util_avg.saturating_add(runnable_avg.saturating_sub(SCHED_CAPACITY))
}
```

No correctness check may read a PELT snapshot.

- [ ] **Step 7: Run PELT and scheduler tests**

```bash
cargo test -p tx-reactor --test pelt -- --nocapture
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
```

Expected: all fixed-point vectors and existing scheduling semantics pass.

- [ ] **Step 8: Commit PELT accounting**

```bash
git add crates/tx-reactor/src/scheduler/pelt.rs \
  crates/tx-reactor/src/scheduler.rs crates/tx-reactor/src/task/control.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/tests/pelt.rs \
  crates/tx-reactor/tests/scheduler.rs crates/tx-reactor/tests/reactor_smoke.rs
git commit -m "feat: add event-driven PELT accounting"
```

## Task 13: Add PELT Shadow Decisions And Scheduler Observability

**Files:**

- Modify: `schema/txobserve.toml`
- Modify: generated `crates/tx-observe/src/l0_schema/schema_catalog.rs` via codegen
- Modify: generated `tools/tx-observe-host-catalog.json` via codegen
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Test: `crates/tx-observe/tests/smoke.rs`
- Test: `crates/tx-reactor/tests/scheduler.rs`

- [ ] **Step 1: Add schema tests/names before emitters**

Register these stable families with fixed integer payloads:

```text
debug.sched.hart.util_avg
debug.sched.hart.runnable_avg
debug.sched.runqueue.depth
debug.sched.placement.choice
debug.sched.pelt.shadow_disagree
debug.sched.wake.route
debug.sched.wake.duplicate
debug.sched.wake_to_dispatch_ns
debug.sched.ipi.sent
debug.sched.ipi.coalesced
debug.sched.ipi.suppressed_idle
debug.sched.ipi_to_dispatch_ns
debug.sched.steal.attempt
debug.sched.steal.success
debug.sched.steal.reject_reason
debug.sched.migration.reason
debug.sched.migration.duration_ns
debug.sched.token.stale
debug.sched.owner.retry
```

- [ ] **Step 2: Regenerate and check schema artifacts**

```bash
cargo xtask observe-schema codegen
cargo xtask observe-schema check
```

Expected: codegen updates both generated catalogs; check passes without stale
artifact errors.

- [ ] **Step 3: Implement placement and victim shadow decisions**

For each initial placement and idle victim selection, compute both depth and
PELT candidates from the same event snapshot. In `pelt-shadow`, execute the
depth choice and emit both choices plus disagreement. In `depth`, avoid PELT
decision events except sampled hart snapshots. Before S4, explicit
`tx.sched.load=pelt` fails boot with a clear unsupported-promotion diagnostic.

Use the approved placement tuple:

```text
minimum (pressure, nr_running, round_robin_distance)
preferred/parent retained only within capacity/8 pressure and +1 nr_running
```

Victims use descending `(pressure, nr_running, round_robin_distance)` and still
validate candidates under locks.

- [ ] **Step 4: Keep emit paths bounded**

Use fixed-size schema payloads, no allocation, and sampling for high-frequency
queue snapshots. Correlate wake and dispatch by task key/generation and a
monotonic wake sequence; correlate migration by task key and queue epoch.

- [ ] **Step 5: Test shadow has zero behavioral effect**

Run identical scripted scheduler events through depth and pelt-shadow. Assert
identical selected hart, queue, slice, budget, and victim; assert shadow emits
the candidate/disagreement record.

- [ ] **Step 6: Run schema and shadow gates**

```bash
cargo xtask observe-schema check
cargo test -p tx-observe --test smoke -- --nocapture
cargo test -p tx-reactor --test scheduler pelt_shadow -- --nocapture
cargo test -p tx-reactor --test pelt -- --nocapture
```

Expected: all pass; depth and shadow behavior are byte-for-byte equivalent in
the model output.

- [ ] **Step 7: Commit shadow mode and events**

```bash
git add schema/txobserve.toml crates/tx-observe/src/l0_schema/schema_catalog.rs \
  tools/tx-observe-host-catalog.json crates/tx-reactor/src/scheduler.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-kernel/src/init.rs \
  crates/tx-observe/tests/smoke.rs crates/tx-reactor/tests/scheduler.rs
git commit -m "feat: add PELT shadow scheduling metrics"
```

## Task 14: Run Matched S4 A/B And Promote Active PELT Conditionally

**Files:**

- Create: `tools/sched-pelt-ab.py`
- Create: `tools/tests/test_sched_pelt_ab.py`
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Modify: `xtask/src/test.rs`
- Modify: `docs/progress/research/2026-08-04-smp-pelt-baseline.md`
- Modify: `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Write A/B validator tests**

The validator accepts ten run directories in `A1 B1 A2 B2 A3 B3 A4 B4 A5
B5` order. Tests must reject mismatched parent, image, hart count, loop count,
metrics cfg, host-load class, incomplete runtime JSON, missing workload marker,
and fewer than five pairs.

- [ ] **Step 2: Implement exact promotion calculations**

For each pair, calculate:

```text
imbalance_area = integral(max_hart_pressure - min_hart_pressure) over sample time
throughput = completed_work / elapsed_ns
wake_p99 = p99(debug.sched.wake_to_dispatch_ns)
```

Promotion passes only when median paired imbalance-area reduction is at least
20%, median paired throughput ratio `pelt/depth` is at least 0.98, and aggregate
wake p99 ratio is at most 1.05. Report every pair and the median; never average
away a missing or incomplete run.

- [ ] **Step 3: Capture five interleaved pairs**

Use one parent/candidate commit pair, one image hash, `-smp 4`, fixed loop
counts, and controlled host load. Run score and metrics separately:

```bash
python3 tools/sched-pelt-ab.py prepare \
  --out target/sched/s4 --pairs 5 --smp 4 --loops 10000
python3 tools/sched-pelt-ab.py run --matrix target/sched/s4/matrix.json
python3 tools/sched-pelt-ab.py evaluate \
  --matrix target/sched/s4/matrix.json \
  --report target/sched/s4/report.json
```

Expected: all ten `runtime.json` files have `complete=true`, zero lost,
overwrite, and framing errors; report states pass or fail with each threshold.

- [ ] **Step 4: Promote only on a passing report**

If the report passes, allow `tx.sched.load=pelt` to use PELT initial placement
and victim ordering. If it fails, keep `pelt` rejected at boot and retain
`pelt-shadow`; record which gate failed. In either result, affinity,
generation, owner, lifecycle, queue epoch, lease, and migration-safe checks
remain authoritative.

- [ ] **Step 5: Rerun S3 correctness under the candidate mode**

```bash
cargo xtask test smp-scheduler-witness --target rv64-qemu --case stress \
  --sched-load pelt --timeout-ms 120000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case mixed \
  --sched-load pelt --timeout-ms 120000
```

Expected on promotion: both pass. On non-promotion: xtask reports the recorded
S4 gate failure and does not claim an active PELT run.

- [ ] **Step 6: Record and commit the S4 decision**

```bash
python3 -m unittest tools.tests.test_sched_pelt_ab
cargo xtask progress validate
cargo xtask lint docs
git diff --check
git add tools/sched-pelt-ab.py tools/tests/test_sched_pelt_ab.py \
  crates/tx-reactor/src/scheduler.rs crates/tx-kernel/src/init.rs xtask/src/test.rs \
  docs/progress/research/2026-08-04-smp-pelt-baseline.md \
  docs/progress/plans/2026-08-04-smp-pelt-scheduler.json docs/progress/STATUS.md
git commit -m "perf: evaluate active PELT scheduling"
```

## Task 15: Measure The S5 Shared-Lock Trigger

**Files:**

- Modify: `Cargo.toml`
- Modify: `crates/tx-reactor/src/spin_lock.rs`
- Modify: `crates/tx-reactor/src/task.rs`
- Modify: `crates/tx-reactor/src/scheduler.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Modify: `docs/progress/research/2026-08-04-smp-pelt-baseline.md`
- Modify: `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`

- [ ] **Step 1: Add a reactor-local observed lock facade**

Register `cfg(tx_lock_metrics_reactor)` in workspace check-cfg. Under global
`tx_lock_metrics` plus the local cfg, route selected locks through
`tx_substrate::SpinMutex<T, LockMetricsOn>` with these names:

```text
debug.lock.task_table.directory
debug.lock.task_control
debug.lock.scheduler.meta
debug.lock.runqueue
debug.lock.wake.compat_shared
debug.lock.wake.hart_ingress
```

Do not change lock structure in this task.

- [ ] **Step 2: Verify the lock facade is cfg-gated**

```bash
RUSTFLAGS="--cfg tx_lock_metrics --cfg tx_lock_metrics_reactor" \
  cargo test -p tx-reactor lock_metrics -- --nocapture
cargo test -p tx-reactor lock_metrics -- --nocapture
```

Expected: observed build reports enabled selected locks; default build reports
disabled with unchanged type behavior.

- [ ] **Step 3: Capture five S4-mode lock runs**

Run the accepted load mode on movable SMP4 with stress, pthread, and mixed
workloads. Require complete zero-loss captures and report wait/service/response
p50/p95/p99 plus total wait for every named lock.

- [ ] **Step 4: Apply the implementation trigger**

Proceed to Task 16 only when both conditions hold in at least two workloads:

```text
(task_table.directory + scheduler.meta + wake.compat_shared) total wait
    >= 5% of summed task runtime
and at least one of those locks has wait p99 >= 10 us
```

If the trigger does not hold, set the S5 sharding JSON step to `canceled` with
the note `measurement gate did not justify sharding`, preserve the shared
directory/compat raw-wake path, and proceed to Task 17. Do not implement
segmented storage for an unmeasured theoretical gain.

- [ ] **Step 5: Commit measurement support and receipt**

```bash
cargo xtask progress validate
git diff --check
git add Cargo.toml crates/tx-reactor/src/spin_lock.rs \
  crates/tx-reactor/src/task.rs crates/tx-reactor/src/scheduler.rs \
  crates/tx-reactor/src/runtime.rs \
  docs/progress/research/2026-08-04-smp-pelt-baseline.md \
  docs/progress/plans/2026-08-04-smp-pelt-scheduler.json
git commit -m "perf: measure reactor shared-lock pressure"
```

## Task 16: Shard Task Storage And Raw Wake Only If S5 Triggered

**Precondition:** Task 15 recorded a passing implementation trigger. If it did
not, mark this task `canceled` in the JSON plan with the measurement result and
make no code changes.

**Files:**

- Modify: `Cargo.toml`
- Modify: `crates/tx-reactor/src/task.rs`
- Create: `crates/tx-reactor/src/task/directory.rs`
- Modify: `crates/tx-reactor/src/task/control.rs`
- Modify: `crates/tx-reactor/src/waker.rs`
- Modify: `crates/tx-reactor/src/wake_ingress.rs`
- Modify: `crates/tx-reactor/src/runtime.rs`
- Test: `crates/tx-reactor/tests/task_lifecycle.rs`
- Test: `crates/tx-reactor/tests/reactor_smoke.rs`
- Test: `crates/tx-reactor/tests/smp_model.rs`

- [ ] **Step 1: Write segmented-directory and raw-wake tests**

Add tests for stable segment publication, generation-safe reuse, concurrent
lookup during segment growth, old-Waker rejection, terminal recycle gates, and
all raw Wakers routing without the compatibility shared queue.

- [ ] **Step 2: Add the compile-time candidate shape**

Under `cfg(tx_reactor_segmented_tasks)`, implement:

```rust
const SEGMENT_SHIFT: usize = 8;
const SEGMENT_SIZE: usize = 1 << SEGMENT_SHIFT;
const MAX_SEGMENTS: usize = 256;
const ALLOC_SHARDS: usize = 16;

struct TaskDirectory {
    segments: [AtomicPtr<TaskSegment>; MAX_SEGMENTS],
    alloc_shards: [SpinLock<FreeList>; ALLOC_SHARDS],
}
```

Segments allocate only during submit, publish once with Release, and never
move. Lookup loads the segment with Acquire, checks generation, and then locks
only the selected `TaskControl`. Allocation/recycle uses the task-id-selected
free-list shard.

- [ ] **Step 3: Enforce terminal recycling gates**

Recycle only when authoritative terminal state has no valid run token, poll
lease, active userspace token, or wake-ingress ownership for that generation,
and Future/completion drain state is released. Increment generation before
publishing the slot to a free list.

- [ ] **Step 4: Remove the compatibility raw-wake hot path**

Every `TaskWakeState` carries its generation and current ingress routing.
Route raw wakes directly to a per-hart ingress; on owner change, the consumer
lock/rechecks and forwards after releasing locks. Delete the shared wake queue
only after workspace `rg` proves no producer retains it.

- [ ] **Step 5: Run correctness parity in both storage builds**

```bash
cargo test -p tx-reactor --test task_lifecycle -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --test smp_model -- --nocapture
RUSTFLAGS="--cfg tx_reactor_segmented_tasks" \
  cargo test -p tx-reactor --test task_lifecycle -- --nocapture
RUSTFLAGS="--cfg tx_reactor_segmented_tasks" \
  cargo test -p tx-reactor --test reactor_smoke -- --nocapture
RUSTFLAGS="--cfg tx_reactor_segmented_tasks" \
  cargo test -p tx-reactor --test smp_model -- --nocapture
```

Expected: both builds have identical semantic receipts and zero invariant
failures.

- [ ] **Step 6: Capture five interleaved S4-versus-S5 pairs**

Use the accepted S4 scheduler mode and identical image/workload configuration.
Promote segmented storage only when targeted shared-lock total wait falls at
least 30%, median throughput is at least 98% of S4, capture integrity is
complete, and every correctness marker is identical.

- [ ] **Step 7: Commit or retain the old storage based on evidence**

On pass, keep the compile-time segmented build available and record whether it
becomes the default. On fail, revert only the candidate code through its own
commit history, retain measurement support, and record the failed gate; do not
claim S5 completion.

```bash
cargo xtask progress validate
git diff --check
git add Cargo.toml crates/tx-reactor/src/task.rs \
  crates/tx-reactor/src/task/directory.rs crates/tx-reactor/src/task/control.rs \
  crates/tx-reactor/src/waker.rs crates/tx-reactor/src/wake_ingress.rs \
  crates/tx-reactor/src/runtime.rs crates/tx-reactor/tests/task_lifecycle.rs \
  crates/tx-reactor/tests/reactor_smoke.rs crates/tx-reactor/tests/smp_model.rs \
  docs/progress/research/2026-08-04-smp-pelt-baseline.md \
  docs/progress/plans/2026-08-04-smp-pelt-scheduler.json
git commit -m "perf: shard measured reactor task hot paths"
```

## Task 17: Run Final Acceptance And Catch Up Active Documentation

**Files:**

- Modify: `docs/Txv3/10_SCHED_SMP_v1.md`
- Modify: `docs/design/02_execution/REACTOR_v0.md`
- Modify: `docs/design/02_execution/SCHEDULER_v0.md`
- Modify: `docs/design/02_execution/reactor_scheduling.md`
- Modify: `docs/design/02_execution/THREAD_RUNTIME_v1.md`
- Modify: `docs/progress/research/2026-08-04-smp-pelt-baseline.md`
- Modify: `docs/progress/plans/2026-08-04-smp-pelt-scheduler.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Run the full host gate**

```bash
cargo -q xtask unit
cargo xtask check
cargo test -p tx-reactor --test task_lifecycle -- --nocapture
cargo test -p tx-reactor --test scheduler -- --nocapture
cargo test -p tx-reactor --test reactor_smoke -- --nocapture
cargo test -p tx-reactor --test userspace_run -- --nocapture
cargo test -p tx-reactor --test smp_model -- --nocapture
cargo test -p tx-reactor --test pelt -- --nocapture
```

Expected: every command passes. If unrelated checkout drift blocks broad
`xtask check`, record the exact unrelated error and still require all focused
gates to pass before handoff.

- [ ] **Step 2: Rerun legacy, static, and shipped movable modes**

```bash
cargo xtask test smoke --target rv64-qemu --timeout-ms 60000
cargo xtask test busybox-boot --target rv64-qemu --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case static --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case movable --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case pipe --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case affinity --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case timer --timeout-ms 60000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case pthread --timeout-ms 120000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case mixed --timeout-ms 120000
cargo xtask test smp-scheduler-witness --target rv64-qemu --case stress --timeout-ms 120000
```

Run the accepted load/storage mode in the movable cases. Every serial log must
pass `fault-decode --all --brief`.

- [ ] **Step 3: Update active docs to shipped behavior only**

Describe the actual default and supported boot modes, queue endpoints,
lock/recheck wake, IPI coalescing, userspace migration-safe boundary, affinity
transactions, and PELT/storage promotion result. Do not describe failed or
unpromoted candidates as production behavior.

- [ ] **Step 4: Close progress with exact evidence**

Set each JSON step to `complete`, `blocked`, or `canceled` according to its
receipt. `STATUS.md` must name changed surfaces, verification commands,
selected defaults, next performance target, and remaining blockers. The
research record must link every retained serial/runtime/report artifact.

- [ ] **Step 5: Run final documentation and progress validation**

```bash
cargo xtask observe-schema check
cargo xtask lint docs
cargo xtask lint invariants
cargo xtask progress validate
git diff --check
rg -n "periodic.*4ms|steal.*New|steal.*Boosted" \
  docs/Txv3/10_SCHED_SMP_v1.md docs/design/02_execution \
  docs/progress/research/2026-08-04-smp-pelt-baseline.md
```

Expected: all lints/validation pass; diff check and targeted stale/placeholder
scan print no active conflict.

- [ ] **Step 6: Commit final acceptance**

```bash
git add docs/Txv3/10_SCHED_SMP_v1.md \
  docs/design/02_execution/REACTOR_v0.md \
  docs/design/02_execution/SCHEDULER_v0.md \
  docs/design/02_execution/reactor_scheduling.md \
  docs/design/02_execution/THREAD_RUNTIME_v1.md \
  docs/progress/research/2026-08-04-smp-pelt-baseline.md \
  docs/progress/plans/2026-08-04-smp-pelt-scheduler.json \
  docs/progress/STATUS.md
git commit -m "docs: record SMP PELT scheduler acceptance"
```

## Dependency And Promotion Summary

```text
Task 1 docs
  -> Task 2 authority
  -> Task 3 queue linearization
  -> Task 4 wake ingress
  -> Task 5 IPI/WFI
  -> Task 6 S1
  -> Task 7 S2 static
  -> Task 8 userspace unwind
  -> Task 9 direct steal
  -> Task 10 affinity
  -> Task 11 S3 movable
  -> Task 12 PELT accounting
  -> Task 13 shadow
  -> Task 14 S4 decision
  -> Task 15 S5 trigger
       -> Task 16 only when triggered
  -> Task 17 final acceptance
```

The implementation is complete when S1-S3 pass, the shipped default is a
documented deliberate choice, S4 active PELT is either promoted with matched
evidence or remains shadow-only with a recorded failed gate, and S5 is either
promoted with measured benefit or recorded as not needed. No VM/pmap,
adaptive-slice, periodic push-balancer, NUMA, RT, cgroup, or PMU work is part of
this plan.
