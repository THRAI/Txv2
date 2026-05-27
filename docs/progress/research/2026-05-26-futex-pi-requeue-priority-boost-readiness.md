# Futex PI Requeue and Scheduler Priority Boost Readiness

Date: 2026-05-26

Branch/worktree: `codex/futex-linux-compat` at
`/Users/3y/.codex/worktrees/futex-linux-compat/Tx`

## Question

Plan the remaining Linux-compatible PI futex requeue operations and real
scheduler priority boosting, and check whether the current substrate/reactor
surfaces are enough to support them.

## Linux Behavior Anchors

- `FUTEX_WAIT_REQUEUE_PI` waits on a non-PI source futex and can be requeued by
  another task's `FUTEX_CMP_REQUEUE_PI` onto a PI target futex. A plain
  `FUTEX_WAKE` on the source removes the waiter without requeue and the waiter
  reports `EAGAIN`; a successful requeue/acquire returns `0`.
  Source: https://man7.org/linux/man-pages/man2/FUTEX_WAIT_REQUEUE_PI.2const.html
- `FUTEX_CMP_REQUEUE_PI` is the PI-aware compare-requeue operation. It moves
  waiters blocked via `FUTEX_WAIT_REQUEUE_PI` from `uaddr` to the PI target
  `uaddr2`, returns the total number woken or requeued, returns `EAGAIN` on
  compare mismatch, rejects `uaddr == uaddr2`, rejects mixed waiter kinds, and
  requires the wake count argument to be exactly `1`.
  Source: https://man7.org/linux/man-pages/man2/FUTEX_CMP_REQUEUE_PI.2const.html
- Linux `do_futex()` routes `FUTEX_WAIT_REQUEUE_PI` to
  `futex_wait_requeue_pi(..., FUTEX_BITSET_MATCH_ANY, uaddr2)` and routes
  `FUTEX_CMP_REQUEUE_PI` to `futex_requeue(..., requeue_pi=1)`.
  Source: https://codebrowser.dev/linux/linux/kernel/futex/syscalls.c.html
- In `futex_requeue(..., requeue_pi=1)`, Linux rejects negative wake/requeue
  counts, rejects same source/target key, requires `nr_wake == 1`, compares the
  source futex when `cmpval` is present, tries to acquire the PI target on
  behalf of the top waiter, sets/uses PI state, and only pairs
  `FUTEX_WAIT_REQUEUE_PI` waiters with `FUTEX_CMP_REQUEUE_PI`.
  Source: https://codebrowser.dev/linux/linux/kernel/futex/requeue.c.html

## Current txKernel Support

Ready for PI requeue word/waiter semantics: yes for the implemented 32-bit
Linux-compatible futex surface.

- `crates/tx-subsystems/src/futex/mod.rs` now has exact waiter rows for non-PI
  waiters and per-lock PI waiter trees keyed by priority/FIFO order.
- `FutexPiWaiter` carries stable waiter identity, waiter TID/task,
  blocked-owner metadata, mailbox/source fields, insertion sequence, and
  priority order. This supports cached top-waiter handoff and owner-chain
  deboost.
- `FUTEX_LOCK_PI`, `FUTEX_TRYLOCK_PI`, `FUTEX_LOCK_PI2`, `FUTEX_UNLOCK_PI`,
  `FUTEX_WAIT_REQUEUE_PI`, and `FUTEX_CMP_REQUEUE_PI` are wired at the syscall
  layer.
- The v3 mailbox/wait-source path can park and wake a waiter cleanly after it is
  moved between source and target tables, provided the futex subsystem owns the
  requeue state transition under its futex table locks.

Ready for RT-priority inheritance over Tx's current scheduler model: yes.

- `crates/tx-reactor/src/scheduler.rs` stores task-level `pi_waiters`, exposes
  upsert/remove/effective-priority APIs, and selects boosted fair work ahead of
  unboosted fair work.
- The thread-runtime seam exposes live TID-to-task lookup to futex through
  `tx_subsystems::reactor_priority`.
