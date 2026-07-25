# Current thread model against Linux task/thread surface

Date: 2026-06-25

## Scope

This note answers the Linux-facing questions raised in the attached thread-model
sweep against the current txKernel code and active design docs. It is not a new
architecture contract. The contract remains in:

- `docs/design/04_process-signals/PROCESS_v1.md`
- `docs/design/02_execution/THREAD_RUNTIME_v1.md`
- `docs/design/04_process-signals/SIGNAL_v1.md`
- `docs/design/02_execution/EXEC_v1.md`
- `docs/design/02_execution/SCHEDULER_v0.md`

Current implementation anchors:

- `crates/tx-subsystems/src/process/structure.rs`
- `crates/tx-subsystems/src/process/execution.rs`
- `crates/tx-subsystems/src/thread_runtime/structure.rs`
- `crates/tx-subsystems/src/thread_runtime/execution.rs`
- `crates/tx-shims/src/linux_syscall/proc.rs`
- `crates/tx-shims/src/linux_syscall/signal.rs`
- `crates/tx-shims/src/linux_syscall/signalfd.rs`

## Short answer

Tx's current model answers the Linux task/thread model as:

- `ProcessIdentity` is the TGID/process shell: stable pid, parent/child
  topology, pgrp/session membership, zombie-visible exit status, payload slot.
- `ThreadIdentity` is the TID shell: stable tid, owner-process weak link,
  thread exit status, payload slot.
- `ProcessPayload` is the current v2 resource container: address space, fd
  table, cwd, credentials, rlimits, namespace proxy, signal dispositions,
  process-directed pending signals, thread roster, and exit source.
- `ThreadPayload` is execution state: saved trap/user context, signal mask,
  thread-directed pending signals, group-pending summary cache, alternate
  signal stack descriptor, clear-child-tid pointer, robust-list head, and
  reactor mailbox/task linkage.
- Linux "a process runs" is modeled as "one of its `ThreadIdentity` members has
  a live `ThreadPayload` and reactor task." The process owns resources and
  topology; the thread owns execution.

The biggest incomplete areas are still the same three sharp committee questions:

1. Full `clone` a-la-carte sharing is not faithfully represented for every
   flag combination yet.
2. `clear_child_tid` exists and is real enough for pthread join, but it is still
   coupled to current exit paths and should be treated as a lifecycle-critical
   path, not a side note.
3. Shared vs per-thread signal state is modeled, but real-time signal queues
   and the full signal-consumption surface remain incomplete.

## 1. Thread identity, TGID, and namespaces

### Current answer

The vocabulary is:

| Linux term | Tx current home | Current behavior |
|---|---|---|
| TGID / process pid | `ProcessIdentity.pid` | Returned by `getpid`; used for parent/child, pgrp/session, wait, kill, procfs. |
| TID | `ThreadIdentity.tid` | Returned by `gettid`; registered separately in the PID-name table for `tkill`/`tgkill` and `/proc/<pid>/task`. |
| Leader thread | Initial thread whose tid equals process pid | `bootstrap_init_process` and `step_fork` create a leader with `Tid(pid.0)`. |
| PID namespace | Current `process::numbers` registry plus `NsProxy` hooks | A flat/root namespace is mostly current; namespace-aware extension exists in design and selected net/user/mount paths, but nested PID namespace semantics are not done. |

This is a defensible answer to the "pid vs tid vs tgid" question: Tx uses
separate role-shaped identities but shares the numeric namespace, matching the
Linux convention that the leader TID has the same numeric value as the process
TGID.

### Gap

Nested PID namespaces are not complete. The active design reserves the
`NsProxy`/`PidName` direction, and implementation has namespace machinery for
user/net/mount-adjacent paths, but current PID rendering and lookup should be
described as flat/root namespace behavior unless a specific namespace path says
otherwise.

## 2. `set_tid_address`, `CLONE_CHILD_CLEARTID`, and pthread join

### Current answer

This is placed correctly on `ThreadPayload`, not on the process:

- `ThreadPayload.clear_child_tid` stores the userspace address.
- `clone(CLONE_CHILD_CLEARTID)` records the child TID address.
- `set_tid_address` updates the current thread's clear-child-tid pointer.
- `step_thread_exit` snapshots the pointer before dropping payload, writes zero
  to the userspace word, and calls the futex lifecycle wake path.
