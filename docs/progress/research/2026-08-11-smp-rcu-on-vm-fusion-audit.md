# SMP and RCU-on-VM Fusion Audit

Date: 2026-08-11

## Scope and worktree boundary

The source worktree is `/Users/3y/.config/superpowers/worktrees/Tx/rcu-on-vm`,
branch `codex/rcu-on-vm`. Its worktree record is marked closed, but the checkout
is currently dirty across HAL, VM, page-backed, ext4, shims, reactor, and xtask
paths. The committed resident-root series and the uncommitted filesystem/runtime
experiments must therefore be treated as separate inputs. Do not merge the
whole worktree or copy its current diff as an SMP integration unit.

## Architecture findings

1. `rcu-on-vm` is a read-mostly publication lane, not a scheduler replacement.
   `PageContainer` publishes an immutable `ResidentRoot` containing stable
   `ResidentCell` bindings. Reads use an epoch `Guard`; the cell obtains an
   independent `MapPin` before the guard ends. PageSlot generation/dirty/
   writeback state, I/O manager custody, `RangeLock`, and pmap/shootdown remain
   mutable coordination domains. Evidence: `crates/tx-subsystems/src/page_backed/resident.rs`
   (`ResidentCell`, `ResidentRoot`, `ResidentHit`) and
   `crates/tx-subsystems/src/page_backed/mod.rs` (`prepare_resident_root_mutation`,
   `lookup_resident_with_guard`, withdrawal paths).

2. The EBR substrate already has a cross-hart maintenance protocol. A failed
   epoch advance requests `IpiKind::Maintenance` for lagging CPUs; the trap
   handler only acknowledges and records a pending bit; normal kernel context
   calls the local drain and bounded zone maintenance service. Evidence:
   `crates/tx-substrate/src/epoch/domain.rs` (`try_advance_epoch_with`,
   `request_maintenance`) and `crates/tx-kernel/src/trap.rs`
   (`dispatch_pending_ipis`, `service_pending_maintenance`).

3. The current SMP lane already owns the right scheduling surfaces: per-hart
   queue shards, wake placement, work stealing, userspace-preempt markers, and
   a WFI idle boundary. Evidence: `crates/tx-reactor/src/scheduler.rs`
   (`HartSchedulerLocal`, `try_steal_from_locals`, `task_runnable_inner_for_locals`)
   and `crates/tx-reactor/src/runtime.rs` (`run_hart_loop_concurrent`).

4. The current SMP boot/reactor path does **not** yet initialize or drain the
   VM per-hart cleanup domain. `init_vm_range_txn_hart`,
   `drain_local_vm_cleanup`, and the shutdown/maintenance calls are present in
   the dirty `rcu-on-vm` `init.rs`/`init/exec.rs`, but are absent from the
   current SMP worktree. This is an integration prerequisite, not evidence
   that the current scheduler already services VM cleanup.

## Fusion decision

Fuse the two lanes at a kernel-owned per-hart progress service, not inside
`ResidentRoot`, `PageSlot`, or the scheduler queues:

```text
trap IPI
  |-- Reschedule ----------> scheduler marker / wake placement
  |-- Maintenance ---------> per-hart maintenance-pending bit
  |-- TlbShootdown ---------> HAL/pmap acknowledgement

hart normal-context boundary
  -> service maintenance-pending (bounded epoch/zone drain)
  -> drain mailbox/wake inputs
  -> pick or steal and poll one task
  -> commit task state; no Guard survives Pending/yield
  -> repeat while work or retire backlog exists
  -> WFI only when scheduler, timer, and maintenance work are all empty
```

`Maintenance` and `Reschedule` must remain separate bits and separate IPI
meanings. A maintenance interrupt must not inspect queues or allocate in trap
context. A reschedule interrupt must not call EBR drain. The existing trap
pending-bit design can be retained; the fusion work should make the reactor
boundary the canonical caller of the bounded service instead of adding a second
RCU driver.

The VM ordering remains:

```text
Guard -> immutable root read -> acquire MapPin -> drop Guard
RangeLock + generation recheck -> slot transition -> root publication
-> pmap teardown/shootdown receipt -> release mapping evidence
```

Root retirement never revokes a PTE and never replaces `RangeLock`, PageSlot,
DMA/page-lease, or shootdown ownership. Guards and `IdentRef` values must not
cross an async wait or scheduler yield.

## Integration blockers