- Remaining scheduler caveat: `PriorityKey` covers RT priority and FIFO order;
  full `SCHED_DEADLINE` runtime semantics are left to a later scheduler slice.

## Track A: Implement PI Requeue Semantics

This track should land before real scheduler boosting. It gives Linux-compatible
syscall behavior for condition-variable style PI requeue without claiming
rt_mutex priority inheritance.

1. Extend futex waiter records.
   - Add a waiter kind for `WaitRequeuePi` rows in the source futex table.
   - Store `target_pi_key`, `waiter_tid`, mailbox/source data, insertion
     sequence, and a requeue state: `WaitingOnSource`, `RequeuedToPi`,
     `AcquiredPi`, `SourceWake`.
   - Keep classic `FutexWaiter` rows distinct from `WaitRequeuePi`; mixed
     waiter kinds on the same source must return `EINVAL` for PI requeue paths.

2. Add subsystem APIs.
   - `step_futex_wait_requeue_pi_in(aspace, uaddr, expected, timeout,
     uaddr2, waiter_tid, guard)`:
     - validate aligned/non-null `uaddr` and `uaddr2`;
     - reject equal futex keys with `EINVAL`;
     - compare source word atomically before enqueue;
     - on mismatch return `EAGAIN`;
     - enqueue a `WaitRequeuePi` source row bound to the target PI key;
     - block through the existing mailbox wait;
     - after wake, return `0` only when state is `AcquiredPi` or the target PI
       handoff has assigned the futex word to this waiter;
     - return `EAGAIN` if woken by ordinary source `FUTEX_WAKE`;
     - return `ETIMEDOUT` on timeout and remove both source/target rows.
   - `step_futex_cmp_requeue_pi_in(aspace, uaddr, uaddr2, nr_wake,
     nr_requeue, cmpval, requeuer_tid, guard)`:
     - require `nr_wake == 1`, else `EINVAL`;
     - reject negative/impossible counts at the syscall boundary;
     - reject equal source/target keys;
     - compare source word and return `EAGAIN` on mismatch;
     - reject classic non-PI waiters and PI-lock waiters on the source with
       `EINVAL`;
     - select at most one top waiter for immediate target acquisition;
     - move up to `nr_requeue` remaining `WaitRequeuePi` rows into
       `PI_WAITERS` for `uaddr2`;
     - return `woken + moved`.

3. Target PI acquisition behavior.
   - If target owner word is unlocked, acquire it on behalf of the top
     requeued waiter, set the owner TID in the user word, preserve/set
     `FUTEX_WAITERS` when more PI waiters remain, mark that waiter
     `AcquiredPi`, and wake it.
   - If target owner word is locked by another TID, set `FUTEX_WAITERS`, move
     the selected waiter(s) into `PI_WAITERS`, and let `FUTEX_UNLOCK_PI` perform
     the existing owner-word transfer.
   - If target owner is the calling waiter, return `EDEADLK` for the acquisition
     attempt where Linux would detect self-deadlock.
   - If target owner TID cannot be resolved once real owner lookup exists,
     return `ESRCH`; until then, keep this path documented as a semantic gap
     rather than fabricating a lookup.

4. Syscall wiring.
   - Parse `FUTEX_WAIT_REQUEUE_PI` timeout like Linux timed wait-style ops,
     including `FUTEX_CLOCK_REALTIME` validity.
   - Route `FUTEX_WAIT_REQUEUE_PI` through the wait/retry loop used by the
     existing futex waits, but translate ordinary source wake to `-EAGAIN`.
   - Route `FUTEX_CMP_REQUEUE_PI` through the new subsystem API; in the Linux
     ABI, `val` is the required `nr_wake`, `timeout`/arg4 is `nr_requeue`, and
     `val3` is the compare value.

5. Tests.
   - Add tx-shims dispatch tests:
     - `WAIT_REQUEUE_PI` mismatch returns `-EAGAIN`.
     - `CMP_REQUEUE_PI` with `val != 1` returns `-EINVAL`.
     - same source/target returns `-EINVAL`.
     - compare mismatch returns `-EAGAIN`.
     - source ordinary `FUTEX_WAKE` wakes a wait-requeue-pi waiter with
       `-EAGAIN`.
     - uncontended target lets `CMP_REQUEUE_PI` acquire target for top waiter
       and `WAIT_REQUEUE_PI` returns `0`.
     - contended target moves waiter to PI table and `UNLOCK_PI` hands off.
   - Add tx-subsystems tests for source-table cleanup on timeout/cancel and
     target-table cleanup after handoff.