- `step_exit_group` also calls the thread-exit userspace cleanup path before
  zombifying drained sibling threads.

So pthread join should be explained as a thread-exit plus futex protocol:
userspace waits on the TID word, and the kernel clears and wakes it when the
thread semantically exits. This is not optional glue; it is the real join
mechanism for musl/glibc.

### Gap

This path is best-effort under address-space teardown, matching the current
code comments. Any future rewrite of `step_exit_group`, process payload drop,
or address-space teardown must preserve "snapshot before payload drop" and
"clear then futex wake" ordering.

## 3. TLS and initial user context

### Current answer

TLS is modeled in the trap/user context, not as a separate kernel object:

- `clone(CLONE_SETTLS)` passes a TLS argument.
- `seed_child_leader_context` writes the architecture-specific thread pointer
  register: RV64 `tp` / x4, LoongArch64 r2.
- The same context seeding also sets child return value `a0 = 0`, optional child
  stack pointer, and preserves/updates the post-syscall PC according to the trap
  handoff discipline.

This answers the RISC-V TLS question: the kernel's durable thread object does
not need a special "TLS object"; the ABI-visible TLS pointer is part of saved
user register state.

## 4. Credentials and per-task attributes

### Current answer

Credentials are currently process-level in Tx:

- `ProcessPayload` stores a `Cred`.
- `SyscallCtx` captures a credential snapshot at syscall entry.
- Credential checks are routed through `cred::checks::*` for the wired surfaces.
- `execve` uses the caller's walker credential for path authorization and can
  apply exec credential transitions where implemented.

This is simpler than Linux's task-struct credential pointer model. For current
Tx, the honest statement is: all threads in a process execute under the
process's credential snapshot; there is not a separate per-thread credential
home.

### Current per-thread/process misc attributes

| Attribute family | Current home | Status |
|---|---|---|
| `comm`/cmdline | `ProcessPayload` accessors and procfs projections | Present enough for current procfs/status needs. |
| `personality` | `ProcessPayload` | Query/set implemented for known flags. |
| `prctl` timerslack / pdeathsig / child-subreaper maps | syscall-side maps in `proc.rs` | Compatibility storage exists, not a complete Linux task attribute model. |
| robust futex list | `ThreadPayload.robust_list_head` / `robust_list_len` | Exit walk is implemented best-effort. |

### Gap

Linux's per-task attribute surface is broader than the current model:
`no_new_privs`, dumpability, seccomp/restriction stack, full child-subreaper
semantics, and exact per-thread credential semantics are not fully placed in
the current implementation.

## 5. Scheduling attributes and CPU accounting

### Current answer

Scheduling is intentionally split:

- Thread/process code owns the semantic subject.
- Reactor owns task mechanics.
- Scheduler policy owns per-task scheduling metadata and CPU-time accounting.

Implementation currently exposes compatibility syscall surfaces for scheduler
attributes, including `sched_getaffinity`, `sched_setaffinity`,
`sched_getattr`, `sched_setattr`, `sched_setscheduler`, `getpriority`, and
`setpriority`. There is also real reactor affinity plumbing for thread tasks in
the kernel reactor-submit path.

### Gap

The current scheduler surface is not full Linux scheduling:

- Dynamic affinity and policy updates are only partially semantic.
- CPU accounting exposed through `wait4` rusage is still zero-filled.
- Per-thread vs per-process accounting such as `CLOCK_THREAD_CPUTIME_ID`,
  `getrusage(RUSAGE_THREAD)`, and complete `times(2)` semantics should not be
  claimed as finished unless verified separately.

## 6. Clone flag surface

### Current answer

Tx now has both process and thread clone paths:

| Clone shape | Current behavior |
|---|---|
| Bare fork / `SIGCHLD` clone | Creates new `ProcessIdentity`, new payload, leader `ThreadIdentity`, forked or shared address-space depending on `CLONE_VM`. |
| `CLONE_THREAD` | Creates a new `ThreadIdentity` inside the current process; no new process identity. |
| `CLONE_SETTLS` | Seeds the child thread pointer register. |
| `CLONE_CHILD_CLEARTID` | Stores clear-child-tid pointer on child payload. |
| `CLONE_CHILD_SETTID` / `CLONE_PARENT_SETTID` | Writes child TID to userspace buffers where requested. |
| `CLONE_VFORK` | Parent waits until child execs/exits through the vfork-done path. |
| `CLONE_NEWNET` | Fresh net namespace for child in the implemented path. |
| `CLONE_NEWNS` | Accepted for LTP namespace harnesses; full mount namespace isolation is deferred. |
| `CLONE_NEWIPC` | Passed into fork options and nsproxy clone path. |

The model-level answer is:

- `CLONE_THREAD` means "same process payload, new thread payload."
- non-`CLONE_THREAD` means "new process identity and leader thread."
- `CLONE_VM` can share the address-space cap across process identities.
- `CLONE_SIGHAND` can share the signal-action table by `Arc`.

### Critical gap

This is where the current model is weakest against Linux's full flag soup.
Linux allows independent sharing axes: VM, files, fs context, signal handlers,
thread group, parentage, vfork, namespaces, pidfd, sysvsem, io context, and
more. Tx's v2 implementation still has a flat `ProcessPayload`; the v3 design
target introduces `Frame { Shared<T> }` to represent per-resource share/copy.

Important current caveat:

- On the `CLONE_THREAD` path, sharing files/fs/sighand/vm is mostly inherent
  because the new thread stays in the same `ProcessPayload`.
- On the non-thread process path, `CLONE_VM` and `CLONE_SIGHAND` have meaningful
  sharing behavior, but `CLONE_FILES` and `CLONE_FS` are accepted more broadly
  than the implementation can faithfully model as shared mutable resources.

So the honest answer to "How do you represent `CLONE_VM` without
`CLONE_THREAD`?" is: Tx can create a separate `ProcessIdentity` with a shared
address-space cap. The honest answer to "How do you represent every independent
Linux clone sharing combination?" is: not fully until the `Frame/Shared<T>` v3
refactor lands.

## 7. Exit, zombies, wait, and reparenting

### Current answer

Exit is split exactly where the model wants it:

- `step_thread_exit` kills one thread: records thread status, drops
  `ThreadPayload`, detaches from `ProcessPayload.threads`, handles
  clear-child-tid and robust futex cleanup, unregisters non-leader TID.
- If the exiting thread was the last thread, it calls `step_process_exit`.
- `step_exit_group` kills the whole thread group: drains threads, does
  userspace exit cleanup, drains fds/shm/sem undo, drops process payload, records
  process exit status.
- `ProcessIdentity` persists as the zombie shell until the parent reaps it.
- `wait4` maps pid selectors to `WaitTarget`, supports `WNOHANG`, blocks on the
  parent's exit source when no zombie is ready, writes POSIX wait-status words,
  and zero-fills rusage for now.
- Reparenting to init exists. Adopted zombie children can be auto-reaped in the
  current implementation.

### Gap

The full Linux wait surface is not complete:

- `WUNTRACED` and `WCONTINUED` are acknowledged but not fully semantic.
- `__WALL`, `__WCLONE`, ptrace wait, and detailed stop/continue status are not
  complete.
- Leader-exit-with-survivors is specified in the design but should not be
  oversold as a mature implementation path unless a focused test proves it.
- Full `PR_SET_CHILD_SUBREAPER` reparenting semantics are not complete, despite
  some compatibility storage.

## 8. Signal model

### Current answer

Tx has the important shared/private split:

| Linux signal state | Tx current home |
|---|---|
| per-thread blocked mask | `ThreadPayload.signal_mask` |
| per-thread pending signals | `ThreadPayload.thread_pending` |
| process-directed pending signals | `ProcessPayload.group_pending` |
| signal dispositions | `ProcessPayload.frame.sig_actions` / `SigActionTable` |
| signal handler saved context | `ThreadPayload.saved_signal_context` |
| saved mask for `rt_sigreturn` | `ThreadPayload.saved_signal_mask` |
| alt stack registration | `ThreadPayload.alt_stack` |
| synchronous fd consumption | signalfd subsystem and signalfd read path |

