# pthread lifecycle Linux optimization audit

Date: 2026-05-30

Scope: compare Tx's current pthread create -> run -> exit -> join behavior with
the Linux optimization stack that matters for musl libcbench-style serial
create/join loops.

## Summary

Tx already has the largest Linux-style pthread creation win: `clone()` with
`CLONE_THREAD | CLONE_VM` stays inside the existing process/address-space
payload instead of copying an address space. The remaining high-cost surface is
not a fork-like copy. It is the combination of musl's deliberate stack
`mmap`/`munmap` traffic and Tx's conservative VM/TLB and wake-delivery policy.

For the musl serial pthread tests, rank the next blockers as:

1. `munmap` pmap teardown and TLB shootdown policy.
2. Remote wake IPI policy for join/clear-child-tid wakeups.
3. Per-thread zone/task allocation and terminal cleanup churn.
4. `rt_sigprocmask` direct state updates, which prior traces show are usually
   cheap once user-page/pagebacked fault envelopes are removed.

## Linux/libc reference points

- Linux pthread creation uses `clone`/`clone3` with `CLONE_VM`,
  `CLONE_FILES`, `CLONE_SIGHAND`, and `CLONE_THREAD`. The key MM optimization is
  that `copy_mm()` shares the caller's `mm_struct` under `CLONE_VM`, avoiding
  page-table copy and fork-style COW setup.
- glibc NPTL keeps a user-stack cache, so steady-state serial create/join can
  often avoid new stack `mmap`/`munmap` syscalls.
- musl does not have that cache. In the checked-in musl source,
  `pthread_create` maps the stack/TLS/guard region with `__mmap` and records
  `map_base`/`map_size`; `pthread_join` calls `__munmap(t->map_base,
  t->map_size)` for joinable threads. Detached thread exit uses
  `__unmapself`.
- Linux clear-child-tid join uses the clone-time child TID pointer: thread exit
  clears the user word and wakes the futex address. The joiner checks the word
  in userspace before sleeping, so already-exited joins avoid a wait syscall.
- Linux TLB invalidation targets the CPUs that have run the address space
  (`mm_cpumask`) and can use ASID/PCID/lazy-TLB behavior rather than broadcast
  to every CPU for every unmap.
- Linux scheduler wakeup avoids an IPI when the target CPU is in a polling idle
  state: the waker sets a need-resched flag that the idle loop is already
  checking, and sends an IPI only when the remote CPU really needs an interrupt.

## Tx current behavior

### Create

`sys_clone_oneshot` accepts the musl pthread flag set for `CLONE_THREAD` and
requires `CLONE_SIGHAND` on thread clones. It dispatches the thread case through
`step_clone_thread`, which allocates a TID, signs a fresh `ThreadPayload` and
`ThreadIdentity`, records the clear-child-tid pointer, attaches the thread to
the existing process payload, and submits it to the reactor.

This is aligned with Linux's `CLONE_VM` shape at the address-space level: no new
`ProcessIdentity` or `AddressSpace` is constructed for pthread creation.

Residual cost: Tx still signs fresh zone objects and constructs reactor task
state for each thread. That is expected to matter less than VM/TLB churn for
musl stack cycles, but it remains a second-order lifecycle cost.

### musl stack map/unmap

The local musl source makes the important benchmark distinction explicit:

- `external/musl/src/thread/pthread_create.c` maps stack/TLS/guard memory with
  `__mmap` and stores `new->map_base` / `new->map_size`.
- `external/musl/src/thread/pthread_join.c` unmaps that region for joinable
  threads.
- `external/musl/src/thread/riscv64/__unmapself.s` routes detached self-unmap
  through `SYS_munmap`.

So Tx cannot assume a glibc-like hot loop where the kernel sees only clone plus
futex. Under musl, every serial create/join iteration can drive VM map/unmap
work.

### `munmap` and TLB shootdown

`sys_munmap` first tries `ctx.aspace.try_munmap(range)`. `try_munmap` acquires
the VM writer range, rewrites recipes, and calls `self.pmap.teardown_range`.

`VmPmap::teardown_range` gathers resident pages in the range, then loops
page-by-page. Each removed page calls `issue_single_unmap_result`, which calls
`shootdown_mappings(asid, &[single_invalidation])`.