6. Verification after Track A.
   - `cargo test -p tx-shims futex_dispatch -- --test-threads=1`
   - `cargo test -p tx-subsystems futex -- --test-threads=1`
   - `cargo test -p tx-subsystems --test v3_futex_waitsource -- --test-threads=1`
   - `cargo fmt --check`
   - `cargo -q xtask unit`
   - `cargo xtask progress validate`
   - `git diff --check`
   - A bounded RV64 smoke run once the host tests are green.

Acceptance for Track A: PI requeue syscalls no longer return `-ENOSYS`; they
match Linux argument validation, compare/requeue counts, source wake `EAGAIN`,
timeout cleanup, and target PI handoff at the futex-word/waiter level. The
status note must still say scheduler PI boosting is not implemented.

Implementation update, 2026-05-26: Track A is implemented in
`crates/tx-subsystems/src/futex/mod.rs` and
`crates/tx-shims/src/linux_syscall/vm.rs`. Covered cases include
`WAIT_REQUEUE_PI` mismatch, source `FUTEX_WAKE` returning `EAGAIN`,
`CMP_REQUEUE_PI` `nr_wake != 1`, same-key rejection, compare mismatch,
uncontended target acquisition for the top waiter, and contended target handoff
through `FUTEX_UNLOCK_PI`. Follow-up coverage also pins multi-waiter
`1 + nr_requeue` movement and timeout/cancel cleanup from the syscall-level
`FutexWaitRequeuePiOp`. Remaining evidence before promoting the work beyond
host/kernel smoke: any guest pthread PI-condvar coverage available in the test
corpus. Track B scheduler priority inheritance remains separate and unstarted.

## Track B: Add Real Scheduler Priority Inheritance

This track needs substrate/reactor API work before futex can honestly implement
Linux rt_mutex behavior.

Implementation update, 2026-05-26: the first scheduler substrate slice is
implemented in `crates/tx-reactor/src/scheduler.rs`. The reactor now exposes
priority donation tokens, donation/revocation errors, effective RT priority
queries, active donation rows on `TaskSchedMeta`, recomputation to the highest
active donor, and queue selection that picks boosted fair tasks before ordinary
fair FIFO work. This only closes the scheduler-local API gap; futex PI still
needs TID/thread/task lookup and owner-chain donation wiring before the PI
paths can use it.

Implementation update, 2026-05-26: the TID/thread/task lookup and reactor
facade seam are now in place. `crates/tx-subsystems/src/thread_runtime` can
resolve a TID to a live `ThreadPayload` and bound `TaskKey`; `tx-kernel` binds
initial and cloned thread submissions through that API instead of a
kernel-private TID map; `tx-reactor::Reactor` exposes donation/drop/effective
priority wrappers; and `tx-subsystems::reactor_priority` gives futex a
dependency-clean path into the boot reactor once PI waiter rows carry task
keys. This still does not make futex PI donate yet; it removes the substrate
blocker for the next futex-owned slice.

Implementation update, 2026-05-26: direct PI futex blocking now uses that
bridge. `FutexPiWaiter` rows carry optional priority donation tokens; contended
`FUTEX_LOCK_PI` donates a live RT waiter's effective priority to the live owner
task; timeout/cancel/reset/unlock revoke the exact token; and unlock handoff
retargets remaining waiter donations to the new owner. This is still a direct
edge implementation, not full Linux rt_mutex owner-chain propagation.

