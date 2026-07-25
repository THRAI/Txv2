# 2026-07-10 process subsystem data-structure fanout review

## Scope

Read-only fanout review of the current process/thread runtime implementation,
focused on data structures, lock boundaries, and likely service-time tails.
No source code was changed.

Primary code anchors:

- `crates/tx-subsystems/src/process/{structure.rs,topology.rs,numbers.rs,execution.rs}`
- `crates/tx-subsystems/src/thread_runtime/{structure.rs,execution.rs}`
- `crates/tx-subsystems/src/signal/mod.rs`
- `crates/tx-subsystems/src/futex/mod.rs`
- `crates/tx-subsystems/src/process/nsproxy.rs`

## Current shape

- The active docs still describe the intended process topology as
  intrusive/DLL-shaped containers, but the current implementation uses
  conservative `ProcessSpinMutex<Vec<_>>` rosters for process children,
  process threads, process-group members, and session members.
- PID/TID/PGID/SID resolution is a single global
  `ProcessSpinMutex<BTreeMap<(u64, PidNameKind), PidName>>`. `NsProxy` carries
  `pid_ns` / `pid_for_children`, but those are still stubs for process lookup;
  most pid/tid lookup paths go through the global `process::numbers` table.
- `ProcessPayload` is flat rather than one monolithic inner lock: aspace, cred,
  nsproxy, and net namespace are `AtomicSlot`s; fd state is
  `SpinMutex<BTreeMap<u32, Cap<OpenFile>>>`; CLOEXEC is
  `SpinMutex<BTreeSet<u32>>`; sem undo, cwd, cmdline, exe, comm, group-exit,
  and vfork waiter each have their own small locks. The outer
  `ProcessIdentity.payload` slot is still important because several paths keep
  that guard while invoking heavier payload or cross-subsystem work.

## Main inefficiency candidates

1. `exit_group` / last-thread `process_exit` payload-slot guard work.
   `step_exit_group_with_posts` and `step_process_exit_inner` hold
   `process.payload.lock()` while detaching shm, draining fd state, closing
   sockets, draining thread rosters, zombifying threads, unregistering TIDs, and
   dropping drained state. This is the strongest current service-time-tail
   suspect.

2. Vec rosters and clone-under-lock.
   `ProcessChildren`, `ProcessThreads`, `ProcessGroupMembers`, and
   `SessionMembers` use `Vec` behind `ProcessSpinMutex`. `detach`, `retain`,
   `find_by_tid`, `snapshot`, and `snapshot_live` are linear; snapshots clone or
   upgrade entries while holding the roster lock. This affects wait/reap,
   thread exit, signal fanout, setpgid/setsid, and session-leader cascades.

3. Global PID namespace.
   Allocation itself is cheap (`AtomicU32::fetch_add(Relaxed)`), but register,
   resolve, enumerate, and unregister share one global `BTreeMap` lock.
   `unregister_pid_number` does a full-table `retain`, and procfs-style
   enumeration holds the namespace lock while walking the map.

4. Fork payload snapshot and fd-table clone.
   `step_fork_with_options` snapshots parent payload fields under the payload
   slot guard. `clone_fds_for_fork` clones the whole fd `BTreeMap` and accounts
   inherited pipe fd refs. `AddressSpace::fork_aspace` is outside that guard,
   which is good, but fd/CLOEXEC cloning remains an O(fd) fork cost.

5. Thread-exit robust-list cleanup.
   Ordinary `step_thread_exit` snapshots clear-child-tid / robust-list fields,
   zombifies the thread, detaches it from the parent thread roster, and then
   does clear-child-tid and robust-list work. `walk_robust_list` upgrades the
   owner process, takes the process payload slot, reads user memory, walks up to
   2048 robust-list entries, writes futex words, and wakes futex waiters.
   This is a concrete "user memory access plus bounded long walk" tail.

6. Futex exact-waiter table.
   The exact waiter path uses a global
   `SpinMutex<Option<BTreeMap<FutexKey, FutexEntry>>>`, with per-entry
   `VecDeque<FutexWaiter>`. The wait path re-reads user memory under the table
   lock to close the lost-wake window; wake traverses waiters under the same
   table lock before notifying outside it.

7. Signal fanout.
   Process-group signal delivery snapshots all pgrp members, then delivers to
   each process. Catchable group pending delivery may snapshot a process's
   thread roster and synchronize each thread's summary. This is naturally
   O(processes * threads) for group fanout.

## Non-hot or lower-priority observations

- `allocate_pid()` / `allocate_tid()` are not the main cost center.
- `getpid()` / `gettid()` read current identity fields and do not hit the
  global namespace table.
- `live_thread_count()` is an `AtomicU32` read.
- `wait4` user-memory writeback occurs after reap; the low-efficiency part is
  children snapshot/reap, not user-copy under the children lock.

## Suggested follow-up order

1. Split `exit_group` / `process_exit` so the payload slot guard only detaches
   and snapshots minimal state; move shm detach, socket close, fd drop, thread
   zombify, and large drops outside or into bounded phases.
2. Fix or explicitly reconcile `step_thread_exit` clear-child-tid and
   robust-list ordering against the active thread-runtime contract, then avoid
   holding the process payload slot across robust-list user-memory walking.
3. Replace or supplement Vec rosters with keyed/intrusive structures where the
   implementation already depends on detach/reap/fanout efficiency.
4. Shard or key-scope the futex exact waiter table and avoid table-lock
   user-memory reads if a two-phase prepare/commit shape can preserve the
   lost-wake invariant.
5. Measure before broader PID namespace work: namespace allocation is cheap,
   but global lookup/unregister/procfs enumeration may become visible in
   process-heavy or procfs-heavy tests.

## Verification

No code was changed. Verification was source inspection only through read-only
`rg`, `sed`, and `nl` queries plus fanout reader reports.

## Implementation follow-up

Later on 2026-07-10, the first non-RCU follow-up was implemented in
`crates/tx-subsystems/src/process/execution.rs` and
`crates/tx-subsystems/src/thread_runtime/execution.rs`: `exit_group` and
last-thread `process_exit` now release the outer process payload slot guard
before shm detach, socket close, thread zombification, TID unregister, and
large drops; robust-list exit cleanup now drops that guard before user-memory
walking. The RCU-facing work remains deliberately out of scope: PID namespace
lookup, Vec roster replacement, futex waiter-table sharding, and publication
vocabulary are still deferred.
