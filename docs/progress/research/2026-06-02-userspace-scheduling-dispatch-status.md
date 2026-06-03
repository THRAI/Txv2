# 2026-06-02: Userspace scheduling dispatch status and blast radius

## Executive summary

Ready: no for default OSComp userspace work dispatch.

The reactor already has multi-hart queueing, remote wake/IPI dispatch,
work-stealing, and rebalance machinery. APs also boot and enter the secondary
reactor loop. However, OSComp userspace threads are intentionally pinned to the
submit hart today. The pin is explicit in
`crates/tx-kernel/src/init.rs:1720`: `userspace_thread_sched_meta_for` builds a
single-bit affinity mask for the current CPU and calls `.pinned()`, with a
comment saying userspace trap/return state still has hart-local architectural
coupling.

The 2026-06-02 full pthread process-DS observe run confirms the effect:
all decoded spans and process DS rows are on hart 1, work-steal and rebalance
counters are zero in the boot scheduler smoke line, and AP observe rings have
zero producers. The AP ring data alone is not sufficient proof because observe
initialization currently happens only on the BSP path, but the scheduling policy
is sufficient proof that OSComp userspace threads cannot be stolen or rebalanced
today.

Enabling dispatch should therefore be staged behind a gate or boot parameter.
The first stage should enable all-hart observability and a non-migrating spread
experiment only after the userspace trap handoff and per-hart slot lifetime are
audited. Full migration/stealing should remain off until stale userspace slots,
ASID residency/shootdown, wake/IPI volume, affinity syscalls, and signal/mailbox
wakes are verified under QEMU.

## Current evidence

### Full pthread observe run

Artifact:
`target/oscomp/custom-run/process-ds-pthread-retry-20260602-200224`

Command:

```sh
RUSTFLAGS="--cfg tx_ds_metrics --cfg tx_ds_metrics_process" \
  cargo xtask observe oscomp-live \
    --name process-ds-pthread-retry-20260602-200224 \
    --test pthread \
    --timeout 900
```

Capture quality:

| field | value |
| --- | ---: |
| `complete` | true |
| `raw_records` | 6,923,021 |
| `lost_records` | 0 |
| `overwritten_records` | 0 |
| `repairs` | 0 |
| `hart_count` | 4 |

Per-ring producers from `host/runtime.json`:

| hart | producer | consumer | lost | overwritten |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 0 | 0 | 0 | 0 |
| 1 | 6,923,020 | 6,923,020 | 0 | 0 |
| 2 | 0 | 0 | 0 | 0 |
| 3 | 0 | 0 | 0 | 0 |

Decoded placement queries:

```sql
SELECT begin_hart, end_hart, sched_migrated, count(*) AS spans
FROM spans
GROUP BY begin_hart, end_hart, sched_migrated;
```

| begin_hart | end_hart | sched_migrated | spans |
| ---: | ---: | --- | ---: |
| 1 | 1 | NULL | 142,193 |

```sql
SELECT hart, count(*) AS ds_rows
FROM ds_method_rows
GROUP BY hart;
```

| hart | process DS rows |
| ---: | ---: |
| 1 | 50,089 |

Serial boot proof:

| serial line | signal |
| --- | --- |
| `Platform HART Count : 4` | QEMU platform exposed four harts. |
| `Domain0 Boot HART : 1` / `Boot HART ID : 1` | BSP/userspace submit hart was hart 1. |
| `smp:aps:online` | APs reached online state. |
| `reactor:ap-loop:ok` / `reactor:ap-runqueue:ok` | AP reactor smoke ran. |
| `reactor:sched:stats:h0=1/1:h1=3/2:steal=0:rebalance=0` | Boot smoke saw some AP/BSP scheduler activity, but no stealing or rebalance. |
| `OS COMP TEST GROUP END libcbench-musl` and `userspace:exited:0` | Full pthread test completed. |

Interpretation: the full pthread workload did not distribute OSComp userspace
work. The current run does not prove that APs cannot emit observe records in all
future configurations; it proves this run had no AP observe producers, and the
code policy explains why OSComp userspace did not move.

## Current code status