On RV64 QEMU, `PmapIf::shootdown_mapping` performs:

- local `pmap::shootdown_mapping(asid, invalidation)`, whose current board
  implementation ignores the ASID/range and calls full local `sfence.vma`; and
- `remote_sfence_vma_asid(asid, invalidation)`.

Remote RFENCE targets are `online_cpus() - current_cpu`. There is no
AddressSpace/mm residency mask equivalent to Linux `mm_cpumask`, and no
batching layer that coalesces a stack unmap into one range/batch.

Consequences for a musl pthread stack unmap:

- a stack with N resident pages can produce N local full fences;
- it can also produce N remote SBI RFENCE calls; and
- each remote call targets all online harts except the current hart, even if
  only one or two harts have run the target address space.

This is the highest-confidence absolute-cost blocker from the Linux comparison.

### Exit and clear-child-tid join wake

`step_thread_exit` snapshots `clear_child_tid`, writes zero through
`AddressSpace::copy_to_user`, and calls `step_futex_lifecycle_wake_in(aspace,
tid_ptr, 1, guard)`.

Futex wait/wake identity is address-space scoped. Ordinary futex wake uses
`MailboxSchedulerHint::WakeHandoff`; clear-child-tid uses
`MailboxSchedulerHint::LifecycleWake`, which is stronger and routes to boosted
scheduler placement.

This is broadly aligned with Linux's clear-child-tid + futex wake model.

### Scheduler wake and IPI

Futex wake posts a wait-source event into the target task mailbox, latches the
scheduler hint, and wakes the task waker. The reactor drains the wake, maps the
mailbox hint into a scheduler wake hint, chooses placement, and sets
`wake_remote = target_hart != current_hart`.

Same-hart placement avoids remote IPI and uses local reschedule/preempt markers.
That part is already the right shape.

Remote placement unconditionally calls `send_reschedule_ipi` when
`wake_remote` is true. The kernel helper forwards that to `SmpIf::send_ipi`,
and the RV64 HAL suppresses only self-IPI before sending an SBI IPI. Secondary
harts run work, then execute `wfi`, then acknowledge pending reschedule IPIs
after wake. There is no published per-hart polling-idle flag and no
set-need-resched-without-IPI handshake comparable to Linux's polling idle path.

This is the second highest-confidence blocker for join wake cost.

## Investigation plan

1. Measure before changing policy.
   - Add sampled counters around `VmPmap::teardown_range`: pages removed,
     invalidation count, shootdown call count, and whether a call was single
     page or batch/range.
   - Add RV64 RFENCE counters: ASID, invalidation size, remote target mask,
     target count, and count of SBI RFENCE calls.
   - Add reactor wake counters: source hint, current hart, target hart,
     `wake_remote`, remote IPI sent, and whether the target was observed idle
     once idle-state tracking exists.

2. Fix pmap policy first.
   - Batch invalidations produced by one `teardown_range` and call
     `PmapIf::shootdown_mappings` once per range operation.
   - Override the RV64 plural shootdown implementation so it emits one
     range/batch RFENCE where legal instead of N singleton RFENCEs.
   - Replace local full `sfence.vma` with ASID/range-shaped local fences when
     supported by the board pmap policy.
   - Add an AddressSpace/ASID residency mask updated on user pmap activation;
     remote RFENCE should target only harts in that mask, excluding current.

3. Then fix scheduler wake policy.
   - Add per-hart idle/polling state around the AP idle loop.
   - Teach remote wake dispatch to set the target hart's need-resched marker
     without IPI when the target is in polling idle and will observe it
     promptly.
   - Keep the current SBI IPI path for non-idle remote harts or uncertain
     states.

4. Re-run bounded evidence.
   - Use single-hart and SMP4 `pthread-minimal1` / `pthread_createjoin_serial1`
     windows with observe counters.
   - Compare per-iteration `sys_munmap`, RFENCE count/target count, futex
     lifecycle wake latency, remote IPI count, and total libcbench pthread
     timings.

## Current caveats

- This audit did not implement code changes.
- The Linux/glibc reference points are primary-source aligned, but only the Tx
  and local musl paths were line-checked in this pass.
- The scheduler worker and pmap worker were read-only; both independently
  reported the same gaps: no idle-polling IPI suppression, no pmap shootdown
  batching, and no ASID/mm residency target mask.