Implementation update, 2026-05-26: PI requeue waiters now participate in the
same direct donation lifecycle. When `FUTEX_CMP_REQUEUE_PI` moves
`FUTEX_WAIT_REQUEUE_PI` waiters onto a contended target PI futex, each live RT
waiter donates through the current target owner task. When `FUTEX_UNLOCK_PI`
hands the PI futex to the first waiter, the handed-off waiter's donation is
dropped and remaining waiter donations are retargeted to the new owner. Focused
dispatch tests prove both the contended-owner boost on requeue and the
old-owner-drop/new-owner-preserve behavior after unlock. The remaining semantic
gap is still owner-chain propagation: a boosted owner that is itself blocked on
another PI futex does not yet forward the donation.

Implementation update, 2026-05-26: the first PI owner-lookup hardening slice is
implemented. `FUTEX_LOCK_PI` and `FUTEX_TRYLOCK_PI` now return `ESRCH` when the
owner TID encoded in the futex word does not resolve to a live
`ThreadPayload`. `FUTEX_CMP_REQUEUE_PI` also validates a contended PI target
owner before moving source waiters; on `ESRCH`, the waiter remains on the
source futex and a later ordinary source `FUTEX_WAKE` still resumes it with the
Linux `EAGAIN` result. This closes the unknown-owner error behavior for the
implemented PI paths, but not owner-chain cycle detection.

Implementation update, 2026-05-26: bounded owner-chain donation refresh is now
implemented for the current PI waiter table. `FutexPiWaiter` records the owner
TID it is blocked behind; when a waiter donates to an owner, futex refreshes any
existing PI waiter row for that owner and continues up to a bounded depth. This
now propagates a high-priority waiter through a blocked owner for both nested
`FUTEX_LOCK_PI` and `FUTEX_CMP_REQUEUE_PI` target-owner cases. It is still not
full Linux `rt_mutex`: waiters remain FIFO, cycles are bounded but not yet
reported as `EDEADLK`, and priority ordering/preemption semantics are only as
strong as the current scheduler donation API.

Implementation update, 2026-05-26: PI unlock handoff now chooses the highest
effective RT-priority waiter and uses FIFO insertion sequence only as a
tie-breaker. This closes the most visible FIFO handoff gap for direct PI locks
and for PI requeue waiters after they have moved onto the target PI futex. The
implementation still evaluates priority at handoff time from the current waiter
vector; it does not yet maintain a Linux-style priority tree/rt_mutex wait
queue.

1. Make task identity available to the futex subsystem.
   - Move TID-to-`TaskKey` lookup out of kernel-init-private
     `THREAD_REACTOR_TASKS` into a thread-runtime-owned registry or exported
     capability-shaped API.
   - Ensure `ThreadPayload.task` is set when userspace thread tasks are
     submitted, for both exec initial thread and clone children.
   - Provide safe lookup helpers:
     - current thread TID -> `TaskKey`;
     - owner TID -> live `ThreadPayload`/`TaskKey`;
     - `TaskKey` -> current scheduler priority snapshot.

2. Add scheduler boost APIs.
   - Extend `TaskSchedMeta` with `base_class`, `base_rt_priority`,
     `effective_class`, `effective_rt_priority`, and active boost records.
   - Add a public reactor method shaped like
     `donate_priority(owner: TaskKey, donor: TaskKey, reason: BoostReason) ->
     BoostToken` and `drop_priority_donation(token)`.
   - Keep donation keyed by reason/futex edge so timeout, cancel, unlock, and
     owner death can revoke exactly the donations they created.

3. Make priority affect scheduling.
   - Update enqueue/pick ordering so effective RT priority outranks Fair/Idle
     and higher RT priority wins among RT tasks.
   - Give `WakeHint::PriorityBoost` concrete behavior: if a boosted owner is
     already queued, move it toward the front or into the effective-priority
     queue; if it is polling on another hart, request reschedule/preemption.
   - Add unit tests in `crates/tx-reactor/tests/scheduler.rs` proving boosted
     owner ordering over fair tasks and ordering among RT priorities.

4. Add futex PI owner-chain donation.
   - Extend `FutexPiWaiter` with waiter `TaskKey`, base/effective priority
     snapshot, target owner TID/task, and blocked-on futex key.
   - When a waiter blocks on a PI futex, donate to the current owner.
   - If the owner is itself blocked on another PI futex, propagate the donation
     along the owner chain with a bounded cycle/deadlock check.
   - On `UNLOCK_PI`, timeout, cancel, source wake, robust owner death, or task
     exit, revoke the affected donation tokens and recompute effective priority.