| area | status | code anchor |
| --- | --- | --- |
| AP boot and reactor loop | Present. AP entry initializes per-CPU state, marks the CPU online, enables IPI/timer wakeups, drives `run_secondary_reactor_once`, polls idle, then WFI/acks reschedule IPIs. | `crates/tx-kernel/src/init.rs:1559`, `crates/tx-kernel/src/init.rs:1570` |
| BSP observe init | BSP only. `drive_bootstrap_exec` calls `tx_observe::init::<P>(bsp)` and installs dump hooks. AP entry does not initialize observe. | `crates/tx-kernel/src/init/exec.rs:335` |
| Initial userspace submission | Initial userspace thread is submitted from the current hart with `Self::userspace_thread_sched_meta()`. | `crates/tx-kernel/src/init/exec.rs:638` |
| Pthread child submission | Clone path records current CPU as submit CPU and submits child threads with `userspace_thread_sched_meta_for(submit_cpu).preempted_on_submit()`. | `crates/tx-kernel/src/init/reactor_submit.rs:42`, `crates/tx-kernel/src/init/reactor_submit.rs:185` |
| Userspace scheduling metadata | Single-hart affinity plus pinned migration policy. This is the current dispatch gate. | `crates/tx-kernel/src/init.rs:1720` |
| Scheduler migration flag | `.pinned()` sets `MigrationPolicy::Pinned`; task metadata records `can_migrate` only when policy is `Movable`. | `crates/tx-reactor/src/scheduler.rs:132`, `crates/tx-reactor/src/scheduler.rs:191` |
| Initial spread | Only runs when the task is not kernel-only, can migrate, and has `spread_on_submit`; pinned userspace never reaches this path. | `crates/tx-reactor/src/scheduler.rs:1114` |
| Work stealing | Implemented, but rejects tasks unless `can_migrate`, non-kernel, not recently stolen, affinity allows thief, and task is queued on victim. Pinned userspace is ineligible. | `crates/tx-reactor/src/scheduler.rs:967` |
| Rebalance | Implemented via per-hart loop maintenance and scheduler stats. It ultimately uses the same steal eligibility. | `crates/tx-reactor/src/runtime.rs:489`, `crates/tx-reactor/src/runtime.rs:1016` |
| Remote wake/IPI | Runnable placement marks the target hart need-resched and sends an IPI if the placement is remote and the platform signal sends one. | `crates/tx-reactor/src/runtime.rs:1087`, `crates/tx-kernel/src/init/helpers.rs:20` |
| Affinity syscall surface | `sched_setaffinity` / `sched_getaffinity` are wired through the reactor affinity seam, but `getcpu` still writes CPU 0 and node 0. | `crates/tx-shims/src/linux_syscall/proc.rs:98`, `crates/tx-shims/src/linux_syscall/proc.rs:1031` |

## Why work is on one hart today

The policy chain is direct:

1. `userspace_thread_sched_meta_for(cpu_id)` computes `affinity =
   Self::cpu_bit(cpu_id)`.
2. It returns `InitialSchedMeta::fair().with_affinity(affinity).pinned().userspace_thread()`.
3. `InitialSchedMeta::pinned()` sets `migration = MigrationPolicy::Pinned`.
4. Scheduler metadata sets `can_migrate = initial_meta.migration ==
   MigrationPolicy::Movable`.
5. Initial placement returns `first_hart_in_mask(meta.affinity)` whenever
   `!meta.can_migrate || !meta.spread_on_submit`.
6. Steal eligibility requires `meta.can_migrate`, so pinned userspace can never
   be stolen or rebalanced.

With the current OSComp flow, the first user thread is submitted on boot hart 1,
and cloned children use the current CPU as their submit CPU. Since the initial
thread never migrates, child submissions also keep resolving to hart 1.

## Blast radius for enabling userspace dispatch

### 1. Policy flip point

Small code change, high correctness impact.

The obvious flip is `userspace_thread_sched_meta_for`: use an online-CPU mask
instead of `cpu_bit(submit_cpu)`, call `.movable()`, and probably call
`.spread_on_submit()`. That is only one policy site, but it changes the
contract assumed by trap handoff, userspace slots, signal wakes, TLB
invalidation, and Linux-visible affinity.

