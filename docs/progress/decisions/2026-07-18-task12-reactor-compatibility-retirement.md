# Task 12 reactor compatibility retirement

**Date:** 2026-07-18
**Status:** Accepted and implemented.

## Context

Tasks 10 and 11 left narrow compatibility paths so grouped reactor modules and
thread-owned userspace rendezvous could land without combining their behavioral
changes with public-surface retirement. Task 12 removes those temporary paths
after proving all live consumers use the owning APIs.

## Decision

- Delete `crates/tx-reactor/src/mailbox.rs` and the inline public
  `tx_reactor::wait_source` module. Keep the ten existing root mailbox/wait
  names, without adding root names, through direct owner exports.
- Delete `crates/tx-reactor/src/userspace/compat.rs`,
  `ReactorShared.userspace`, the six global Reactor userspace methods, all
  `LegacyUserspaceRun*` adapters, and the retired `TimerPreempt`, `Preempted`,
  `AlreadyResolved`, and `NotRunning` userspace vocabulary. Per-thread
  `UserspaceRunSlot` ownership and strict `UserspaceRunResult` /
  `InvalidPhase` semantics remain canonical.
- Delete `Reactor::submit -> TaskId`. Reactor callers use `submit_task`, retain
  `TaskKey`, and query through `task_status`; unrelated `submit` methods are
  unchanged.
- Retire generation-blind public query signatures. `Reactor::task_status` and
  `last_stop_reason`, plus scheduler budget, runtime, placement, wake-target,
  migration, userspace-classification, affinity, nice, and RT-priority queries,
  accept `TaskKey` and validate the co-stored generation under the owning task
  or scheduler-metadata lock. This is an intentional Task 12 source-API break;
  no source compatibility is claimed for callers that passed `TaskId`.
- Keep structural tests that reject reintroduced forwarding files/modules,
  global userspace methods/state, retired userspace vocabulary, widened root
  mailbox/wait exports, and legacy Reactor submission.

## Consumer Classification

At starting commit `c1242a060b191630fb808c31a5ee4fb3087fdc14`, `crates` and
`boards` contained 137 `.submit(` hits. Exactly 48 had a `Reactor` receiver:
45 Reactor integration-test calls and three tx-subsystems test calls. All 48
now call `submit_task`.

The remaining 89 hits are all unrelated to `Reactor`:

- 39 `TaskTable::submit` calls (25 core tests, 11 lifecycle integration tests,
  two runtime tests, and the one task-lifecycle production transition).
- 25 `BlockQueue::submit` calls.
- 10 `PageRequestQueue::submit` calls.
- 11 `PageService::submit` calls.
- Four `BlockDispatchExecutor::submit` calls.

The Task 12 `.id()` diff audit found five added conversions: three only assert
that the first submitted task occupies `TaskId(0)`, while two truncated a
`TaskKey` for `last_stop_reason`. The two unsafe additions and six pre-existing
stop-reason truncations now pass `TaskKey` directly; the retired legacy
`submit` implementation's own `submit_task(future).id()` conversion is deleted.
No generation-sensitive Reactor query receives a truncated key.

## Public Task Identity Boundary

The generation-sensitive public query inventory is:

- Reactor lifecycle: `task_status`, `last_stop_reason`, and `task_affinity`.
- Scheduler accounting: `remaining_budget_ns` and `total_runtime_ns`.
- Scheduler metadata: `task_affinity`, `task_nice`, and `task_rt_priority`.
- Scheduler placement/classification: `is_queued`, `task_placement_hint`,
  `target_hart_for_wake`, `can_migrate`, and `is_userspace_thread`.

All accept `TaskKey`. Scheduler reads take the metadata lock once and compare
the stored `TaskSchedMeta.key` before copying data; they do not classify a task
through a lock-free slot reread. Lifecycle reads resolve the exact generation
through `TaskTable` / `TaskControl` while holding the task-table lock.

The structural ratchet recursively scans every authored
`crates/tx-reactor/src/**/*.rs` after building one source-tree-wide,
module-qualified symbol table. A fixed-point resolver follows direct and
transitive `type` aliases, grouped and renamed imports, public reexports, and
physical or inline module-qualified paths across files. It rejects known
generation-sensitive public query signatures unless their identity resolves to
`TaskKey`, rejects any path resolving to `TaskId`, and rejects calls that pass
`task.id()`. Public `type` aliases and `use` / `pub use` reexports resolving to
`TaskId` are themselves public surfaces, so they must match the same exact
slot/value allowlist rather than silently creating another public identity
name. Symbols remain module-scoped, preventing a same-named `TaskKey` alias in
one module from colliding with a `TaskId` alias in another. The exact allowlist
permits `TaskId` only as a slot/index or diagnostic value:

- the `TaskId` definition and its exact root/core exports;
- `TaskKey::id`, which explicitly projects a slot value for physical storage,
  publication, and reporting boundaries;
- `TaskHandle::{new,id}`, whose value is a scheduler slot handle while the
  authoritative metadata row separately stores the full `TaskKey`;
- `DequeuedTask.task` and `QueuedTaskReport.task`, which are diagnostic/report
  values and do not authorize mutation or identity-sensitive lookup.

The compatibility scan has no `tx_reactor::mailbox::*`,
`tx_reactor::wait_source::*`, `compat_locals`, compatibility definition, or
global Reactor facade hit. Its remaining 21 textual hits are ten negative
structural-test fixtures and eleven live per-thread ThreadRuntime method calls
or definitions.

## Verification

- TDD RED: `cargo test -p tx-reactor --test module_layout
  task12_compatibility_paths_and_legacy_submit_are_retired -- --exact
  --nocapture` failed because `crates/tx-reactor/src/mailbox.rs` still existed.
- Review TDD RED: the relocated-submit fixture placed an inherent
  `impl crate::Reactor { pub fn submit(..) }` in another source module and the
  lifecycle-only scanner accepted it. The repository-wide GREEN scanner walks
  all reactor sources while excluding comments, literals, trait methods, and
  unrelated inherent `submit` methods.
- Code-quality REDs: after a completed task was drained and its slot reused, a
  stale `TaskKey.id()` read the replacement task's `Blocked` stop reason; an
  inline `mod relocated { impl crate::Reactor { pub fn submit(..) } }` also
  escaped the depth-zero source scanner. `Reactor::last_stop_reason` now accepts
  `TaskKey` and resolves the exact generation, and the structural scanner
  recursively visits inline module item bodies while retaining its comment,
  literal, trait-impl, and unrelated-impl exclusions.
- Final-review REDs: the stale slot-reuse witness initially observed
  `Some(Runnable)` through `task_status(TaskId)` instead of rejecting the old
  generation. The structural RED simultaneously listed all eight migrated
  scheduler `TaskId` query signatures and every query call that passed
  `task.id()`. The consolidated GREEN witness reuses one slot and proves stale
  rejection plus current-generation data for status, stop reason, budget,
  runtime, affinity, nice, RT priority, queue/placement, wake target, migration,
  and userspace classification. Scanner fixtures cover nested modules, type and
  import aliases, UFCS/direct calls, comments, strings, and unrelated methods.
- Alias-ratchet RED: a two-file fixture defined `QueryIdentity = TaskId` in one
  module and imported it into lifecycle, accounting, placement, and userspace
  query signatures in another; the per-file alias scanner returned no
  violations. GREEN resolves direct, transitive, module-qualified, private,
  renamed-import, and public-reexport aliases across files, rejects public alias
  surfaces and unknown public TaskId-taking methods, and accepts TaskKey aliases
  plus same-named aliases in distinct modules.
- GREEN at the rewritten Task 12 point: `cargo test -p tx-reactor --lib`
  passes 219/219; `cargo test -p tx-reactor` passes 203/203 integration tests,
  including 18/18 module-layout tests. The final Task 13 tip adds its own tests
  and is recorded separately in `STATUS.md` and the completed reactor plan.
- Consumers: kernel `thread_future` passed 27/27, kernel `trap_handoff` passed
  7/7, and `cargo test -p tx-subsystems --lib thread_runtime -- --nocapture`
  passed 26/26. Reactor and kernel no-default library checks pass.
- `cargo check -p tx-kernel-riscv64-qemu-virt --target
  riscv64gc-unknown-none-elf` reached `tx-kernel` and failed only because
  `tools/images/vendor/tx-netfast-riscv64` is absent.
- `cargo xtask lint arch` reports the unchanged 83 inherited findings.
  `cargo xtask lint unused` reports the unchanged two tx-kernel findings.
- `cargo xtask boundary-report --json` reports substrate 682 and reactor 51.
  The substrate delta from 679 is exactly the two required direct root owner
  exports plus the migrated timer integration import; the structural witness
  pins those paths and no ratchet ceiling changed.
- Scoped rustfmt and `git diff --check` pass. Full fmt retains only the
  inherited `xtask/src/test.rs:184` wrapping diff.
- `cargo xtask lint docs` retains 27 inherited broken links and six
  stale-vocabulary warnings. `cargo xtask progress validate` remains blocked
  only by the inherited missing `docs/design/05_filesystem/IO_MANAGER_v1.md`
  reference; Task 12 changes no progress JSON.

## Next Step And Blockers

Task 13 adds transition observability and the final correctness gates. The only
Task 12 target-check blocker is the inherited missing
`tools/images/vendor/tx-netfast-riscv64` image. Repository-wide lint, docs, and
progress-validation baselines remain inherited and are not Task 12 fixes.
