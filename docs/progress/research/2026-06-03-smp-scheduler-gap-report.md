# 2026-06-03: SMP userspace scheduling module gap report

## Scope

This report consolidates the 2026-06-02/03 read-only subagent audits for the
planned userspace SMP scheduling migration. It is a gap ledger, not an
implementation plan. The question it answers is:

> What must be fixed or measured before OSComp userspace threads can move from
> the current pinned policy to cross-hart spread, wake placement, and eventual
> stealing?

The modules are grouped by ownership boundary:

- Substrate EBR / Zone / Cap
- Process / ThreadRuntime / Signal
- VM / PageBacked / VFS / filesystem backends
- Reactor / Scheduler / wake substrate
- Observe / vocabulary / lint infrastructure

Priority notation:

- **P0**: block userspace SMP migration or explain a current 30ms-class tail.
- **P1**: must be addressed before default-on migration, but can follow a gated
  experiment.
- **P2**: cleanup, doc/lint hardening, or performance hygiene.

## Executive summary

The current scheduler should remain pinned by default. `tx-reactor` has
multi-hart mechanisms, but several semantic subsystems still have large
lock-held service regions, incomplete read-mostly EBR lookup performance, or
doc/code mismatches in paths that would become hot once userspace can migrate.

The reported `observe -> upgrade` 30ms gap is unlikely to be the
`IdentRef::to_cap()` CAS loop alone. Current cap-upgrade probes show no retry
pressure in the sampled `sigprocmask` run: sampled upgrades had one attempt and
zero retries. The more plausible buckets are:

1. `Weak::observe()` and `registry::slot_for()` lookup work before `to_cap()`.
2. Zone `slot_from_key()` resolving through registry/Keg locks and slab-list
   scans.
3. Caller-side locks that clone/drop `Cap` or perform lifecycle work while a
   coarse lock is held.
4. VM/PageBacked cold paths and pmap publish/teardown service time.
5. Observe attribution gaps: current counters can show the hole, but not yet
   cleanly assign it to observe, lookup, upgrade, lock service, DS method, or
   step phase.

The right next move is not to flip `.pinned()` to `.movable()`. The first gate
is attribution: all-hart observe, slot lookup metrics, typed cap/phase rows,
and lock-service rows that can be joined to lock rows. The second gate is P0
correctness and critical-section shrinkage.

## Migration gate checklist

| Gate | Required before | Status |
| --- | --- | --- |
| AP observe initialization and final per-hart scheduler summaries | Any cross-hart userspace experiment | Open |
| Userspace migration boot/cfg gate, default pinned | Submit-spread experiment | Required |
| Clone attach transaction or rollback | Cross-hart clone-heavy pthread tests | Open |
| Process/thread exit two-phase teardown | Default-on userspace SMP | Open |
| Robust-list walk outside process payload lock | pthread default-on migration | Open |
| Slot lookup attribution for `Weak::observe()` / `registry::slot_for()` | Explaining 30ms observe-upgrade tails | Open |
| RangeLock writer retention decision | VM-heavy SMP workload | Open |
| pmap publish/teardown counters and ASID residency/shootdown audit | User task migration | Open |
| PageBacked in-flight per-page dedup | File-backed cold faults on SMP | Open |
| Remote wake/IPI attribution | Wake-time placement | Open |
| L5 step phase rows or equivalent observe/upgrade typed rows | Performance acceptance | Open |

## Substrate EBR / Zone / Cap

### Current state

- `Weak<T> -> IdentRef<'g, T> -> Cap<T>` is implemented with guard-scoped
  observation and generation checks.
- `IdentRef::to_cap()` is a metadata CAS loop that validates generation/state
  and increments retain count.
- Optional `tx_cap_upgrade_metrics` counters exist for slow/retried cap
  upgrades:
  - `debug.cap.upgrade.to_cap.duration_ns`
  - `debug.cap.upgrade.to_cap.attempts`
  - `debug.cap.upgrade.to_cap.retries`
- Per-CPU zone buckets exist, and zone/page-allocator DS metrics are available
  behind `tx_ds_metrics` plus local gates.

### Gaps