Recommendation: add an explicit gate, for example a cfg plus a boot parameter:

- default: current pinned behavior
- stage 1: wide affinity and `spread_on_submit`, but still non-stealable after
  entry if the trap-slot audit needs a middle step
- stage 2: movable userspace with stealing/rebalance enabled

### 2. Trap handoff and per-hart userspace slots

Highest correctness risk.

`PerHartSlotted` sets `current_thread_payload(hart)` only during each poll and
clears it on poll exit. `run_thread` separately sets
`current_userspace_payload(entry_hart)` immediately before
`enter_userspace_with_context`; that userspace slot intentionally spans the
machine userspace round trip. The trap shell resolves a syscall or user page
fault via `current_payload_for_hart(hart) =
current_thread_payload(hart).or_else(current_userspace_payload(hart))`.

Migration changes the lifetime assumptions. If a thread enters userspace on
hart A, traps on hart A, is requeued, and later re-enters userspace on hart B,
the old hart's userspace slot must be cleared at exactly the right time. Today
`run_thread` clears the userspace slot after `entry_wait.await`, except for
`TimerPreempt`. Timer preempt deliberately preserves the slot so a later trap
can still find the payload. That is sensible for pinned execution, but it needs
a stale-slot audit before user threads can move.

Audit target:

- add a test where the same userspace task enters on hart A, resolves a timer
  preempt, is repolled on hart B, and then traps again
- assert the old hart slot does not incorrectly hand off a new trap to the old
  payload after migration
- assert `current_userspace_thread_identity` follows the payload slot exactly

### 3. RV64 userspace resume context

High correctness risk.

`enter_userspace_with_context` writes a per-hart `KernelResumeCtx`, activates
the user pmap, and enters user mode. On a from-user `TrapAction::Reschedule`,
the trap path longjmps back through the current hart's resume context. This is
hart-local by design; it is safe only if the trap returns to the same hart that
entered userspace. That is true for a single userspace round trip, but movable
threads make the next round trip potentially use a different hart.

Required proof: migration may happen only after the prior userspace round trip
has fully unwound to the Rust future and the current hart's resume context is no
longer active. No code should attempt to resume a userspace entry on a different
hart mid-round-trip.

### 4. VM, ASID residency, and remote shootdown

Medium-high correctness and performance risk.

The HAL has ASID-shaped support:

- userspace entry calls `activate_user_pmap(root)`, which marks the root ASID
  resident on the current CPU and writes `satp`
- from-user reschedule clears current ASID residency before returning to the
  kernel future
- ASID-scoped remote `sfence.vma` targets the ASID residency mask minus the
  current CPU
- batched pmap invalidation coalesces ranges before calling remote ASID shootdown

This is the right shape for migration, but it is not yet proven under movable
userspace. With dispatch enabled, the same process ASID can become resident on
multiple harts over a pthread run. `mmap`, `munmap`, and `mprotect` are already
major pthread costs, so remote shootdown target volume must be measured rather
than guessed.

### 5. Wake placement, IPIs, and mailbox behavior

Medium correctness and performance risk.

Runnable placement already returns a target hart and `wake_remote`; runtime
marks that hart need-resched and asks `SmpRescheduleSignal` to send a reschedule
IPI unless the target hart is in the polling-idle window. With userspace spread
or migration enabled, clone, futex wake, signal wake, and lifecycle wake can all
become remote wake paths.

The mailbox binding is updated during `PerHartSlotted::poll` from the current
task mailbox. That should follow a migrated reactor task, but it needs an
observe-backed stress test for signal delivery and futex wake handoff.

### 6. Linux-visible scheduling ABI

Medium correctness risk.

`sched_setaffinity` and `sched_getaffinity` already use the reactor affinity
seam. Once tasks are genuinely movable, this surface becomes observable by
tests. `getcpu`, however, still writes CPU 0/node 0 unconditionally. That was
acceptable under a single-user-hart bring-up model, but it becomes misleading
when user code actually runs on multiple harts.

Required follow-up: make `getcpu` report the current hart/CPU under the syscall
context, or explicitly gate userspace dispatch away from tests that rely on
`getcpu` until that syscall is fixed.

