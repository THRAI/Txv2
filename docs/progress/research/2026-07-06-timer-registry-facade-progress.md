# Timer Registry Facade Progress

Date: 2026-07-06

## Summary

Started the Package C/D implementation slice from
[`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md): the substrate
timer wheel now exposes producer-facing and reactor-facing facade traits, and
timer expiry routing is expressed through a router trait rather than only a
concrete `TimerWheel::fire_due` method.

Update: the slice now includes the first reactor-owned scheduler-aware timer
wake router. Timer expiry can wake a parked task through scheduler placement
without relying on a mailbox-captured `Waker`.

## What Changed

- Added `TimerRegistrar` for producer-side `install_for_task`.
- Added `TimerRegistry` for reactor-side `fire_due_with` /
  `next_deadline_ns`.
- Added `TimerWakeRouter` plus `DirectMailboxTimerWakeRouter`.
  `TimerWheel::fire_due` is now a compatibility wrapper over
  `TimerRegistry::fire_due_with` using the direct mailbox router.
- Re-exported the new facade types through `tx-substrate::wake`,
  `tx-reactor::adapter::bus_wire`, `tx-reactor::timer`, and the `tx-reactor`
  crate root.
- Updated `tx-reactor::Reactor::advance_time_to` and
  `tx-scripts::drive` timer installs to call through the new facade traits.
- Added a red/green test in `tx-reactor/tests/v3_timer_surface.rs` proving a
  task-bound timer can be installed through `TimerRegistrar`, fired through
  `TimerRegistry::fire_due_with`, and delivered to a `TimerWakeRouter` with the
  expected token and role.
- Added scheduler owner binding to `TaskMailbox` as substrate-neutral raw
  owner fields instead of reusing trace-only `task_id_low`.
- Added `TaskTable::make_parked_owner_runnable_with_hint` so a reactor wake
  router can move a parked task to runnable without going through
  `TaskWakeState`.
- Added a reactor-owned timer wake router used by `HartRuntimeView` and
  `Reactor::advance_time_to`; it posts `TimerFired`, resolves mailbox owner,
  calls scheduler placement, enqueues into the selected local queue, and uses
  `RescheduleSignal` for remote dispatch.
- Added a red/green smoke test showing a task that never registers a mailbox
  waker is still resumed by timer expiry through the scheduler-aware route.
- Removed the direct mailbox compatibility route: deleted
  `DirectMailboxTimerWakeRouter`, removed `TimerWheel::fire_due`, and dropped
  the corresponding reexports. Tests that need to fire a wheel now provide an
  explicit `TimerWakeRouter`.

## Verification

- The new test first failed because `TimerRegistrar`, `TimerRegistry`, and
  `TimerWakeRouter` did not exist.
- `cargo test -p tx-reactor --test v3_timer_surface -- --nocapture` passed
  14/14.
- `cargo fmt --check` passed.
- `cargo check -p tx-substrate -q` passed.
- `cargo check -p tx-reactor -q` passed.
- `cargo check -p tx-scripts -q` passed with the pre-existing
  `step_connect.rs` unused-variable warning from `tx-subsystems`.
- The scheduler-aware timer wake test first failed because the direct mailbox
  route posted `TimerFired` but did not make the parked task runnable; after the
  reactor router landed, the focused test passed.
- `cargo test -p tx-reactor --test reactor_smoke -- --test-threads=1
  --nocapture` passed 40/40.
- `cargo check -p tx-substrate -q`, `cargo check -p tx-reactor -q`, and
  `cargo check -p tx-scripts -q` passed after the owner/router slice.
- `cargo fmt --check` passed after the owner/router slice.
- `rg -n "DirectMailboxTimerWakeRouter|\\.fire_due\\(" crates --glob '*.rs'`
  has no matches after the direct-router retirement.
- `cargo check -p tx-substrate -q` and `cargo check -p tx-reactor -q` passed
  after the direct-router retirement.
- `cargo test -p tx-reactor --test v3_timer_surface -- --nocapture` passed
  14/14 after the direct-router retirement.
- `cargo test -p tx-reactor --test reactor_smoke -- --test-threads=1
  --nocapture` passed 40/40 after the direct-router retirement.

Attempted `cargo test -p tx-scripts
drive_yield_on_wait_source_with_deadline_returns_etimedout -- --nocapture`,
but the current dirty tree failed before the test with an unrelated
`tx-scripts::process::exec::script` compile error: missing
`drive_exec_post_commit_ops`.

`reactor_smoke` default-parallel execution is still not a valid gate for this
file: existing tests share per-hart `current_task_mailbox` slots. A
default-parallel run failed in an existing current-slot test, while the same
suite passed with `--test-threads=1`.

## Remaining Work

This does not complete the full timer retirement goal. Runtime timer expiry now
has a scheduler-aware route and the direct mailbox compatibility router is gone,
but the legacy `TimerQueue` / `DeadlineFuture` / `timer_sleep` path remains
active in `tx-reactor`, `tx-subsystems`, `tx-kernel`, and several `tx-shims`
syscall helpers.

The next necessary design-to-code step is to migrate active waits and syscall
timeout users to `TimerRegistrar` guards, then remove the legacy timer queue
interfaces and direct mailbox compatibility surface from active exports.