| Gap | Evidence | Risk | Priority |
| --- | --- | --- | --- |
| `Weak::observe()` lookup is not included in cap-upgrade metrics | `IdentRef::to_cap()` metrics start after the slot is already known | A reported "observe-upgrade" span may blame cap upgrade while lookup or caller locks dominate | P0 |
| `registry::slot_for()` / `slot_from_key()` can take registry/Keg locks and scan slab lists | Substrate audit found lock + list-scan key resolution | Read-mostly Cap/Weak lookup is not yet the O(1) SMP read path the docs imply | P0 |
| Zone internal locks lack the same metrics as public `SpinMutex<T, LockMetricsOn>` | `zone::sync::SpinLock` is separate | Slot lookup and Keg service time can hide outside `lock_rows` | P1 |
| `PayloadCap<T>` mostly wraps `Cap<T>` today | Split payload evidence is mostly type-surface | Caller locks may still clone/drop retaining caps rather than cheap payload evidence | P1 |
| Reclaim callback can run `drop_in_place(T)` | EBR callback drops `T` then returns slot | Large destructors can produce reclaim service spikes under churn | P1 |
| 16-bit generation wrap remains an assumption | Generation is packed in slot metadata | High churn plus long-lived stale weak handles may need quarantine or wider generation | P2 |

### Migration gate

Before userspace SMP migration is default-on:

1. Add `debug.ds.substrate.zone.slot_from_key` and
   `debug.ds.substrate.zone.registry_slot_for` or equivalent typed rows.
2. Add observed-lock coverage for registry/Keg if that design remains.
3. Decide whether key-to-slot resolution must become O(1) before default-on SMP.
4. Audit zone-backed `Drop` implementations for bounded reclaim behavior.

## Process / ThreadRuntime / Signal

### Current state

- Process and thread entities use role-shaped `Cap`, `Weak`, and `PayloadCap`.
- Process payload liveness is still a lock-protected
  `SpinMutex<Option<PayloadCap<ProcessPayload>>>`.
- Process topology containers are `SpinMutex<Vec<...>>` wrappers for children,
  threads, process-group members, and session members.
- PID/TID namespace is a single
  `SpinMutex<BTreeMap<(u64, PidNameKind), PidName>>`.
- Process DS metrics and process lock-service counters now exist for several
  pthread lifecycle paths.

### Gaps

| Gap | Evidence | Risk | Priority |
| --- | --- | --- | --- |
| Clone thread register/attach race | `step_clone_thread` allocates/signs/registers before conditional payload attach | Process exit can race and leave a registered unattached thread path | P0 |
| Exit group/process exit holds payload lock while doing teardown | `step_exit_group` drains shm/fds/threads, zombifies, unregisters, drops refs under payload lock | Large lock service tails; cross-hart migration amplifies service-in-lock | P0 |
| Robust-list walk holds payload lock while reading user memory and waking futexes | `walk_robust_list` snapshots aspace while guard/payload lock remain in scope | User-memory and futex work can create 30ms-class service gaps | P0 |
| PID namespace global lock and closure-under-lock shape | `with_namespace` runs caller closure while locked | Projection/procfs or namespace scans can serialize unrelated process work | P1 |
| Process rosters are linear `Vec` snapshots/drains under lock | children/threads/group/session wrappers | Signal fanout, wait, exit storms clone/drop caps while locked | P1 |
| Signal action table is table-level locked | Docs call for per-entry atomic/CAS; code uses `SpinMutex<[SigActionEntry; NSIG]>` | Direct doc/code mismatch and avoidable signal contention | P1 |
| Signal routing comments and implementation disagree | Comments imply scan under thread lock; code snapshots then scans unlocked | Race semantics depend on revalidation but are not stated cleanly | P1 |
| Legacy `Channel` and v3 `WaitSource` coexist | exit/wait paths still carry both shapes | Lifetime and double-fire complexity under SMP | P2 |

### Migration gate

Before enabling movable userspace threads:

1. Make clone/thread publication transactional: reserve process alive/group-exit
   state, attach, then publish/register; or rollback all published identities on
   attach failure.
2. Convert exit paths to two-phase teardown:
   - under lock: mark exiting, detach minimal ownership, take snapshots;
   - outside lock: shm detach, fd close/drop, thread cap drop, futex wake;
   - publish zombie/wake after semantic state is complete.
3. Move robust-list user-memory walk outside the process payload lock.
4. Either implement per-entry signal action atomics or explicitly downgrade the
   doc and instrument the table lock.

## VM / PageBacked / VFS / filesystem backends

### Current state

- VM recipe publication is the closest module to the intended read-mostly EBR
  model: readers pin an EBR-published recipe tree; writers serialize mutation
  and publish a new tree.
- `RangeLock` serializes overlapping VM operations with a spin-locked state.
- `VmPmap` has a spin-locked resident mapping store.
- PageBacked file fetch happens outside the page-container lock in important
  paths, but the page cache itself is still one `BTreeMap` under one lock per
  `PageContainer`.