### 7. Observability

Must happen before judging dispatch performance.

Current live drain can read all rings, but kernel observe initialization is
called only on the BSP bootstrap exec path. That explains why AP producer
counters are zero in the current run even though APs booted. Before enabling
userspace dispatch experiments, AP entry should initialize its observe producer
for its hart, and the report should include:

- per-hart producer/consumer/loss stats
- per-hart task poll/completion stats at shutdown, not just boot smoke
- scheduler placement rows: submit target, wake target, steal source/target,
  rebalance source/target
- remote IPI counts and polling-idle suppression counts
- ASID residency/shootdown target-mask counters

## Suggested staged enablement plan

1. Observability first.

   Initialize observe on APs and add final-run scheduler summaries. Re-run
   pthread with current pinned userspace to prove all-hart observe plumbing is
   live without changing scheduling behavior.

2. Keep current default pinned, add an experimental gate.

   Add a cfg/boot parameter for userspace dispatch. Do not change default
   OSComp behavior until the gated path survives QEMU.

3. Stage initial spread without stealing if needed.

   New pthread children can be submitted with wide affinity and
   `spread_on_submit`, while post-entry migration remains disabled. This tests
   AP userspace entry, trap handoff, pmap activation, and AP observe with less
   stale-slot risk than full steal/rebalance.

4. Add stale-slot and resume-context tests.

   Host tests should model A-to-B repoll after timer preempt and after syscall
   trap. QEMU tests should include timer preemption under a multi-hart pthread
   workload.

5. Enable full movable userspace behind the gate.

   Use `.movable().spread_on_submit()` and wide affinity. Then allow
   `try_steal_from_locals` and rebalance to move queued userspace tasks. Keep
   the gate off by default until the trace shows stable multi-hart execution.

6. Promote only with evidence.

   Promotion criteria should include complete/lossless observe, rows on all
   expected harts, no fault-decode traps, all pthread bodies complete, nonzero
   placement across harts, and a before/after wall-time comparison that does
   not trade correctness for IPI/shootdown overhead.

## Verification matrix

| stage | checks |
| --- | --- |
| Docs/recon only | `cargo xtask progress validate`; `cargo xtask lint docs`; scoped `git diff --check`. |
| AP observe init | focused observe tests plus a pinned pthread live run; assert AP producers are nonzero during AP smoke and loss counters remain zero. |
| Scheduler policy tests | host tests for wide-affinity userspace initial spread, pinned userspace non-stealability, movable userspace steal eligibility, and affinity movement. |
| Trap/slot migration tests | host tests for old-hart slot cleanup across timer preempt, syscall trap, and page fault; QEMU timer-preempt smoke under dispatch gate. |
| VM/TLB tests | existing ASID residency tests plus QEMU counters for ASID remote target masks during `mmap`/`munmap`/`mprotect`. |
| OSComp gated run | `cargo xtask observe oscomp-live --test pthread` with AP observe and dispatch gate; inspect per-hart spans, scheduler rows, remote IPIs, ASID shootdowns, and fault-decode serial log if traps appear. |

## Current answer to "is all work on one hart?"

For OSComp userspace in the measured full pthread run: yes. All decoded spans
and process DS method rows are on hart 1, and the userspace scheduling metadata
forces single-hart pinned placement.

For the whole kernel: no. APs did boot, AP reactor smoke did run, and the boot
scheduler smoke showed some task polling/completion on hart 0 and hart 1. What
is missing is userspace work dispatch across APs during the pthread workload,
not AP bring-up itself.

## Bottom line

The dispatch machinery is present; the userspace policy deliberately prevents
it from being used. The blast radius of flipping that policy is broad because
the hardest dependencies are not scheduler queue mechanics, but userspace
trap/resume lifetime, stale per-hart payload slots, pmap/ASID shootdown
correctness, Linux-visible affinity/getcpu semantics, and all-hart observation.

Do not enable work dispatch by only changing the affinity mask. Make it a gated
experiment with AP observe first, then prove initial spread, then prove full
steal/rebalance under lossless QEMU traces.