5. Linux-compatible PI error behavior.
   - Return `ESRCH` when an owner TID in a PI futex word cannot be resolved.
   - Return `EDEADLK` for detected self-deadlock and owner-chain cycles.
   - Preserve `FUTEX_OWNER_DIED` and `FUTEX_WAITERS` bits during handoff.
   - Keep `EPERM` for unlock by a non-owner.

6. Verification after Track B.
   - `cargo test -p tx-reactor scheduler -- --test-threads=1`
   - `cargo test -p tx-shims futex_dispatch -- --test-threads=1`
   - `cargo test -p tx-subsystems futex -- --test-threads=1`
   - `cargo -q xtask unit`
   - `cargo xtask test smoke --target rv64-qemu --timeout-ms 30000`
   - Focused guest pthread/robust/condvar PI evidence once userspace tests are
     available or added.

Acceptance for Track B: PI waiters donate effective priority to futex owners,
donations affect scheduler selection/preemption, owner-chain donation is bounded
and cleaned up, and futex PI paths no longer carry the "FIFO only" caveat.

## Readiness Verdict

Ready: implemented for PI requeue syscall semantics and the Tx-native
Linux-shaped `rt_mutex` priority-tree/task-`pi_waiters` model. The remaining
caveats are stabilization and coverage, not missing core PI machinery.

Closed gaps in this worktree:

- exported TID/thread/task lookup;
- `ThreadPayload.task` submission wiring for initial and cloned userspace
  thread creation paths;
- public reactor donation/revocation/effective-priority API;
- effective-priority fields and fair-runqueue ordering over boosted RT tasks;
- direct futex PI donation creation, revocation, and handoff retargeting;
- PI requeue donation and retarget tests proving scheduler-visible boosts.
- Linux-compatible `ESRCH` for unknown PI owner TIDs in `LOCK_PI`,
  `TRYLOCK_PI`, and contended-target `CMP_REQUEUE_PI`.
- bounded owner-chain donation propagation for nested `LOCK_PI` and
  requeue-to-blocked-owner cases.
- priority-ordered PI unlock handoff by scheduler effective RT priority, with
  FIFO sequence as tie-breaker.
- per-lock priority waiter trees and task-level `pi_waiters` top-waiter
  accounting.
- explicit guest pthread PI-condvar evidence through the reusable branch-local
  helper.

Remaining non-blocking caveats before merge:

- the PI-condvar helper is not integrated into the upstream OSComp judge suite;
- full `SCHED_DEADLINE` ordering is represented only as future API space;
- broader LTP/futex selftest-style coverage has not been run on this branch.

Recommended continuation order:

1. Keep the PI-condvar helper as a named Makefile target with private image
   behavior.
2. Clean stale comments/progress notes that still describe completed PI/futex2
   paths as deferred.
3. Run focused host gates plus the pthread guest evidence gates.
4. Treat any small LTP/futex selftest run as coverage discovery unless it
   isolates a clear regression in the futex module.

## Cycle Detection and Nested Deboost Reference Pass

Planning update, 2026-05-26: the next implementation slice is recorded in
`docs/progress/plans/2026-05-26-futex-pi-cycle-cleanup.json`.

Linux `rt_mutex_adjust_prio_chain()` is the behavior model for explicit
owner-chain checks. Tx's next helper should walk the current `FutexPiWaiter`
`waiter_tid -> blocked_owner_tid` graph before adding a new PI waiter edge or
moving a `WAIT_REQUEUE_PI` waiter to the target PI table. Reaching the blocking
waiter again should return `EDEADLK`; finding a non-live owner should keep the
existing `ESRCH` behavior; exceeding the bounded depth should also fail
closed as `EDEADLK` rather than silently truncating propagation.