- VFS entities are zone-backed, but the current walker state is Cap-heavy.

### Gaps

| Gap | Evidence | Risk | Priority |
| --- | --- | --- | --- |
| RangeLock writer preference is not retained across production wait | `acquire_step` drops rich pending-writer carrier | Materializers can continue ahead of blocked writers under churn | P0 |
| pmap publish/teardown does heavy work under one pmap state lock | resident store reserve/commit/insert/drain under lock | Cross-hart page faults and munmap can serialize and amplify tails | P0 |
| ASID residency / shootdown target policy is not proven | teardown batches invalidations, but residency-mask proof is absent | Migration can add unnecessary remote SFENCE/IPI cost | P0 |
| PageBacked lacks per-page in-flight dedup | two cold faults may both fetch/allocate; `install_if_absent` picks a winner | SMP cold faults duplicate I/O/allocation and add tail variance | P0 |
| ext4/FAT pager paths are synchronous/coarse-locked | read backend returns `Done/Err`; pager locks wrap traversal | Cold-cache filesystem I/O is not yet the yielding path described by docs | P0 |
| PageContainer allocates anonymous pages under PC lock | PC `BTreeMap` metadata and allocation interact | Metadata lock can include allocator service time | P1 |
| User-buffer prefault loops whole range synchronously | read/write prefaults full user buffer before byte progress | Large I/O can become long non-progressing VM work | P1 |
| VFS warm walk uses `Cap<DEntry>` rather than documented `IdentRef` walk | VFS docs describe zero-refcount warm walk | Path resolution pays refcount/lock traffic and mismatches docs | P1 |
| DEntry child cache has no visible reclaim/negative-entry policy | strong `Cap` child map | Growth and invalidation policy unclear for SMP filesystem workloads | P1 |

### Migration gate

Before default-on userspace SMP:

1. Decide and implement retained writer preference or document the weaker
   production RangeLock contract with starvation metrics.
2. Add pmap counters: pages touched, shifted resident entries, shootdown calls,
   target harts, and ASID residency hits/misses.
3. Add PageBacked per-page in-flight wait slots.
4. Constrain current ext4/FAT backends to memory-image bringup or add an
   async/yielding pager path before filesystem-heavy SMP tests.
5. Move PC allocation outside the PC metadata lock where possible.

## Reactor / Scheduler / wake substrate

### Current state

- APs boot and enter the secondary reactor loop.
- The scheduler has per-hart queues, remote wake/IPI dispatch, work-steal, and
  rebalance machinery.
- OSComp userspace threads are intentionally pinned today:
  `userspace_thread_sched_meta_for` builds a single-hart affinity mask and calls
  `.pinned()`.
- Steal eligibility rejects pinned userspace tasks.

### Gaps

| Gap | Evidence | Risk | Priority |
| --- | --- | --- | --- |
| AP observe init is BSP-only | Existing research note shows AP rings had no producers | Cannot trust AP work/idle claims without AP observe | P0 |
| Remote wake/IPI attribution is not durable enough | Runtime reports placement action, but no complete source/wake-hint rows | Wake-time placement can create IPI storms without obvious blame | P0 |
| Userspace migration can expose stale slot/trap handoff assumptions | Current comments keep userspace pinned for hart-local trap state coupling | One-line `.movable()` flip is unsafe | P0 |
| `TaskSchedMeta` mixes hot and cold state behind global lock | pick/wake/stop scans clone/mutate through global meta lock | Scheduler may become a global-lock bottleneck once tasks spread | P1 |
| Waker/mailbox uses global/locked queues and O(queue) coalescing | `TaskWakeState::wake`, `TaskMailbox::post` | High fanout signal/futex/IO wakes can serialize producers | P1 |
| `WaitSource::notify` posts to mailboxes while holding subscriber lock | notify path upgrades weak mailboxes and posts under source lock | Fanout wake cost becomes source-lock service time | P1 |
| Timer queues are Vec scan/remove under lock | timer wheel removes/posts while locked | Timer-heavy workloads can show service spikes | P2 |
| Linux `getcpu` returns CPU 0/node 0 | affinity API exists but `getcpu` is stub-like | Multi-hart userspace makes this visibly wrong | P1 |

### Migration gate

The safe scheduler rollout remains:

1. Keep default pinned.
2. Initialize observe on all harts and emit final scheduler summaries.
3. Add boot/cfg gated submit spread with no steal.
4. Verify trap return, saved user context, userspace slots, and ASID residency.
5. Add wake-time placement with remote IPI counters and suppression accounting.
6. Only then enable steal for migration-safe preempted userspace tasks.

## Observe / vocabulary / lint infrastructure

### Current state

- `cargo xtask lint docs` checks links, txdoc syntax, duplicate tags, and some
  code-comment `txdoc:TXV3-*` references.
- Retired vocabulary gates exist through clippy config and step-vocabulary
  ratchets.
- Boundary ratchets exist for raw substrate/reactor references outside
  adapters.
- `#[platform_adapter]` records metadata and is enforced by lint/reporting,
  not by compiler sealing.
- Observe has bounded ring emission and derived `lock_rows` / `ds_method_rows`.
- Process/thread lock-service counters and cap-upgrade counters exist, but are
  still generic counters rather than typed joinable rows.

### Gaps

| Gap | Evidence | Risk | Priority |
| --- | --- | --- | --- |
| General observe/upgrade phase attribution is missing | OBS L5 phase tracing is deferred; cap counters are narrow | 30ms holes can be localized but not causally assigned | P0 |
| `cap.upgrade` rows lack type/caller/span dimensions | current counters only duration/attempt/retry | Cannot distinguish process owner upgrade, VFS lookup, signal route, etc. | P0 |
| Lock-service counters cannot join cleanly to lock rows | free-form counters lack lock id/guard lifetime | We can see service and phase, but not always bind phase to guard instance | P1 |
| Observe boundary lint is absent | docs say adapters must not emit observe events | Adapter scopes could accidentally become hot emitters | P1 |
| Boundary scanner is file-level coarse | file with adapter declaration can hide raw refs in same file | Ratchet is useful but not a module-span seal | P1 |
| Some invariant lints are informational or broad pattern checks | syscall ctx bridge informational, SUBJ patterns partial | Architecture promises can drift without failing CI | P1 |
| `Txv3` txdoc coverage is less strict than `docs/design` | docs lint harvests tags but does not require all section anchors | Newer active docs can be less review-stable than older docs | P2 |

### Migration gate

For performance work, the highest-value observe additions are:

1. `cap_upgrade_rows`: object kind/type, caller/span id, attempts, retries,
   duration, sampled fast-path rows.
2. `zone_lookup_rows`: registry lookup, slot-from-key, Keg lock wait/service,
   list-scan length.
3. `step_phase_rows`: observe, upgrade, reserve, commit, publish duration.
4. `lock_service_rows`: lock id, phase id, task/span id, duration.
5. Scheduler/wake rows: placement target, wake hint, source family, IPI sent,
   IPI suppressed, target idle/polling state.

## Recommended work order

### 1. Attribution first

- AP observe initialization.
- Final per-hart scheduler summary at shutdown.
- `Weak::observe` / registry / `slot_from_key` metrics.
- Typed cap upgrade and L5 step phase rows.
- Joinable lock-service rows for process/thread lifecycle.

### 2. Correctness P0s

- Transactional clone attach or rollback.
- Robust-list walk outside process payload lock.
- Two-phase process/thread teardown.
- RangeLock writer retention decision.
- PageBacked per-page in-flight dedup.
- pmap shootdown/ASID residency proof.

### 3. Critical-section shrinkage

- Process rosters and PID namespace: snapshot weak/ids under lock, upgrade/drop
  outside lock.
- PageContainer: allocate/fetch outside PC metadata lock.
- WaitSource and TimerWheel: drain targets under lock, post outside lock.
- Scheduler: split hot/cold task metadata or move queue-owned hot fields out of
  the global table lock.

### 4. Gated scheduling rollout

- Stage A: submit spread for explicitly gated userspace threads, no steal.
- Stage B: wake-time placement with IPI suppression and attribution.
- Stage C: steal only preempted, migration-safe userspace tasks.
- Stage D: fairness/group scheduling and Linux-visible CPU semantics
  (`getcpu`, affinity, sched policy).

## Bottom line

The architecture direction is sound: read-only EBR + role-shaped caps, reactor
as mechanism, scheduler as policy, and observe-backed ratchets. The
implementation is not yet ready for default userspace SMP migration. The
blocking work is mostly outside the scheduler: process/thread lifecycle locks,
substrate handle lookup, VM pmap/PageBacked cold paths, and observe attribution
for the five step phases.

Treat the current 30ms observe-upgrade evidence as a symptom of a missing
phase/lookup/lock attribution model, not as proof that the cap-retain CAS loop
is slow.