1. The active SMP design calls for an atomic current-hart ownership field and a
   lock-and-recheck wake/steal protocol. The current implementation instead
   pops a victim queue, drops that queue lock, then updates shared task metadata
   and finally pushes into the thief queue. Wake placement similarly updates
   metadata and queue membership in separate operations. This is a scheduler
   linearization gap that must close before RCU maintenance is used as a shared
   progress contract. Evidence: `crates/tx-reactor/src/scheduler.rs`,
   `try_steal_from_locals` and `task_runnable_inner_for_locals`.

2. VM hart ownership is not wired in the current SMP lifecycle: BSP/AP setup
   and the normal-context cleanup drain must be added before a resident-root
   publication can rely on owner-hart reclamation. The RCU branch's dirty
   `init.rs` is a source reference only; it is not a mergeable integration
   unit.

3. Drain pressure is expressed inconsistently: the maintenance service is
   bounded, while some idle-loop paths use an unlimited drain budget. The fused
   service should use one bounded budget, report remaining retire work, and set
   a follow-up maintenance/reschedule marker instead of doing unbounded work in
   an idle transition.

4. The `rcu-on-vm` checkout contains unrelated dirty ext4, network, HAL, and
   OSComp changes, plus reactor diagnostics. Only the resident-root publication
   commits and their focused tests are candidates for a later port. The closed
   worktree record itself reports JBD2 fixture failures and guest blockers, so
   it is not a green integration baseline.

## Concurrency boundary recheck

The scheduler gap has two concrete interleaving windows, not only a general
design mismatch:

1. `try_steal_from_locals` removes a task from the victim queue, releases that
   queue lock, updates `TaskRunOwner`, and then acquires the thief queue lock to
   insert it. A wake or affinity move can observe the old owner after physical
   removal, or the new owner before physical insertion. Queue membership and
   logical ownership therefore do not have one linearization point.

2. `pick_next_from_local` removes a task and marks scheduler metadata as
   `Polling` before `TaskTable::take_runnable_future_by_id` changes task status
   from `Runnable` to `Polling`. An owner-aware wake in that interval can still
   observe the task-table state as runnable and create a new queued ownership
   transition for a task whose original poll lease is about to start. The
   scheduler metadata, task lifecycle, and queue operation must be combined by
   one queue-locked transition or a generation/sequence-checked retry protocol.

The userspace and hart-local checks narrow the remaining scope:

- Production `run_thread` uses the `UserspaceRunSlot` stored in each
  `ThreadPayload`, plus per-hart current-payload registries. The single
  `ReactorShared.userspace` slot is a compatibility facade and is not the
  production thread round-trip owner, so it does not serialize all user
  threads.
- `ReactorLocals::ensure_hart` serializes allocation with `init_lock`, publishes
  each leaked local through a Release pointer store, and reads it with Acquire.
  No concurrent initialization defect was found in this audit.
- The task mailbox/deadline/delegate trampolines still use an eight-hart static
  array while reactor locals and thread-payload registries permit 64 harts.
  This does not affect the requested 1-hart/2-hart witness, but harts 8 and
  above silently fall back from task-owned routing and require a later common
  per-hart substrate.

Focused host verification passed `cargo test -p tx-reactor --test scheduler`
(45 tests) and `cargo test -p tx-reactor --test userspace_run --test hart_loop`
(21 + 7 tests). These are deterministic functional tests; they do not exercise
the wake-versus-steal or pick-versus-wake interleavings above and therefore are
not evidence that the SMP race is closed.

## Recommended implementation order

1. Freeze the per-hart progress contract and close the scheduler queue/meta
   linearization gap; add a race witness for wake-versus-steal.
2. Add BSP/AP `init_vm_range_txn_hart` setup and a single bounded normal-context
   maintenance service that drains EBR, VM cleanup, and zone work outside the
   reactor queue/view locks.
3. Rebase/port only the committed `ResidentRoot` and page-backed withdrawal
   slices onto the current SMP branch. Keep PageSlot, I/O, RangeLock, and pmap
   ownership unchanged.
4. Route all EBR/zone service through the fused normal-context per-hart hook;
   retain the existing trap-only IPI acknowledgement rule.
5. Add host tests for guard lifetime, bounded retire backpressure, wake/steal
   ordering, and maintenance re-entry; then run the 2x SMP witness.
6. Run the RV64 2-hart RCU smoke plus focused page-backed/VM and OSComp
   page-fault witnesses. Do not claim a scheduler performance result until the
   same-source 1-hart/2-hart pair reaches the same group-end receipts.

## Current status

This is an architecture audit only. No source code was changed. The main
blockers are the scheduler ownership linearization gap, inconsistent drain
budgeting, missing VM per-hart lifecycle wiring, and the dirty/non-green
`rcu-on-vm` integration surface.