Linux PI requeue validation must happen before destructive source-row removal.
If a selected `WAIT_REQUEUE_PI` waiter would create an owner-chain cycle on the
target PI futex, `CMP_REQUEUE_PI` should return `EDEADLK`, preserve the source
waiter row, and leave that waiter wakeable by ordinary source `FUTEX_WAKE` with
the existing `EAGAIN` result.

Timeout/cancel cleanup needs a true deboost recomputation, not just token drop.
When a PI waiter row is removed, futex must remember the old
`blocked_owner_tid`, revoke that waiter's donation, recompute the strongest
remaining direct donation for that owner, and then propagate that changed
effective priority upward through any owner chain. This applies to direct
`FUTEX_LOCK_PI` waits and to `WAIT_REQUEUE_PI` waiters after they have moved to
a contended target PI futex.

Implementation update, 2026-05-26: the cycle/deboost slice is implemented.
`FUTEX_LOCK_PI` and contended-target `FUTEX_CMP_REQUEUE_PI` now run a bounded
PI waiter graph walk before adding the new blocked-owner edge; reaching the
requesting waiter returns `EDEADLK`, and `CMP_REQUEUE_PI` performs this check
before removing source rows. `step_futex_pi_cancel_wait_source` now records the
removed waiter's old `blocked_owner_tid`, drops the edge donation, and invokes
the existing chain-refresh path from that owner so direct and requeued
timeout/cancel cleanup down-propagates deboosts. This closes the observable
cycle and nested deboost gaps tracked by
`docs/progress/plans/2026-05-26-futex-pi-cycle-cleanup.json`; remaining
Linux-compat caveat is still the lack of Linux's full rt_mutex priority tree
and task `pi_waiters` structure.

## Full rt_mutex Priority-Tree Model Update

Implementation update, 2026-05-26: the Linux-style tree model is now
implemented in the futex worktree and tracked by
`docs/progress/plans/2026-05-26-futex-pi-rtmutex-tree.json`.

Tx now mirrors the core Linux `rt_mutex` shape with Rust ordered maps rather
than literal rbtrees:

- each PI futex key maps to a `FutexPiLockState` with `owner_tid`,
  `waiters_by_order`, `waiters_by_id`, stable waiter IDs, and cached
  `top_waiter`;
- each PI waiter records `waiter_tid`, optional waiter `TaskKey`,
  `blocked_on_key`, `blocked_owner_tid`, sequence, and priority order key;
- each scheduler task now stores `pi_waiters` keyed by an owned-futex
  `PiLockToken`, so only each owned lock's current top waiter contributes to
  the owner's effective RT priority;
- `FUTEX_UNLOCK_PI` pops the cached top waiter for handoff, removes the old
  owner's task `pi_waiters` entry, retargets remaining waiters to the new
  owner, and publishes only the handed-off waiter wake;
- timeout/cancel cleanup removes the exact waiter from both ordered indexes,
  refreshes the owner's task `pi_waiters` row, and propagates deboost through
  any blocked-owner chain;
- `FUTEX_CMP_REQUEUE_PI` inserts moved waiters into the target lock's ordered
  tree and only updates the target owner's task `pi_waiters` row when the
  target top waiter changes.

This closes the earlier "vector scan plus per-waiter donation token" caveat.
The remaining caution is now regression-harness shape, not data-structure
shape: a focused guest evidence pass on 2026-05-26 booted the current
rt_mutex-model RV64 kernel, passed the stock static/dynamic libctest pthread
condvar/robust slice, and ran an injected static RV64 musl
`PTHREAD_PRIO_INHERIT` condvar test to `PASS pthread-pi-condvar`, wrapper
status `0`, and guest `userspace:exited:0`. The serial logs are
`target/oscomp/os_serial_out_futex_pthread_linux_compat_rtmutex_20260526.txt`
and `target/oscomp/os_serial_out_pthread_pi_condvar_write_20260526.txt`;
`fault-decode` found no trap lines in the explicit PI-condvar log. The explicit
test is now rerunnable via
`tools/guest-tests/run-pthread-pi-condvar-rv64.sh` and the documented
`make oscomp-rv64-pthread-pi-condvar` entrypoint, but it is still branch-local
evidence because it injects a private sdcard copy via `debugfs` and selects it
through `basic-musl` rather than a normal OSComp judge suite.