`tkill` resolves a TID through the PID-name registry and routes directly to a
thread. `tgkill` currently requires `tgid == caller pid` and then resolves the
target thread within the caller's process. `rt_sigsuspend`, `sigaltstack`,
`sigtimedwait`, signalfd, signal mask updates, signal wake hints, and
`rt_sigreturn` have real syscall-side behavior.

This answers the shared/private signal question well at the model level:
dispositions and process-directed pending state live with the process; blocked
mask, thread-directed pending state, and actual delivery context live with the
thread.

### Gap

Signal completeness is still partial:

- Real-time signals are not represented as full per-occurrence queued `siginfo`
  objects; much current state is bitset-shaped.
- `rt_sigqueueinfo` is still `ENOSYS`.
- `sigaltstack` records the alternate stack, but `SA_ONSTACK` delivery is
  explicitly deferred in the syscall comment.
- Cross-process `tgkill` is not supported; current `tgkill` is own-thread-group
  only.
- LA64 has a musl `SIGCANCEL` fail-fast workaround, so cancellation signal
  delivery is not fully ABI-complete there.

## 9. Exec and threads

### Current answer

`execve` is a multi-phase operation that reads userspace path/argv/envp,
checks credentials, invokes the exec script, swaps the process image, closes
CLOEXEC fds, resets signal state as implemented, and returns through the fresh
user context. `vfork` completion is notified on successful exec.

### Gap

Linux's brutal case remains the hard one: exec from a non-leader thread in a
multi-threaded process destroys siblings and makes the caller become the leader.
The design names this as deferred. Current code has `collapse_threads_for_exec`
support and exit-group machinery, but this should be described as incomplete
unless a focused non-leader exec test is green.

## 10. Committee-facing answer

If asked "what is your thread model?" the concise answer is:

> Tx separates addressability from operation. A process has a
> zombie-stable `ProcessIdentity` and a live-only `ProcessPayload`; a thread has
> a zombie-stable `ThreadIdentity` and a live-only `ThreadPayload`. Linux TGID
> maps to `ProcessIdentity.pid`; Linux TID maps to `ThreadIdentity.tid`.
> Threads are the scheduling, trap, signal-delivery, TLS, futex-exit, and
> execution-context units. Processes are resource, topology, policy, pgrp,
> session, parent/child, group-signal, and wait/zombie units.

If asked "where does pthread join live?" answer:

> In `ThreadPayload.clear_child_tid` plus `step_thread_exit`: clone or
> `set_tid_address` registers the userspace TID word; thread exit clears it and
> futex-wakes waiters before/while tearing down thread payload.

If asked "where does Linux's `clone` flag soup stress the model?" answer:

> At the resource-sharing boundary. Tx can already distinguish new process vs
> new thread and can share address space and signal dispositions in some paths,
> but full Linux a-la-carte sharing needs the v3 `Frame { Shared<T> }` resource
> wrapper so VM, fd table, fs context, and signal dispositions can each be
> independently shared or copied.

If asked "what is still unplaced?" answer:

> Fully faithful clone-resource sharing, nested PID namespaces, complete
> real-time signal queues, cross-process `tgkill`, non-leader multithreaded
> exec, complete wait stop/continue/ptrace semantics, exact per-task Linux
> credentials/attributes, and complete user-visible CPU accounting.

## Suggested next doc/code actions

1. Promote the `Frame { Shared<T> }` v3 clone-sharing design from deferred text
   into an implementation plan, because it is the central answer to Linux's
   a-la-carte clone combinations.
2. Add a small focused test matrix for `CLONE_VM` without `CLONE_THREAD`,
   `CLONE_FILES` without `CLONE_THREAD`, and `CLONE_SIGHAND` combinations, so
   the current accepted-but-not-faithful cases are explicit.
3. Add a pthread lifecycle note that treats `clear_child_tid`, robust-list
   cleanup, cancellation signal delivery, and join wake ordering as one
   lifecycle contract.
4. Split signal follow-up into bitset-compatible signals vs real-time queued
   `siginfo` delivery, because the current model answers the former much better
   than the latter.