Tx's `PriorityKey` currently represents RT priority/FIFO order while leaving
full deadline scheduling semantics for a later scheduler slice.

## Merge-Readiness Stabilization Evidence

Stabilization update, 2026-05-27: the rerunnable harness and stale-caveat
cleanup are tracked by
`docs/progress/plans/2026-05-27-futex-linux-compat-stabilization.json`.

Current evidence:

- `make -n oscomp-rv64-pthread-pi-condvar` confirms the named target rebuilds a
  private RV64 submit kernel, runs the helper against
  `target/oscomp/futex-pthread-data`, and writes
  `target/oscomp/os_serial_out_pthread_pi_condvar_stabilize_20260527.txt`.
- `make oscomp-rv64-pthread-pi-condvar` passed with `PASS pthread-pi-condvar`,
  wrapper status `0`, `txkernel:qemu-riscv64-virt:userspace:exited:0`, and no
  fault-decode trap lines.
- Focused host gates passed: `cargo test -p tx-shims futex_dispatch --
  --test-threads=1`, `cargo test -p tx-subsystems futex -- --test-threads=1`,
  `cargo test -p tx-subsystems --test v3_futex_waitsource --
  --test-threads=1`, `cargo test -p tx-reactor pi_waiter --
  --test-threads=1`, and `cargo test -p tx-reactor scheduler --
  --test-threads=1`.
- Focused RV64 libctest pthread condvar/robust and cancel slices passed with
  userspace exit `0`; current logs are
  `target/oscomp/os_serial_out_futex_pthread_stabilize_20260527.txt` and
  `target/oscomp/os_serial_out_futex_pthread_cancel_stabilize_20260527.txt`.

Remaining caveats:

- The PI-condvar helper is branch-local but now judge-scored: the 2026-05-27
  follow-up injects `pthread_pi_condvar_testcode.sh`, selects
  `tx.oscomp.groups=pthread-pi-condvar`, copies
  `judge_pthread-pi-condvar.py` into the private data dir, and asserts
  `tools/oscomp-judge.py` reports `[pthread-pi-condvar] 1/1`.
- Full `SCHED_DEADLINE` runtime semantics remain future scheduler work.
- Small LTP/futex selftest coverage is no longer blocked on image mechanics:
  `external/oscomp-autotest` is initialized, the RV64 OSComp testdata image is
  prepared, and `tools/build-slim-sdcard.py` can build a 17-case futex/robust
  LTP image. The current run reaches the cases but most return `TBROK` on
  missing `/proc/meminfo`; `set_robust_list01` exits `0`. This makes procfs
  coverage the next LTP blocker before the futex cases can isolate futex
  semantics.

## Caveat Closure Update

Follow-up update, 2026-05-27: caveat closure is tracked by
`docs/progress/plans/2026-05-27-futex-caveat-closure.json`.

Changes:

- Added `tools/guest-tests/judge_pthread-pi-condvar.py` and changed the
  PI-condvar helper to run as a dedicated `pthread-pi-condvar` group.
- Added kernel group routing for `pthread-pi-condvar` to
  `pthread_pi_condvar_testcode.sh`.
- Made the Makefile PI-condvar target build the RV64 kernel before submit so
  group-routing changes cannot accidentally boot stale kernels.
- Fixed `tools/build-slim-sdcard.py` for system Python 3.9, missing output
  parents, ext4 inode mode preservation, and dynamic musl loader/lib inclusion
  for slim LTP images.
- Added `make oscomp-rv64-ltp-futex` as the reusable LTP futex discovery
  target.

Current evidence:

- `target/oscomp/os_serial_out_pthread_pi_condvar_judged_20260527.txt`:
  `[pthread-pi-condvar] 1/1`, guest exit `0`, no fault-decode trap lines.
- `target/oscomp/os_serial_out_ltp_futex_20260527.txt`: selected LTP futex
  cases boot and execute; most block on `/proc/meminfo`, while
  `set_robust_list01` exits `0`; guest exit `0`, no fault-decode trap lines.
