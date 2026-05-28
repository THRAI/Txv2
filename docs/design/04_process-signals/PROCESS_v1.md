# Process — v1

<!-- txdoc:04-PROCESS-SIGNALS-PROCESS-V1 -->

## Status

<!-- txdoc:PROCESS-STATUS-1 -->

Draft v1.2.

This document specifies the **process subsystem**: ProcessIdentity/ProcessPayload/ProcessGroup/Session entities, their bindings and materializations, the step catalog for fork/clone/exec/exit/wait/setpgid/setsid/kill, and the cross-cutting lifecycle patterns that tie them together.

It is the larger semantic counterpart to `THREAD_RUNTIME_v1`. Where thread-runtime specifies what a thread is and how one runs, PROCESS_v1 specifies what a process is and how processes relate to each other. Together they cover POSIX process and thread semantics.

Companion documents:

- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) — entity model, Cap/PayloadCap/Weak, SENTINEL_DEAD.
- [`EBR_ZONE_INTERFACE_v1.md`](../01_substrate/EBR_ZONE_INTERFACE_v1.md) — current zone/reference interface spelling.
- [`MUTATION_COMPOSITIONS_v1.md`](../01_substrate/MUTATION_COMPOSITIONS_v1.md) — `structural_move`, `structural_withdraw`, `structural_publish`.
- [`BITMAP_RESERVATION_v1.md`](../01_substrate/BITMAP_RESERVATION_v1.md) — `AtomicBitmap` reservation primitives.
- [`THREAD_RUNTIME_v1.md`](../02_execution/THREAD_RUNTIME_v1.md) — thread semantics, signal delivery, stop state.
- [`SIGNAL_v1.md`](SIGNAL_v1.md) — POSIX signal compatibility shim; `deliver_posix_signal` entry point; sig_actions routing; handler delivery machinery.
- [`REACTOR_v0.md`](../02_execution/REACTOR_v0.md) — reactor contract.
- [`SCHEDULER_v0.md`](../02_execution/SCHEDULER_v0.md) — scheduler policy (for nice/setpriority/setscheduler deferrals).
- [`NAMESPACE_VIEW_v1.md`](../00_meta-framework/NAMESPACE_VIEW_v1.md) — nsproxy, pid-name resolution, namespace-view commits, and projected RNodes.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — ARCH-5 (publication rule), BIF-* (bifurcation), STEP-* (step discipline), SCRIPT-* (script rules).
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — authoritative bindings, derived materializations, and publication rule.
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §3 (five-phase discipline), §4 (substrate primitive families).
- [`cred_service_v_1_draft (2).md`](<../02_execution/cred_service_v_1_draft (2).md>) and [`rlimit_service_v_1_draft (1).md`](<../02_execution/rlimit_service_v_1_draft (1).md>) — process policy services.

### What this document pins

<!-- txdoc:PROCESS-WHAT-THIS-DOCUMENT-PINS-1 -->

- Entity structure for ProcessIdentity, ProcessPayload, ProcessGroup, Session.
- Bindings and materialized DLL containers for process topology.
- Frame placement (on ProcessPayload) and the Shared<T> sharing model for vm/fd_table/sig_actions/fs_context.
- ProcessPolicy structure (cred + rlimits; signal_mask is not here — it's per-thread per THREAD_RUNTIME).
- Step catalog: fork, clone, execve (static, leader-only in v1), exit, exit_group, process_exit, wait family, setpgid, setsid, getpid family, kill/tkill/tgkill, set_tid_address.
- Cross-cutting patterns: reparenting, orphan pgroup SIGHUP, session leader death, group exit cascade, zombie pgroup membership.
- POSIX alignment: committed vs deferred.
- Cross-doc corrections applied to credential and rlimit service drafts.

### Zone-derived type policy

<!-- txdoc:PROCESS-ZONE-DERIVED-TYPE-POLICY-1 -->

PROCESS uses policy-based zones for every reclaimable semantic entity, but its
public surface stays role-shaped:

| Process declaration | Zone-derived public type | Reclamation role |
|---|---|---|
| `ProcessIdentity` | `Cap<ProcessIdentity>`, `Weak<ProcessIdentity>`, `IdentRef<'g, ProcessIdentity>` | zombie-stable identity retention and guarded lookup |
| `ProcessPayload` | `PayloadCap<ProcessPayload>` reached through `ProcessIdentity.payload` | operational process state; dropped at exit |
| `ThreadIdentity` references | `Cap<ThreadIdentity>` in process thread containers | addressability for thread-group membership and wait surfaces |
| `ProcessGroup` / `Session` | `Cap<ProcessGroup>`, `Cap<Session>` | identity-only topology entities |
| PID/name table rows | stored `Cap<...>` for addressability or `Weak<...>` only for stale hints | obligation derives the evidence |
| Frame slots (`vm`, `fd_table`, `sig_actions`, `fs_context`) | role-specific shared slot holding identity-retaining evidence | not raw `Zone<T, Policy>` in process code |

Raw zone policy names are confined to entity-zone declarations. Fork, clone,
exec, exit, wait, setpgid, and signal-routing steps consume witnesses and caps;
they do not select `RcPolicy` or `EbrPolicy` at call sites.

### What this document defers

<!-- txdoc:PROCESS-WHAT-THIS-DOCUMENT-DEFERS-1 -->

- **Non-leader execve.** Tid-rename semantics, deferred to Phase 2. Phase 1 requires exec from the thread-group leader.
- **Nested pid namespaces.** Single root namespace in v1. Multi-level `PidName` / `PidStruct` accommodates extension; semantics for CLONE_NEWPID et al. deferred to `NAMESPACE_VIEW_v1`.
- **CHILD_SUBREAPER.** Linux 3.4 feature, out of 2.6 parity scope. Orphans reparent to pid 1.
- **Full ptrace.** Three intercepts sketched; observation subsystem is separate.
- **Scheduler policy.** REACTOR_v0 non-goal.
- **Full signal semantics.** Specified in [`SIGNAL_v1.md`](./SIGNAL_v1.md); PROCESS_v1 provides the state placement and producer integration points (§7.6, §8.2, §17.2 in SIGNAL_v1 terms).

---

## 1. Position in the architecture

<!-- txdoc:PROCESS-POSITION-ARCHITECTURE-1 -->

A process is:

- A **unit of resource ownership.** Owns an address space, fd table, signal dispositions, working directory, credentials, resource limits.
- A **unit of policy.** Carries the credentials and rlimits that authorize operations.
- A **member of the process tree.** Has a parent, may have children, belongs to a process group, which belongs to a session.
- A **container for threads.** One or more threads execute within the process's resources.

It is **not**:

- A unit of execution (that's thread).
- A pure namespace entry (it has operational state too).
- Single-typed: process splits into ProcessIdentity (structural, zombie-survivable) and ProcessPayload (operational, dropped at exit).

### 1.1 Relationship to other subsystems

<!-- txdoc:PROCESS-RELATIONSHIP-OTHER-SUBSYSTEMS-1 -->

- **Thread-runtime** (`THREAD_RUNTIME_v1`) — specifies thread semantics. PROCESS owns process-level aggregation of threads (the threads DLL on ProcessPayload) and operations that affect the thread group (fork, exec, exit_group).
- **Reactor** (`REACTOR_v0`) — the mechanism layer. Each thread is a reactor task; PROCESS does not directly interact with the reactor.
- **VM** (`VM_v1_2`) — AddressSpace operations. Process provides `Frame.vm: Shared<AddressSpace>` and delegates AS mutation to VM.
- **VFS** — fd table, path resolution, working directory. Process provides `Frame.fd_table: Shared<FdTable>` and `Frame.fs_context: Shared<FsContext>`.
- **Signal** (future spec) — extends THREAD_RUNTIME's placement. Process owns `ProcessPayload.group_pending` and the shared `sig_actions` table in Frame.
- **TTY** (future spec) — consumes process topology for foreground pgroup and SIGHUP cascading on session leader death.
- **Cred** (v1.2) and **Rlim** (v1.1) — services. ProcessPolicy on ProcessPayload holds `Binding<Credential>` and `Binding<RLimitBag>` (execve may swap them atomically in Phase 2).

### 1.2 Layering

<!-- txdoc:PROCESS-LAYERING-1 -->

```
PROCESS_v1 (this document)            ← process tree, group/session, fork/exec/wait
    ↓ uses
THREAD_RUNTIME_v1                     ← thread execution, signal delivery
    ↓ uses
REACTOR_v0                            ← task mechanism
```

And across the stack:

```
PROCESS_v1 ←→ VM_v1_2                 ← AS via Frame.vm
           ←→ VFS                     ← fd table, fs context
           ←→ Cred, Rlim              ← ProcessPolicy
           ←→ Signal                  ← group_pending, sig_actions
           ←→ TTY                     ← pgroup/session consumer
```

PROCESS owns its entities and their relationships. Other subsystems provide the types that Frame holds (AddressSpace, FdTable, etc.).

### 1.3 The thread/process provision

<!-- txdoc:PROCESS-THE-THREAD-PROCESS-PROVISION-1 -->

Execution, scheduling, signal delivery, synchronous-fault attribution, stop/continue compliance, and termination are all **thread-level**. The process is the resource container (Frame), the pid-namespace identity, the parent/child node, the pgroup/session member, and the zombie persistence shell.

**What the thread provides to the process:**

- **Evaluation.** A process with zero running threads is a zombie — no forward progress occurs without threads. All userspace execution is thread execution.
- **Scheduling presence.** Schedulable units are threads. The reactor task and scheduler metadata (class, priority, affinity, remaining budget) are per-thread. "The process runs" is shorthand for "one of its threads is currently on-hart."
- **Signal-delivery presence.** AST runs per-thread. `signal_mask`, `signal_summary`, and `thread_pending` are per-thread. A process-directed signal is ultimately delivered to *a* specific thread.
- **Fault attribution.** Only a thread faults. SIGSEGV's target is the specific faulting thread, not "the process."
- **Stop/continue compliance.** "The process is stopped" means every thread has transitioned to `Stopped`. Process-stopped is emergent from all-threads-stopped; no separate process-level stop flag exists.
- **Termination compliance.** "The process exits" means every thread has run `step_thread_exit` and the last triggered `step_process_exit`. `exit_status` is populated by the last thread.
- **Wait observability.** `thread_exit_source` fires per thread (pthread_join, clear_child_tid); `exit_source` fires when the last-thread-exit triggers process exit (waitpid, pidfd). Both wires fire because threads fire them.

**What the process provides to threads:**

- **Resource home.** Shared Frame: fd_table, address_space, sig_actions, fs_context.
- **Identity scope.** Thread tids are allocated in the process's pid_namespace.
- **Signal-routing infrastructure.** Process-directed signals land in `group_pending`; threads consult this alongside their own `thread_pending`.
- **Policy bundle.** cred, rlimits. Threads execute under the process's credentials and resource limits.
- **Pgroup/session membership.** Process-level relations; threads don't carry these.
- **Parent/child topology.** Process-level. SIGCHLD routes to the parent *process*, not a parent thread.
- **Zombie shell.** After all threads exit, the process persists to carry exit_status until the parent reaps via waitpid.

**On Gewalt fan-out:** Process-level Gewalt operations (`kill(pid, ...)`, `exit_group`, `group_stop`, `group_continue`) are **fan-out compositions over thread-level primitives**. The process entity provides the iteration target (the threads DLL) and coordination state (GroupExit, group_pending); each thread provides the per-unit semantic (its `signal_summary`, `stop_state`, `thread_pending`, reactor waker). The process is the **dispatcher** for these ops, not the semantic unit.

This framing is consistent with SIGNAL_v1 §12 (`deliver_posix_signal` dispatches to thread-level primitives after routing decisions) and with the Gewalt/event factoring in SIGNAL_v1 §1 (where Gewalt is metalevel on the evaluator, and each thread is an evaluator). PROCESS owns the container; the thread owns the execution.

---

## 2. Entities

<!-- txdoc:PROCESS-ENTITIES-1 -->

Four entity types, with the Identity/Payload split applied where meaningful.

### 2.1 ProcessIdentity

<!-- txdoc:PROCESS-PROCESSIDENTITY-1 -->

```rust
pub struct ProcessIdentity {
    // Naming
    pub pid_name: PidNameSnapshot,             // non-retaining namespace number snapshot

    // Upward authoritative bindings
    pub parent: Binding<ProcessIdentity>,
    pub pgrp: Binding<ProcessGroup>,

    // Materialization: children whose parent binding names this process
    pub children: DllContainer<ProcessIdentity>,

    // DLL linkages (intrusive)
    pub parent_chain: DllNode<ProcessIdentity>,    // linked in parent.children
    pub pgrp_chain: DllNode<ProcessIdentity>,      // linked in pgrp.members

    // Payload attachment
    pub payload: PayloadBinding<ProcessPayload>,

    // Post-exit state (readable on zombie)
    pub exit_status: AtomicOption<ExitStatus>,
}
```

**Structural:**
- `pid_name` — non-retaining snapshot of the process pid number(s). The authoritative binding lives in `PidNamespace.numbers -> PidName`; the snapshot exists for fast rendering and tracing.
- `parent` — authoritative upward binding. Changes only via reparenting (on parent exit or via PR_SET_CHILD_SUBREAPER in Phase 2+).
- `pgrp` — authoritative upward binding. Changes via setpgid (or inheritance at fork).
- `children` — downward materialization. Walkers re-validate entries against their `parent` binding per BINDING_v1 §6.2.
- `parent_chain`, `pgrp_chain` — intrusive DLL nodes participating in the parent's `children` and the pgroup's `members` containers.

**Zombie behavior:**
- Identity persists after payload drop.
- `exit_status` is set at exit (before payload drop).
- Readable by waitpid without payload access.

**Projections:**
- structural — some Cap<ProcessIdentity> extant.
- addressability-for-waitid — binding present in parent.children.
- addressability-for-kill — structural AND `payload.load().is_some()`.

### 2.2 ProcessPayload

<!-- txdoc:PROCESS-PROCESSPAYLOAD-1 -->

```rust
pub struct ProcessPayload {
    // Thread roster
    pub threads: DllContainer<ThreadIdentity>,
    pub thread_count: AtomicU32,               // len(threads), maintained atomically

    // Frame and namespace context (per this document §3 and NAMESPACE_VIEW_v1)
    pub frame: Frame,
    pub nsproxy: Cap<NsProxy>,

    // Policy (per credential and rlimit service specs)
    pub policy: ProcessPolicy,

    // Process-directed signal state
    pub group_pending: PendingSignalQueue,     // per THREAD_RUNTIME §5.1

    // Group-exit coordination
    pub group_exit: GroupExit,                 // see §5

    // Leader-exit-with-survivors support
    pub leader_exit_status: AtomicOption<ExitStatus>,

    // Identity back-pointer
    pub identity: Weak<ProcessIdentity>,
}
```

- **Thread roster.** `threads` is a DLL of ThreadIdentity entries. `thread_count` is a denormalized atomic for cheap "is there more than one thread" checks (used by fork, exec, exit_group); maintained atomically with DLL insert/remove.
- **Frame.** Contains the Shared<T> slots (vm, fd_table, sig_actions, fs_context) and inline scalars (cwd, root, umask). See §3.
- **Namespace context.** `nsproxy` is an immutable bundle of namespace references used by syscall resolve/render helpers. It owns no process topology. The bundle may carry a `user_ns` mirror for view helpers, but Linux-compatible capability/uid/gid authority is credential-owned (`Cred` / `SubjectAuthority`) and must stay consistent with any mirror at publication boundaries; PROCESS stores the active bundle but does not own user-namespace maps or capability semantics.
- **Policy.** See §4.
- **Group-directed pending.** Process-directed signals queue here. Thread-directed signals queue on each ThreadPayload's `thread_pending`.
- **Group exit.** Coordinates exit_group and non-initiator exec's thread-group collapse. See §5.
- **Leader exit status.** Set when the leader thread exits separately (leader-exit-with-survivors); used at process_exit to determine process-visible exit status. See §6.
- **Identity back-pointer.** Weak to avoid retention cycle.

**Projections:**
- structural — some PayloadCap<ProcessPayload> extant.
- payload — same as structural; ProcessPayload has no sub-projections.

#### 2.2.1 v2 amendment — flat ProcessPayload as ratified shape
<!-- txdoc:PROCESS-PROCESSPAYLOAD-V2-AMENDMENT-1 -->

**Status:** v1 spec preserved above for reference. The implementation
shipped a flat shape across five slices and the v2 amendment ratifies
that flat shape. `Frame` / `Shared<T>` / `ProcessPolicy` / `nsproxy`
are deferred to **v3**; see "Deferred to v3" below.

**Why a v2 amendment, not a code refactor.** Five slices built on the
flat shape, in this order:

- trio Phase 2a (commit `abefad4`): added `fds:
  SpinMutex<[Option<Cap<OpenFile>>; FD_TABLE_SIZE]>` and the syscall
  dispatcher's fd-resolving arms (`write`, `exit`, `exit_group`,
  `getpid`).
- trio Phase 2b (commit `119602b`): added `brk_base` /
  `current_brk` as `AtomicU64` for the `brk(2)` syscall; added the
  `read` / `rt_sigprocmask` / `rt_sigaction` arms.
- pre-ELF Wave 3 (commit `abc9fb8`): added the saved-trap-frame +
  thread-future state on `ThreadPayload` (touched the
  `ProcessPayload` shape only indirectly through the threads
  vector).
- ELF loader Wave 1 (commit `c67c970`): flipped `aspace` to
  `AtomicSlot<Cap<AddressSpace>>` so exec Phase 6's atomic store
  could swap the address space without a sibling-thread quiesce.
- fork/clone/wait4 Wave 1 (commit `e697631`): added `exit_source:
  Channel` and `exit_source_id: u64` so `sys_wait4` could
  park on the carrier without holding the parent's `Cap`.
- DAC + setuid Wave 2 (commit `fcd9639`): added `cred:
  SpinMutex<Cred>` (a per-process credential snapshot) and the
  twelve cred-aware syscall arms.
- DAC + setuid Wave 4 (commit `b9ae7a0`): added `fd_cloexec:
  AtomicU32` for `step_close_cloexec_fds` to consult during exec
  Phase 7.
- fd-ops Wave 1 (2026-05-07): flipped `fds` from
  `[Option<Cap<OpenFile>>; FD_TABLE_SIZE]` to
  `BTreeMap<u32, Cap<OpenFile>>` and `fd_cloexec` from `AtomicU32`
  to `BTreeSet<u32>`. Closes the `FD_TABLE_SIZE = 8` ceiling and
  the fd-31 CLOEXEC ceiling — sparse fds (e.g. shells doing
  `>&100`-style redirection) are now first-class. New accessor
  surface (`allocate_fd`, `allocate_fd_at_least`, `next_fd_above`,
  `install_fd`, `fd_cloexec_snapshot`, `clear_fd_cloexec`) lands
  alongside.

The flat shape is correct, well-tested, and load-bearing. Refactoring
to `Frame { Shared<T> }` would require refactoring every step
function that touches the payload, every test that constructs one,
and every existing slice's accessor (`process.aspace_cap()`,
`process.fds_lock()`, `process.cred()`). The only feature that
**strictly** requires `Shared<T>` is `CLONE_FILES` / `CLONE_VM` /
`CLONE_SIGHAND` (the share-vs-copy clone variants); all of those are
deferred beyond bare-fork in `THREAD_RUNTIME_v1` and the
fork/clone/wait4 plan.

**Implemented flat shape (canonical for v2):**

```rust
pub struct ProcessPayload {
    /// Address-space slot. Atomically swapped at exec Phase 6.
    /// (ELF loader Wave 1.)
    pub(crate) aspace: AtomicSlot<Cap<AddressSpace>>,
    pub(crate) threads: SpinMutex<Vec<Cap<ThreadIdentity>>>,
    pub(crate) sig_actions: SigActionTable,
    pub(crate) group_pending: PendingSignalQueue,
    /// Per-process credential snapshot. (DAC + setuid Wave 2.)
    pub(crate) cred: SpinMutex<Cred>,
    pub(crate) cwd: SpinMutex<Option<Cap<DEntry>>>,
    /// Sparse fd table. Flipped from
    /// `[Option<Cap<OpenFile>>; FD_TABLE_SIZE]` to
    /// `BTreeMap<u32, Cap<OpenFile>>` in fd-ops Wave 1
    /// (2026-05-07). Any `u32` fd is a valid key; `step_fork`
    /// clones the entire map. (Trio Phase 2a; fd-ops Wave 1.)
    pub(crate) fds: SpinMutex<BTreeMap<u32, Cap<OpenFile>>>,
    /// Per-fd close-on-exec set. Flipped from `AtomicU32` to
    /// `BTreeSet<u32>` in fd-ops Wave 1 (2026-05-07). The fd-31
    /// ceiling has been retired alongside the fd-table ceiling.
    /// (DAC + setuid Wave 4; fd-ops Wave 1.)
    pub(crate) fd_cloexec: SpinMutex<BTreeSet<u32>>,
    /// Heap region anchors. (Trio Phase 2b.)
    pub(crate) brk_base: AtomicU64,
    pub(crate) current_brk: AtomicU64,
    /// Reactor wait source for child-zombify events.
    /// (Fork/clone/wait4 Wave 1.)
    pub(crate) exit_source: Channel,
    pub(crate) exit_source_id: u64,
    /// Identity back-pointer.
    pub(crate) identity: tx_substrate::zone::Weak<ProcessIdentity>,
}
```

**Deferred to v3:**

- `Frame { Shared<T> }`. Currently the payload owns `aspace`,
  `fds`, `cwd`, `cred` directly (no `Shared<T>` indirection). v3
  introduces the share-vs-copy split when CLONE_FILES / CLONE_VM /
  CLONE_SIGHAND land.
- `ProcessPolicy`. Currently rlimits and policy hooks live in
  per-call check sites; the cred snapshot does not yet carry the
  rlimit bundle. v3 introduces an explicit `policy: ProcessPolicy`
  field with the rlimit + scheduling-policy bundle.
- `nsproxy: Cap<NsProxy>`. Namespace-aware builds carry this immutable bundle
  on the payload. Older flat-namespace paths can model it as a bundle whose
  fields all point at init namespaces. `NAMESPACE_VIEW_v1` owns the userns,
  pidns, mountns, and netns semantics; PROCESS owns only the pointer
  publication on clone/unshare/setns.
- `group_exit: GroupExit` and `leader_exit_status: AtomicOption<ExitStatus>`.
  Currently exit_group collapse is handled inline by
  `step_exit_group` without a dedicated coordination struct;
  leader-exit-with-survivors is not implemented. v3 adds both when
  the wait4 surface grows beyond bare-zombie reaping.

The v2 amendment **does not** weaken the v3 target. The flat shape
is a load-bearing v2 stable surface; v3 is the refactor pass once
the share-vs-copy clone surface lands.

### 2.3 ProcessGroup

<!-- txdoc:PROCESS-PROCESSGROUP-1 -->

Identity-only (no payload). A process group exists while it has members; has no computation state.

```rust
pub struct ProcessGroup {
    // Naming
    pub pgid_name: PidNameSnapshot,            // non-retaining namespace number snapshot

    // Upward authoritative binding
    pub session: Binding<Session>,

    // Materialization: processes whose pgrp binding names this group
    pub members: DllContainer<ProcessIdentity>,

    // DLL linkage in session.members
    pub session_chain: DllNode<ProcessGroup>,

    // Informational (not authoritative)
    pub leader_pid: Pid,                       // the pid of the original leader; may no longer be live
}
```

- `pgid_name` — non-retaining snapshot of the pgroup's namespace-visible number(s). The authoritative pgid binding lives in `PidNamespace.numbers -> PidName`.
- `session` — authoritative binding to the containing session.
- `members` — downward materialization; walkers re-validate against `proc.pgrp`.
- `leader_pid` — informational only. The leader may have exited; the group persists as long as members remain.

**Lifetime:**
- Created via setpgid (when a process joins a pgid that doesn't exist yet) or inherited at fork.
- Reclaimed when membership falls to zero AND no Cap retainers remain. (Retention is held by `session.members` entry and by each member's `pgrp` binding.)

**Projections:**
- structural — some Cap<ProcessGroup> extant.
- orphaned — true iff no member has a parent in a different pgroup within the same session (derived; drives SIGHUP/SIGCONT semantics, see §8.2).

### 2.4 Session

<!-- txdoc:PROCESS-SESSION-1 -->

Identity-only (no payload).

```rust
pub struct Session {
    // Naming
    pub sid_name: PidNameSnapshot,

    // Materialization: pgroups whose session binding names this session
    pub members: DllContainer<ProcessGroup>,

    // Informational
    pub leader_pid: Pid,

    // Controlling tty (optional). Foreground-pgrp lives on TtyIdentity
    // per OPA-3 (see below).
    pub controlling_tty: Binding<Tty>,
}
```

- `sid_name` — non-retaining snapshot of the session's namespace-visible number(s). The authoritative sid binding lives in `PidNamespace.numbers -> PidName`.
- `members` — downward materialization of pgroups.
- `leader_pid` — informational; the original leader's pid when setsid was called.
- `controlling_tty` — derived peer reference to the tty (if any). The authoritative slot is `TtyIdentity.session_pgrp` per OPA-3 (TTY-owned control binding); `Session.controlling_tty` is the matching mirror so process-side callers can find their tty without walking the device registry. Set when the session leader opens a tty without `O_NOCTTY` (TIOCSCTTY publishes both halves); cleared by TIOCNOTTY or tty hangup.

**Foreground process group — not stored on Session.** Per [`OBJECT_PATTERN_FIXES_v1.md`](../00_meta-framework/OBJECT_PATTERN_FIXES_v1.md) OPA-3 (TTY-CTL-1), the authoritative foreground-pgrp slot lives on `TtyIdentity.session_pgrp`, bundled with the controlling-session reference as a single `SessionPgrp { session, foreground_pgrp }` atomic-swap unit. Process-side callers that need the foreground pgrp use the `Session::foreground_pgrp_cap()` helper, which performs the two-hop weak dereference:

```text
Session ─ controlling_tty ──Weak──▶ TtyIdentity
                                       └ session_pgrp ──Weak──▶ ProcessGroup
```

Either upgrade may return `None` (no controlling tty installed; tty has been reclaimed; tty has no foreground pgrp; pgrp has been reclaimed). Callers must handle the `None` case explicitly — e.g. session-leader death (§8.3) skips the SIGHUP cascade if the helper returns `None`. `tcsetpgrp(3)` is a tty operation that publishes a new `SessionPgrp` via `swap_commit` on the tty, not on the session.

**Lifetime:**
- Created via setsid.
- Reclaimed when no member pgroups remain AND no Cap retainers.

**Projections:**
- structural — some Cap<Session>.
- leader_live — the original leader process is still alive (influences SIGHUP cascade on leader death, §8.3).

---

## 3. Frame

<!-- txdoc:PROCESS-FRAME-1 -->

The Frame is a typed environment with per-resource sharing. It lives **on ProcessPayload** — at exit, the Frame's `Shared<T>` slots release their shares, triggering reclamation of owned resources.

```rust
pub struct Frame {
    // COW-persistent slots (can be independently shared or copied per clone flag)
    pub vm: Shared<AddressSpace>,              // CLONE_VM
    pub fd_table: Shared<FdTable>,             // CLONE_FILES
    pub sig_actions: Shared<SigActionTable>,   // CLONE_SIGHAND
    pub fs_context: Shared<FsContext>,         // CLONE_FS

    // Inline slots — DEntry-shaped because path resolution and
    // `getcwd(2)` rendering both need the named-path edge that
    // `RNode` alone doesn't carry. VFS's `ResolveCtx` takes
    // `Cap<DEntry>` for cwd-bound lookups; the same shape lives here.
    pub cwd: Cap<DEntry>,                      // (fs_context may override; belongs here for speed)
    pub root: Cap<DEntry>,                     // chroot boundary
    pub umask: AtomicU16,

    // Namespace pointers live in ProcessPayload.nsproxy.
}
```

**v1.2 amendment.** Earlier drafts spelled `cwd: Cap<RNode>` and
`root: Cap<RNode>`. This was a simplification that lost the
named-path edge needed to render absolute paths (POSIX `getcwd(2)`
walks the parent-name chain back to the root). VFS's
[`ResolveCtx`](../05_filesystem/VFS_CHECKS_V2.1.md) already takes
a `Cap<DEntry>` for cwd-bound resolution; PROCESS aligns with that
shape. The DEntry's contained `Cap<RNode>` is reachable via
`dentry.rnode()` for code paths that only care about the inode
identity.

### 3.1 Shared<T> semantics

<!-- txdoc:PROCESS-SHARED-SEMANTICS-1 -->

Each Shared<T> is a per-resource sharing wrapper:

- **Share** (`Shared<T>::share`) — increment the wrapper's reference count; returns another `Shared<T>` pointing at the same T. Used when a clone flag is set.
- **Fork-copy** (`Shared<T>::fork_copy`) — increment refcount of a T that is COW-persistent; when a thread later mutates, COW-triggered copy produces a distinct T. For types like AddressSpace and FdTable whose internal structure is a persistent tree, the clone is an O(1) root copy.
- **Get** (`Shared<T>::get`) — read-only reference to the T under a guard.
- **Get-mut** (`Shared<T>::get_mut`) — write access; triggers COW if refcount > 1.

The semantics of each T's cloning are the subsystem's concern, not Frame's. VM owns what AddressSpace COW means; VFS owns what FdTable COW means; etc. Frame just invokes the subsystem's `clone` implementation.

### 3.2 Why Frame is a single struct with inline slots

<!-- txdoc:PROCESS-WHY-FRAME-SINGLE-STRUCT-INLINE-SLOTS-1 -->

The alternative — pushing the sharing machinery into per-subsystem "inheritance trees" — was considered and rejected:

- **Locality.** Frame slots are accessed on almost every syscall (fd lookup, AS lookup). One pointer-hop via inline slot; two or more via a tree. Hot-path cost is significant.
- **Working-set cohesion.** Frame slots are cached together. Scattering them across subsystem-owned structures hurts cache behavior.
- **No information gained.** A subsystem tree would record inheritance history. POSIX doesn't expose inheritance graphs; no consumer needs the history that subsystem trees would provide beyond what Shared<T>'s refcount already captures.
- **Clone complexity unchanged.** Per-flag decisions are equally expressible with Frame-inline slots and with subsystem trees; the branching structure of clone is the same.

Frame is intentionally compact. Namespace context is carried by `ProcessPayload.nsproxy` rather than by per-resource Frame fields; see `NAMESPACE_VIEW_v1`.

---

## 4. ProcessPolicy

<!-- txdoc:PROCESS-PROCESSPOLICY-1 -->

Per the credential and rlimit service specs, authorization and resource accounting are bundled on ProcessPolicy:

```rust
pub struct ProcessPolicy {
    pub cred: Binding<Credential>,             // per credential service
    pub rlimits: Binding<RLimitBag>,           // per rlim v1.1
    // signal_mask REMOVED — now per-thread on ThreadPayload (per THREAD_RUNTIME §10)
}
```

- **Cred** — Credential is a durable entity holding uid/gid/caps. Process holds one via Binding. Execve (Phase 2 when suid/caps is added) can swap this. See the credential service spec.
- **Rlimits** — RLimitBag is a durable entity holding soft/hard ceilings for each rlimit. See rlim v1.1.

**Cross-doc correction (confirmed here):** any `signal_mask: SigMask` field shown in older ProcessPolicy or PolicyBag sketches is **wrong**. Signal mask is per-thread and lives on `ThreadPayload`. The ProcessPolicy struct in this document does not include it.

ProcessPolicy is an embedded struct on ProcessPayload, not a separately-allocated entity. Its Bindings retain their targets (Credential, RLimitBag). At ProcessPayload reclamation, the Bindings drop and release those retentions.

---

## 5. GroupExit: thread-group collapse coordination

<!-- txdoc:PROCESS-GROUPEXIT-THREAD-GROUP-COLLAPSE-COORDINATION-1 -->

When any thread initiates `exit_group` or `execve` (from a multi-threaded process), all other threads in the group must terminate before the initiator can proceed. This coordination uses `ProcessPayload.group_exit`:

```rust
pub struct GroupExit {
    /// One-shot CAS: transitions from None to Some exactly once per process
    /// lifetime. After transition, the enclosed GroupExitState is stable;
    /// subsequent callers observe Some and join the collapse as non-initiators.
    ///
    /// Not a `Binding<T>` — `GroupExitState` is not a semantic entity with
    /// projections or retention. It is transient coordination state, alive
    /// only during the collapse window. Implementable as an atomic pointer
    /// with one-shot CAS semantics (substrate provides this as
    /// `AtomicOneShot<T>` or equivalent; the exact shape is substrate
    /// implementation detail).
    pub state: AtomicOneShot<GroupExitState>,
}

pub struct GroupExitState {
    pub status: ExitStatus,                    // exit status if this is an exit_group
    pub is_exec: bool,                         // initiator wants exec, not exit
    pub remaining_threads: AtomicU32,          // decrements as non-initiator threads exit
    pub completion_channel: Channel,           // reactor channel; initiator waits on it
}
```

Observers never need to upgrade `state` to a Cap — the enclosing `ProcessPayload` already retains it via allocation — so Binding's upgrade-via-CAS machinery is unneeded. A single one-shot atomic suffices. `AtomicOneShot<T>` supports:

- `compare_exchange(None, boxed_state) -> Result<(), ExistingRef>` — one-shot CAS.
- `load(&guard) -> Option<&T>` — guard-safe read.

The substrate primitive lives in `tx-fnd/atomic-oneshot` (or equivalent); the exact naming is implementation-layer.

### 5.1 Flow

<!-- txdoc:PROCESS-FLOW-1 -->

**Initiator:**

1. Construct a GroupExitState with `status`, `is_exec`, `remaining_threads = thread_count - 1` (the initiator itself is not counted), and a fresh completion_channel.
2. `group_exit.state.compare_exchange(None, Some(GroupExitState))`.
3. If CAS succeeds, this thread is the initiator. Proceed to step 4.
4. If CAS fails: someone else already initiated. This thread is not the initiator; it joins the collapse as a non-initiator (proceeds to step_thread_exit per §7).
5. For each thread in ProcessPayload.threads (except self), update its `signal_summary.termination = true` and fire its task waker. (Done atomically under a walk of the thread DLL.)
6. If `remaining_threads == 0` at the start (single-threaded process), skip to step 8.
7. Wait on completion_channel (reactor wait, `WaitProtocol::Killable`; this wait cannot be interrupted except by the kernel itself terminating the initiator).
8. Proceed:
   - If `is_exec`: continue with execve's AS-replacement / fd-close-on-exec / sig-reset phases.
   - Else (exit_group): this thread is now the last thread. Proceed to step_thread_exit for the initiator, which will trigger step_process_exit.

**Non-initiator thread exits (via site A/B observing termination):**

1. Run step_thread_exit normally.
2. In step_thread_exit's commit phase, after the thread's state is updated but before returning:
   - If `group_exit.state.load()` is Some: decrement `remaining_threads`.
   - If the decrement brought it to 0: fire `completion_channel` waker.

**Clone during collapse:**

Clone(CLONE_THREAD) during an active group_exit is forbidden. Clone's observe phase reads `group_exit.state`; if Some, clone fails with EAGAIN (Linux uses EAGAIN for resource contention). This prevents new threads from joining a dying process.

### 5.2 Invariants

<!-- txdoc:PROCESS-INVARIANTS-1 -->

- **Single initiator.** The CAS on `state` ensures only one thread initiates a given collapse.
- **Bounded wait.** Remaining threads are each a running reactor task. Each, on its next delivery site, observes termination and exits. No thread can escape the collapse (short of a kernel bug).
- **No new threads.** Clone checks `group_exit.state` at commit; denied while collapsing.
- **Completion fires exactly once.** `remaining_threads` is a monotonic countdown. The thread whose decrement brings it to 0 fires the waker. Subsequent decrements (impossible given no new threads) would be bugs.

### 5.3 Observer behavior during collapse

<!-- txdoc:PROCESS-OBSERVER-BEHAVIOR-DURING-COLLAPSE-1 -->

During the collapse window, the process is transitioning but not yet exited:

- ProcessIdentity still alive (Cap retained).
- ProcessPayload still alive (initiator + non-exited siblings).
- Threads DLL contains live threads decreasing over time.
- waitpid on the process: blocks (payload still Some).
- /proc/pid: rendered; shows the transitional state (threads count decreasing).
- Signal delivery: new signals still queue to group_pending; non-exiting threads may still receive them briefly before being told to terminate.

This is a brief window (typically microseconds-milliseconds). POSIX doesn't specify intermediate-state visibility; the observed behavior is internally consistent.

---

## 6. Leader-exit-with-survivors

<!-- txdoc:PROCESS-LEADER-EXIT-SURVIVORS-1 -->

POSIX permits the thread-group leader to exit (via `exit(2)` or `pthread_exit`) while non-leader threads continue running. The process is not reaped until all threads have exited.

### 6.1 Data structure

<!-- txdoc:PROCESS-DATA-STRUCTURE-1 -->

```rust
// In ProcessPayload:
pub leader_exit_status: AtomicOption<ExitStatus>,

// In ProcessIdentity:
pub exit_status: AtomicOption<ExitStatus>,    // process-visible final status
```

- `leader_exit_status` — set when the leader thread calls exit. Held until process_exit fires.
- `ProcessIdentity.exit_status` — written at process_exit. Read by waitpid.

### 6.2 Priority of process exit status

<!-- txdoc:PROCESS-PRIORITY-PROCESS-EXIT-STATUS-1 -->

When process_exit fires (last thread exits), the status is determined by priority:

1. `ProcessPayload.group_exit.state.status` — highest priority. Set by exit_group.
2. `ProcessPayload.leader_exit_status` — second priority. Set by leader's ordinary exit.
3. Last-thread's exit_status — fallback. Used when the leader is still alive and a sibling happens to be the last to exit (rare).

```rust
fn compute_process_exit_status(payload: &ProcessPayload, last_thread_status: ExitStatus) -> ExitStatus {
    if let Some(state) = payload.group_exit.state.load_snapshot() {
        return state.status;
    }
    if let Some(leader_status) = payload.leader_exit_status.load() {
        return leader_status;
    }
    last_thread_status
}
```

### 6.3 Step flow

<!-- txdoc:PROCESS-STEP-FLOW-1 -->

**Leader calls exit(status) (ordinary, not exit_group):**

1. In step_thread_exit commit phase, detect "this thread is the leader" (the thread's tid name is the same role-capable pid/tgid name as the process leader).
2. If true and `thread_count > 1` (siblings exist): record status in `payload.leader_exit_status`.
3. Decrement thread_count.
4. If thread_count reaches 0: trigger step_process_exit with status from `compute_process_exit_status`.

**Non-leader calls exit:**

1. step_thread_exit commit phase, as normal.
2. Decrement thread_count.
3. If thread_count reaches 0: last thread. Trigger step_process_exit with status from `compute_process_exit_status` (which will use leader_exit_status if leader already exited, else the current thread's status).

**exit_group:**

1. Initiator sets `group_exit.state` (§5.1). `state.status` holds the process-visible status.
2. All threads cascade exit.
3. Last thread triggers step_process_exit; `compute_process_exit_status` returns `state.status`.

### 6.4 Observability during leader-survivors window

<!-- txdoc:PROCESS-OBSERVABILITY-DURING-LEADER-SURVIVORS-WINDOW-1 -->

After leader exits, before last sibling exits:

- `ProcessIdentity.payload.load().is_some()` — true. Process is not zombied yet.
- Leader's ThreadIdentity in threads DLL, with `payload.is_none()` (thread-zombied) and exit_status set.
- waitpid on the process: blocks (payload still Some).
- /proc/pid/status: rendered normally.
- /proc/pid/task: lists all threads including the zombie leader (with state "Z").
- Signal to process (kill(pid, sig)): queues to group_pending; alive siblings may deliver.
- Signal to the leader tid specifically (tgkill(pid, pid, sig)): reaches the zombie leader, which cannot deliver (it has no payload). The signal is queued in group_pending but never processed for this thread; it may be picked up by a sibling if the signal is not thread-specific.

---

## 7. Step catalog

<!-- txdoc:PROCESS-STEP-CATALOG-1 -->

This section specifies the steps that implement the process subsystem's POSIX surface. Each step follows the five-phase discipline (observe → upgrade → reserve → commit → publish) per SUBSYSTEM_ANATOMY §3. Topology-mutation steps invoke the compositions specified in [`MUTATION_COMPOSITIONS_v1.md`](../01_substrate/MUTATION_COMPOSITIONS_v1.md) (`structural_move`, `structural_withdraw`, `structural_publish`); bitmap-allocation steps use the reservation primitives specified in [`BITMAP_RESERVATION_v1.md`](../01_substrate/BITMAP_RESERVATION_v1.md).

### 7.1 fork and clone

<!-- txdoc:PROCESS-FORK-AND-CLONE-1 -->

Linux has `fork(2)`, `vfork(2)`, `clone(2)`, `clone3(2)`. They all call into the same underlying mechanism. In POSIX, `fork(2)` is the syscall; Linux clone is the generalization.

**fork(2)** = clone with no CLONE_* flags (and SIGCHLD as termination signal for parent's waitpid).

**pthread_create** = clone with CLONE_VM | CLONE_FILES | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD (plus CLONE_CHILD_CLEARTID, CLONE_SETTLS, etc.).

#### 7.1.1 Clone flag support (v1)

<!-- txdoc:PROCESS-CLONE-FLAG-SUPPORT-V1-1 -->

| Flag | v1 support | Notes |
|---|---|---|
| CLONE_VM | ✓ | Share AddressSpace via `Frame.vm.share()` |
| CLONE_FILES | ✓ | Share FdTable |
| CLONE_FS | ✓ | Share fs_context |
| CLONE_SIGHAND | ✓ | Share sig_actions |
| CLONE_THREAD | ✓ | New thread in current process (vs. new process) |
| CLONE_PARENT_SETTID | ✓ | Write new tid to specified userspace buffer at clone time |
| CLONE_CHILD_SETTID | ✓ | Write new tid to child-side userspace buffer |
| CLONE_CHILD_CLEARTID | ✓ | Per THREAD_RUNTIME §2.6; register addr for FUTEX_WAKE at exit |
| CLONE_SETTLS | ✓ | HAL sets TLS register in new thread's initial state |
| CLONE_PARENT | deferred | Phase 2 |
| CLONE_NEWUSER | deferred for clone | `unshare(CLONE_NEWUSER)` follows `NAMESPACE_VIEW_v1`; clone support must create the user namespace first and grant capabilities only inside it. |
| CLONE_NEWNET | deferred for clone | `unshare(CLONE_NEWNET)` follows `NAMESPACE_VIEW_v1`; when combined with `CLONE_NEWUSER`, Linux creates userns first and owns the new netns by it. |
| CLONE_NEWPID, CLONE_NEWNS, etc. | deferred | Phase 2 (nested namespaces and remaining namespace kinds) |
| CLONE_PIDFD | deferred | Phase 2 |
| CLONE_PTRACE, CLONE_UNTRACED | deferred | Observation subsystem |
| CLONE_VFORK | deferred | Rare in practice |
| CLONE_IO | deferred | I/O context sharing |
| CLONE_SYSVSEM | deferred | SysV sem subsystem |
| Others (CLONE_DETACHED, CLONE_CHILD_IMMEDIATE, etc.) | ignored | Historical / unused |

Unsupported flags return EINVAL from clone. clone3 supports the same flag subset.

#### 7.1.2 step_clone_process (new-process path; !CLONE_THREAD)

<!-- txdoc:PROCESS-STEP-CLONE-PROCESS-NEW-PROCESS-PATH-CLONE-THREAD-1 -->

```rust
fn step_clone_process(
    flags: CloneFlags,
    caller_thread: Cap<ThreadIdentity>,
    caller_proc: Cap<ProcessIdentity>,
    stack_ptr: Option<UserPtr>,          // if None, use parent's
    parent_tid_ptr: Option<UserPtr<u32>>, // CLONE_PARENT_SETTID
    child_tid_ptr: Option<UserPtr<u32>>,  // CLONE_CHILD_SETTID / CLONE_CHILD_CLEARTID
    tls: Option<UserAddr>,                // CLONE_SETTLS
) -> StepOutcome<Pid> {
    // Phase 1: observe
    //   - deny if caller_proc's group_exit is in progress
    //   - check namespace-relative authority if flags request privileged
    //     namespaces (Phase 2; userns/netns details live in NAMESPACE_VIEW_v1)

    // Phase 2: upgrade
    //   - upgrade caller_proc and caller_thread to Cap (already held)

    // Phase 3: reserve
    //   - reserve_rlimit: RLIMIT_NPROC (cred-gated; see rlim v1.1)
    //   - reserve_zone: new ProcessIdentity slot
    //   - reserve_zone: new ProcessPayload slot
    //   - reserve_zone: new ThreadIdentity slot (initial thread)
    //   - reserve_zone: new ThreadPayload slot
    //   - read caller_proc.payload.nsproxy.pid_for_children
    //   - reserve_pid_name: allocate pid/tid numbers in child pid namespace
    //     and visible ancestors; reserve PidName slots and namespace index slots
    //   - reserve share/copy of each Frame slot per CLONE_* flags:
    //     - if CLONE_VM: vm.share() reservation (share with parent)
    //     - else: vm.fork_copy() reservation (COW root copy)
    //     - similarly for fd_table, sig_actions, fs_context
    //   - initialize all zone-reserved structs with appropriate content,
    //     including non-retaining PidNameSnapshot values on target identities

    //   POINT OF NO RETURN begins in phase 4. All preceding failures
    //   drop reservations cleanly.

    // Phase 4: commit (infallible)
    //   - commit ProcessIdentity zone slot; its retention starts at 1 (local Cap)
    //   - commit ProcessPayload zone slot; link identity to payload
    //   - commit ThreadIdentity/Payload zone slots
    //   - link ThreadIdentity into ProcessPayload.threads DLL; atomic thread_count = 1
    //   - set child.parent binding to caller_proc
    //   - set child.pgrp binding to caller_proc.pgrp (inherit)
    //   - commit namespace index entries: pid/tid numbers -> PidName
    //   - insert child into caller_proc.children DLL
    //   - insert child into pgrp.members DLL
    //   - commit rlimit charge
    //   - commit pid-name reservations
    //   - write parent_tid_ptr and child_tid_ptr if requested (best-effort)
    //   - set child thread's CLONE_CHILD_CLEARTID address if requested
    //   - set child thread's TLS if requested
    //   - submit child thread's future to reactor

    // Phase 5: publish
    //   - tracepoint: trace_process_forked(parent_pid, child_pid)
    //   - fire lifecycle wake: parent's "children state changed" (for waitpid)

    StepOutcome::Done(child_pid)
}
```

The new process is observable as soon as the namespace index commit completes. External observers resolving the child's pid through `PidNamespace.numbers` find a `PidName` that targets a fully formed `ProcessIdentity`.

**Atomicity note:** commit phase publishes to multiple independent structures (`PidNamespace.numbers`, `parent.children`, `pgrp.members`). These are independently atomic but not cross-structure atomic. This is class-3 compositional (per BINDING_v1 §5.4 / CONCEPTS §8): a brief window exists where the child is visible in some indexes but not others. POSIX does not specify atomicity across such indexes; the behavior is acceptable. Each namespace entry must nevertheless target a fully initialized identity.

#### 7.1.3 step_clone_thread (CLONE_THREAD path)

<!-- txdoc:PROCESS-STEP-CLONE-THREAD-CLONE-THREAD-PATH-1 -->

```rust
fn step_clone_thread(
    flags: CloneFlags,
    caller_thread: Cap<ThreadIdentity>,
    caller_proc: Cap<ProcessIdentity>,
    stack_ptr: UserPtr,                   // required for new thread
    parent_tid_ptr: Option<UserPtr<u32>>,
    child_tid_ptr: Option<UserPtr<u32>>,
    tls: Option<UserAddr>,
) -> StepOutcome<Tid> {
    // Phase 1: observe
    //   - caller_proc.group_exit.state == None (collapse check)
    //   - CLONE_SIGHAND without CLONE_VM is rejected (Linux rule)

    // Phase 2: upgrade

    // Phase 3: reserve
    //   - reserve_rlimit: RLIMIT_NPROC (threads count against this)
    //   - reserve_zone: new ThreadIdentity, ThreadPayload
    //   - reserve_pid_name (tid): allocate from caller's active pid namespace
    //   - No Frame slot changes (threads share the process's Frame)

    // Phase 4: commit (infallible)
    //   - commit ThreadIdentity, ThreadPayload
    //   - link ThreadIdentity into caller_proc.payload.threads DLL
    //   - increment caller_proc.payload.thread_count
    //   - commit tid number -> PidName(Thread) into PidNamespace.numbers
    //   - write parent_tid_ptr and child_tid_ptr
    //   - set child_tid_clear address
    //   - set TLS
    //   - submit new thread's future to reactor

    // Phase 5: publish
    //   - tracepoint: trace_thread_created(tgid, new_tid)

    StepOutcome::Done(new_tid)
}
```

### 7.2 execve (v1 scope)

<!-- txdoc:PROCESS-EXECVE-V1-SCOPE-1 -->

v1 supports static ELF binaries executed from the thread-group leader. Dynamic linking (PT_INTERP) and non-leader exec are deferred.

```rust
async fn script_execve(path: &str, argv: &[&str], envp: &[&str]) -> Result<!, Errno> {
    // Phase A: resolve and authorize
    let target = vfs::resolve(path).await?;        // VFS walk
    require_executable(&target, &caller_cred)?;    // cred check (Phase 2 for full: suid, caps)
    let parsed = elf::load_header_and_segments(&target).await?;  // ELF parse

    // Phase B: thread-group collapse (if multi-threaded)
    if caller_proc.payload.thread_count.load() > 1 {
        if !caller_is_leader() {
            return Err(Errno::ENOSYS);  // v1 limitation: only leader may exec
        }
        initiate_group_exit_for_exec(caller_proc).await?;
        // After this returns, caller is the sole thread.
    }

    // Phase C: POINT OF NO RETURN — begin irreversible mutations

    // New AddressSpace from ELF
    let new_as = VM::build_address_space_from_elf(&parsed)?;

    // Replace Frame.vm (COW: fork or replace depending on CLONE_VM at original clone)
    caller_proc.payload.frame.vm.replace(new_as);

    // fd table: scan for close-on-exec, close those
    caller_proc.payload.frame.fd_table.close_on_exec_scan();

    // sig_actions: reset to default per POSIX (non-ignored signals → SIG_DFL; ignored stay ignored)
    caller_proc.payload.frame.sig_actions.exec_reset();

    // Reset other per-exec state (pending signals preserved per POSIX)
    caller_thread.payload.alt_stack = None;
    // Signal mask preserved per POSIX
    // cred preserved (suid/caps reset deferred to Phase 2)

    // Set up new userspace stack with argv/envp
    let new_sp = build_argv_envp_stack(&parsed, argv, envp)?;
    caller_thread.payload.regs.set_sp(new_sp);
    caller_thread.payload.regs.set_pc(parsed.entry_point);

    // Phase D: publish (tracepoint)
    trace_process_execd(caller_proc.pid_name, path);

    // Return to userspace; new image starts executing
    // (Script returns !; userspace entry happens via normal outer loop.)
    unreachable!()
}
```

**Failure modes:**

- Phase A failures (ENOENT, EACCES, ENOEXEC, etc.) — exec fails cleanly, caller continues.
- Phase B collapse failure — not possible (collapse cannot fail).
- Phase C onward — no rollback. Any failure here terminates the process (SIGBUS or kernel panic depending on the failure point). This is POSIX-acceptable per SUBSYSTEM_ANATOMY §3.7 (point of no return).

### 7.3 Exit steps

<!-- txdoc:PROCESS-EXIT-STEPS-1 -->

#### 7.3.1 step_thread_exit

<!-- txdoc:PROCESS-STEP-THREAD-EXIT-1 -->

Specified in THREAD_RUNTIME_v1 §7.2. This subsection covers process-level concerns.

When step_thread_exit runs, after it has updated thread state:

1. Decrement `payload.thread_count`.
2. If the exiting thread is the leader and `thread_count > 0`: record status in `payload.leader_exit_status`.
3. If `thread_count == 0`: trigger step_process_exit (this was the last thread).
4. If `group_exit.state` is Some: decrement `remaining_threads`; if reaches 0, fire completion_channel.

Cases:
- `thread_count > 0` after decrement: thread exit complete; process continues.
- `thread_count == 0`: process_exit must fire. Use the priority rule (§6.2) to determine status.

#### 7.3.2 step_exit_group

<!-- txdoc:PROCESS-STEP-EXIT-GROUP-1 -->

```rust
fn step_exit_group(status: ExitStatus, caller_proc: Cap<ProcessIdentity>) -> StepOutcome<!> {
    // Phase 1: observe — check if collapse already in progress (CAS check first)

    let collapse_state = GroupExitState {
        status,
        is_exec: false,
        remaining_threads: AtomicU32::new(thread_count - 1),
        completion_channel: Channel::new(),
    };

    match caller_proc.payload.group_exit.state.compare_exchange(
        None,
        Some(Cap::new(collapse_state)),
    ) {
        Ok(_) => {
            // This thread is initiator. Wake all other threads.
            wake_all_non_self_threads(caller_proc);
            // Wait for collapse completion.
            reactor::wait(completion_channel, MASK_COMPLETE, WaitProtocol::Killable).await;
            // Now this thread exits too. step_thread_exit (which will be the last).
            step_thread_exit(caller_thread, status)?;
            // step_thread_exit will trigger step_process_exit because thread_count==0.
            unreachable!()
        }
        Err(_) => {
            // Someone else initiated; we join as non-initiator.
            // Fall through to our own step_thread_exit.
            let existing_status = caller_proc.payload.group_exit.state.load().status;
            step_thread_exit(caller_thread, existing_status)?;
            unreachable!()
        }
    }
}
```

#### 7.3.3 step_process_exit

<!-- txdoc:PROCESS-STEP-PROCESS-EXIT-1 -->

```rust
fn step_process_exit(proc: Cap<ProcessIdentity>, status: ExitStatus) -> StepOutcome<()> {
    // Phase 1: observe — payload must exist (we're the ones dropping it)
    // Phase 2: upgrade
    // Phase 3: reserve — nothing fallible at this stage (POINT OF NO RETURN already passed)

    // Phase 4: commit (all infallible)
    //   - write exit_status to ProcessIdentity
    //   - reparent children (see §8.1)
    //   - orphan pgroup SIGHUP/SIGCONT (see §8.2)
    //   - if this is session leader: controlling-tty SIGHUP cascade (see §8.3)
    //   - drop ProcessPayload.payload: transitions from Some → None
    //     (This triggers ProcessPayload drop: Frame slots' Shared<T> release;
    //      policy drops; group_pending discarded; threads DLL drained;
    //      ProcessPayload zone slot eligible for reclamation)
    //   - remove process from pgrp.members (structural_withdraw)
    //     (Note: this releases pgroup's Cap on this process; pgroup retention
    //      decrements. If pgroup was waiting on this process's reap for full
    //      reclamation, this is the release point.)

    // Phase 5: publish
    //   - deliver_posix_signal(SignalTarget::Process(parent), SIGCHLD,
    //       SigInfo { si_pid, si_uid, si_code: CLD_EXITED | CLD_KILLED | CLD_DUMPED,
    //                 si_status: exit_status, .. })
    //     (per SIGNAL_v1 §17.2; subject to parent's SIGCHLD disposition and
    //      SA_NOCLDSTOP / SA_NOCLDWAIT flags)
    //   - fire exit_source: SignalGenerated{Exited(status)} per SIGNAL_ATTACHMENTS_v1 §3.3
    //     (for waitpid direct observers, pidfd subscribers, ptrace tracer)
    //   - fire wait-queue wake: parent's waitpid waiters
    //   - tracepoint: trace_process_exited(pid, status)
}
```

After step_process_exit:
- ProcessIdentity persists (zombie); retention held by parent.children.
- ProcessPayload reclaimed (zone slot returned after epoch quiescence).
- Children reparented; orphan-pgroup SIGHUP fired if applicable.
- Parent will reap via waitpid → withdraw PidName entries and parent.children.

### 7.4 Wait family

<!-- txdoc:PROCESS-WAIT-FAMILY-1 -->

`waitpid`, `wait4`, `waitid` — all reap zombie children. v1 supports WEXITED, WSTOPPED, WNOHANG. WCONTINUED and Linux-specific flags (__WALL, __WCLONE) deferred.

```rust
async fn script_waitpid(pid: Pid, options: WaitOptions) -> Result<(Pid, ExitStatus), Errno> {
    loop {
        let guard = epoch::guard();

        // Walk children DLL; find reapable child(ren).
        let reapable = find_reapable_child(caller_proc, pid, options, &guard)?;

        if let Some(child) = reapable {
            return reap_child(child);
        }

        if options.contains(WNOHANG) {
            return Err(Errno::ECHILD);  // or 0 per POSIX variant
        }

        // Block until a child's state changes.
        let wait_channel = caller_proc.children_state_channel();
        match reactor::wait(wait_channel, MASK_CHILD_STATE, WaitProtocol::Interruptible).await {
            WaitOutcome::Ready => continue,  // re-check
            WaitOutcome::Interrupted => return Err(Errno::EINTR),
            WaitOutcome::Killed => return Err(Errno::EINTR),
            _ => continue,
        }
    }
}
```

`find_reapable_child`:
- Walk children DLL with re-validation.
- For each entry: apply the resolved wait selector. Exact pid selectors compare against the canonical child identity; pgrp selectors compare against `entry.pgrp`.
- For each matching entry: check `entry.payload.load().is_none()` (zombied).
- Return first zombie found, or None.

`reap_child`:
- Read `child.exit_status`.
- `structural_withdraw` from parent.children DLL.
- withdraw pid-name entries from `PidNamespace.numbers` (Phase 1: also from pgrp.members since v1 keeps zombies in pgroup until reap).
- Release RLIMIT_NPROC charge.
- Drop local Cap; child's retention drops; SENTINEL_DEAD; child identity reclaimed.
- Return (pid, exit_status).

### 7.5 Session and pgroup steps

<!-- txdoc:PROCESS-SESSION-PGROUP-STEPS-1 -->

#### 7.5.1 step_setsid

<!-- txdoc:PROCESS-STEP-SETSID-1 -->

```rust
fn step_setsid(caller: Cap<ProcessIdentity>) -> StepOutcome<Sid> {
    // Phase 1: observe
    //   - caller is not already a session leader or pgroup leader
    //   - caller's current pgroup is not pgid == caller's rendered pid
    //     (i.e., caller is not a pgroup leader in this pid namespace)

    // Phase 2: upgrade

    // Phase 3: reserve
    //   - reserve_zone: new Session slot
    //   - reserve_zone: new ProcessGroup slot
    //   - reserve role-capable PidName publication for sid/pgid using the
    //     caller's pid number in the caller's pid namespace. The number is
    //     reused as a session id and process-group id; it is not a second
    //     conflicting namespace entry.

    // Phase 4: commit
    //   - initialize new Session: sid_name = caller.pid_name snapshot, members = empty DLL, controlling_tty = None
    //   - initialize new ProcessGroup: pgid_name = caller.pid_name snapshot, session = new_session, members = empty
    //   - commit namespace number role(s) -> PidName(Session/ProcessGroup)
    //   - structural_publish: new pgroup into session.members
    //   - structural_move: caller from old pgroup to new pgroup (updates caller.pgrp binding)

    // Phase 5: publish
    StepOutcome::Done(render_process_pid(caller, current_ctx().nsproxy.pid_ns)?)
}
```

#### 7.5.2 step_setpgid

<!-- txdoc:PROCESS-STEP-SETPGID-1 -->

```rust
fn step_setpgid(target_pid: Pid, new_pgid: Pgid, caller: Cap<ProcessIdentity>) -> StepOutcome<()> {
    // Phase 1: observe
    //   - resolve target_pid through caller.nsproxy.pid_ns to ProcessIdentity
    //   - authorization check: target must be caller or caller's child
    //   - target must be in same session as caller
    //   - if new_pgid != rendered target pid, pgroup must already exist (target joins existing)
    //   - if new_pgid == rendered target pid, pgroup may be created (target becomes leader)
    //   - target must not have exec'd since fork (Linux rule)

    // Phase 2: upgrade

    // Phase 3: reserve
    //   - if new pgroup needed: reserve Zone slot, reserve PidName and namespace index slot
    //   - reserve structural_move operands

    // Phase 4: commit
    //   - if creating new pgroup: initialize, commit pgid number -> PidName, structural_publish into session.members
    //   - structural_move: target from old_pgrp to new_pgrp (updates target.pgrp binding)

    // Phase 5: publish
    StepOutcome::Done(())
}
```

### 7.6 Signal routing

<!-- txdoc:PROCESS-SIGNAL-ROUTING-1 -->

**Canonical entry point:** [`SIGNAL_v1.md`](./SIGNAL_v1.md) §12 specifies `deliver_posix_signal(target, signum, siginfo)` as the single entry point for all POSIX-signal production across the kernel. All callers (kill/tkill/tgkill syscalls, tty ldisc, timer expiry, fault handlers, step_process_exit, etc.) invoke this entry point; routing decisions (Gewalt vs catchable, disposition consultation) happen inside it.

**Process-subsystem-internal helpers** (implementation detail of `deliver_posix_signal` when its target is a Process or ProcessGroup):

- **post_to_group_pending(proc, sig, siginfo):** enqueue into `proc.payload.group_pending`; update `signal_summary.has_deliverable` on one selected unmasking thread per THREAD_RUNTIME §5.6; fire that thread's waker. Invoked by `deliver_posix_signal` for `SignalTarget::Process` with catchable-handler disposition.

- **post_to_thread_pending(thread, sig, siginfo):** enqueue into `thread.payload.thread_pending`; update `signal_summary.has_deliverable`; fire thread's waker. Invoked by `deliver_posix_signal` for `SignalTarget::Thread` with catchable-handler disposition.

- **process_group_fanout(pgrp, sig, siginfo):** iterate `pgrp.members` under epoch guard; for each member (re-validated against its pgrp binding), invoke the Process-targeted routing recursively. Invoked by `deliver_posix_signal` for `SignalTarget::ProcessGroup`.

These helpers are **fan-out compositions** over thread-level primitives: the process entity provides the iteration target (the threads DLL, the pgrp's members DLL) and the group-level pending queue, while each thread provides the per-unit semantic (its signal_summary, its thread_pending, its waker). The process is the **dispatcher** for these ops, not the semantic unit — see §1.4 for the full thread/process provision framing.

**Gewalt routing** (SIGKILL/SIGSTOP/SIGCONT) bypasses these helpers and invokes the control primitives directly:

- SIGKILL → `GroupExit` coordination (§5).
- SIGSTOP → iterate threads, set `stop_state = StopRequested` per THREAD_RUNTIME §6.
- SIGCONT → iterate threads, transition `stop_state` and fire stop channels per THREAD_RUNTIME §6.

These are also fan-out compositions, but their targets are `stop_state` / `signal_summary.termination` rather than pending queues.

### 7.7 Reading operations

<!-- txdoc:PROCESS-READING-OPERATIONS-1 -->

Simple reads, no structural changes.

```rust
fn getpid(thread: Cap<ThreadIdentity>) -> Pid {
    render_process_pid(thread.owner_proc, current_ctx().nsproxy.pid_ns)
}

fn getppid(thread: Cap<ThreadIdentity>) -> Pid {
    let guard = epoch::guard();
    let proc = thread.owner_proc.load(&guard);
    proc.parent
        .load(&guard)
        .and_then(|p| render_process_pid(p, current_ctx().nsproxy.pid_ns))
        .unwrap_or(0)  // 0 if parent is not visible in this pid namespace
}

fn gettid(thread: Cap<ThreadIdentity>) -> Tid {
    render_thread_tid(thread, current_ctx().nsproxy.pid_ns)
}

fn getpgid(pid: Pid) -> Pgid {
    let proc = resolve_process_for_query(current_ctx().nsproxy.pid_ns, pid, &guard)?;
    let pgrp = proc.pgrp.load(&guard)?;
    render_pgrp_id(pgrp, current_ctx().nsproxy.pid_ns).ok_or(ESRCH)?
}

fn getsid(pid: Pid) -> Sid {
    let proc = resolve_process_for_query(current_ctx().nsproxy.pid_ns, pid, &guard)?;
    let session = proc.pgrp.load(&guard)?.session.load(&guard)?;
    render_session_id(session, current_ctx().nsproxy.pid_ns).ok_or(ESRCH)?
}
```

All reads are under epoch guards; no locks. Numeric arguments resolve through
the caller's pid namespace; return values render through the caller's pid
namespace. Canonical parent, pgrp, and session bindings are not rewritten for
visibility holes.

### 7.8 set_tid_address

<!-- txdoc:PROCESS-SET-TID-ADDRESS-1 -->

Per THREAD_RUNTIME §2.6:

```rust
fn step_set_tid_address(addr: UserPtr<u32>, caller_thread: Cap<ThreadIdentity>) -> StepOutcome<Tid> {
    // Phase 4: commit (no reservation needed)
    caller_thread.payload.child_tid_clear.store(Some(addr));
    StepOutcome::Done(caller_thread.tid)
}
```

Returns the calling thread's tid (POSIX-mandated return value).

---

## 8. Cross-cutting lifecycle patterns

<!-- txdoc:PROCESS-CROSS-CUTTING-LIFECYCLE-PATTERNS-1 -->

### 8.1 Reparenting

<!-- txdoc:PROCESS-REPARENTING-1 -->

When a process P exits (step_process_exit), its children must be reparented. They cannot be left with a stale parent pointer.

**Target:** pid 1 (init) in P's pid namespace. Phase 2 may refine to CHILD_SUBREAPER ancestor.

**Mechanism:** P's exit step iterates its own children DLL under a guard. For each child C:

```rust
structural_move(
    element: C,
    binding: &C.parent,
    expected_old: P,
    new_container_cap: init,
    from_dll: &P.children,
    new_container_dll: &init.children,
)
```

This is a substrate-mutation composition (per [`MUTATION_COMPOSITIONS_v1.md`](../01_substrate/MUTATION_COMPOSITIONS_v1.md) §2.1 `structural_move`). The CAS on `C.parent` is the linearization.

**Failure modes:** CAS can fail if C.parent was concurrently changed (shouldn't happen since only P's exit step reparents P's children, and P has only one exit step; but defensive). If CAS fails, the reparent loop re-reads and retries.

**After reparenting:** each child's parent binding names init. init's children DLL contains all reparented children. P.children is empty. P proceeds with step_process_exit.

### 8.2 Orphan pgroup SIGHUP

<!-- txdoc:PROCESS-ORPHAN-PGROUP-SIGHUP-1 -->

POSIX rule: when a process exit orphans a process group (all remaining members have parents outside the group, and the group contains stopped members), send SIGHUP + SIGCONT to every member.

**Trigger:** at step_process_exit of a process P. P's exit may orphan pgroups containing P's children (after reparenting to init, those children may now be in an orphaned state).

**Detection:** a pgroup is orphaned iff every member has parent in a different pgroup within the same session. After reparenting (children go to init, which is in session 1, different session from most users' sessions), many children's pgroups become orphaned.

**Optimization:** Only check pgroups that changed membership (either lost P or gained reparented children). In practice: for each child C reparented, check if C's pgroup is now orphaned.

**Action:** for each newly-orphaned pgroup that contains any stopped member (any member with stop_state == Stopped), enqueue SIGHUP then SIGCONT to every member of the pgroup.

This is a cascade of `deliver_posix_signal(SignalTarget::Process(member), SIGHUP, ...)` followed by `deliver_posix_signal(SignalTarget::Process(member), SIGCONT, ...)` calls, invoked for each member of the orphaned pgroup. Done synchronously within step_process_exit's commit phase. Per SIGNAL_v1 §12, these calls will dispatch to the appropriate routing (handler-pending enqueue, SIG_DFL term, SIGCONT control-op fan-out, etc.) per each member's sig_actions.

### 8.3 Session leader death

<!-- txdoc:PROCESS-SESSION-LEADER-DEATH-1 -->

When a session leader process exits, `step_process_exit` runs the controlling-tty severance cascade in its commit phase. The foreground-pgrp slot is on `TtyIdentity` (OPA-3, §2.4), so the cascade is a **two-hop weak dereference** with `None`-tolerant handling at each hop:

1. **Resolve foreground pgrp via the controlling tty.** Walk `session.foreground_pgrp_cap()` (defined in §2.4):
   - First hop: upgrade `session.controlling_tty: Weak<TtyIdentity>`. If `None` (TIOCNOTTY already ran, or tty already reclaimed), the cascade is a no-op — the session had no controlling tty at exit time.
   - Second hop: read `tty.session_pgrp` and upgrade its `foreground_pgrp: Weak<ProcessGroup>`. If `None` (no fg pgrp installed, or pgrp reclaimed), skip the SIGHUP step but still proceed to step 3 (the tty's own session_pgrp slot must be cleared regardless).
2. **SIGHUP cascade.** If both upgrades succeeded, `deliver_posix_signal(SignalTarget::ProcessGroup(fg_pgrp), SIGHUP, siginfo)`. POSIX §11.1.3 also requires SIGCONT to wake any stopped processes in the fg pgrp; that follows the SIGHUP per `TTY.md` §11.
3. **Clear the tty's session-pgrp binding.** `tty.clear_session_pgrp()` — the tty no longer has a controlling session. This severs the binding on the authoritative side (TTY); subsequent `session.foreground_pgrp_cap()` calls will short-circuit at the second hop.
4. **Clear `session.controlling_tty`.** The mirror on the session side. After this, the session continues to exist (still retained by its pgrps) but has no tty linkage — the leader-exit-with-survivors window (§6) keeps the session reapable.

**Atomicity note.** Steps 1–4 are not a single atomic publication; the cascade is class-3 compositional per `BINDING_v1`. A concurrent reader on another hart can observe the SIGHUP delivered while the tty's `session_pgrp` is still set, or vice versa. POSIX does not constrain intermediate-state visibility for session-leader death; the observed behavior is internally consistent with each individual atomic publication.

**Observable effect:** terminal-connected sessions see SIGHUP when the shell exits. Standard Unix behavior (the "logout hangs up running jobs" behavior).

### 8.4 Exit-group cascade coordination

<!-- txdoc:PROCESS-EXIT-GROUP-CASCADE-COORDINATION-1 -->

Specified in §5.

### 8.5 Zombie pgroup membership

<!-- txdoc:PROCESS-ZOMBIE-PGROUP-MEMBERSHIP-1 -->

**Decision:** zombies (ProcessIdentity with payload=None) stay in pgrp.members and session.members until reap. Withdrawn at reap, not at exit.

**Rationale:**
- waitpid(-pgid) needs to enumerate children in a pgroup, including zombies.
- Signal delivery (kill(-pgid)) naturally skips zombies via the `member.payload.is_some()` check at per-member dispatch.
- pgroup reclamation is refcount-driven; zombie members keep the pgroup alive until reap (which is correct — the pgroup should persist as long as any of its members is addressable).

**Walker discipline:** kill(-pgid) enumerates members, skips those with payload=None (zombies). waitpid(-pgid) enumerates members, selects those with payload=None. Both under epoch guard, re-validating pgrp binding per BINDING_v1 §6.2.

### 8.6 Thread-group leader role

<!-- txdoc:PROCESS-THREAD-GROUP-LEADER-ROLE-1 -->

v1 stance:
- Leader = the thread whose tid equals the process's pid.
- Identified by structural property (tid == pid), not a distinguished field.
- Leader-exit-with-survivors supported (§6).
- Non-leader execve NOT supported in v1 (returns ENOSYS).

When non-leader exec is added in Phase 2, we'll revisit whether to rename the surviving thread's tid or accept divergence from Linux. Current v1 sidesteps the question.

---

## 9. PidNamespace, PidName, and pid allocation

<!-- txdoc:PROCESS-PIDNAMESPACE-PIDNAME-PID-ALLOCATION-1 -->

Per `NAMESPACE_VIEW_v1`, `PidNamespace` owns numeric signifier bindings for
pid, tid, pgid, and sid lookup. The canonical PROCESS graph owns parentage,
pgrp membership, session membership, and thread membership.

```rust
pub struct PidNamespace {
    pub level: u8,
    pub parent: Option<Cap<PidNamespace>>,
    pub user_ns: Cap<UserNamespace>,
    pub numbers: AllocIndex<u32, Cap<PidName>>,     // shared pid/tid/pgid/sid number space
    pub init_proc: Option<Cap<ProcessIdentity>>,              // pid 1 in this ns
}

pub struct PidName {
    pub kind: PidKind,
    pub target: PidTarget,
    pub numbers: SmallVec<[NamespaceNumber; 1]>,    // v1 exactly one; nested namespaces extend this
    pub death_rule: PidNameDeathRule,
}

pub enum PidTarget {
    Process(Cap<ProcessIdentity>),
    Thread(Cap<ThreadIdentity>),
    ProcessGroup(Cap<ProcessGroup>),
    Session(Cap<Session>),
}

pub struct PidNameSnapshot {
    pub kind: PidKind,
    pub numbers: SmallVec<[NamespaceNumberValue; 1]>,
}
```

**Shared number space:** pid, tid, pgid, and sid all draw from the same namespace index. One numeric key resolves to one `PidName`; the `PidName.kind` determines whether it is being used as a process, thread, pgrp, or session signifier. When POSIX requires the same number to serve as pid/pgid/sid roles, represent that as a role-capable name rather than conflicting entries in separate maps.

**PidName vs PidStruct.** `PidStruct` may remain the implementation name, but architecturally it is a `PidName`: a namespace-owned numeric binding object that targets a canonical PROCESS identity. Target identities hold non-retaining `PidNameSnapshot` values only; they do not retain `Cap<PidName>`.

**v1 simplification:** one root `PidNamespace`; no nesting. `level = 0`, `parent = None`. Multi-namespace support (nested via CLONE_NEWPID) extends `PidName.numbers`; it does not change target entity identity.

**Allocation:**
- `numbers.reserve()` — reserves a free numeric slot and preallocates any index nodes needed for infallible commit.
- `numbers.reserve_at(i)` — reserves a specific numeric slot for POSIX role reuse or future tid-preservation cases.
- On commit (phase 4), the reservation publishes `nr -> Cap<PidName>`.
- On drop of an uncommitted reservation, the slot reservation rolls back.

`AllocIndex` is the namespace-number reservation and publication surface, not the VM page-cache XArray. `BITMAP_RESERVATION_v1` remains the general bitmap reservation primitive and may be used inside an `AllocIndex`; syscall-facing pid allocation should treat `PidNamespace.numbers` as the only surface it touches.

**Resolve/render:**

- Numeric syscall inputs resolve `nr + caller.nsproxy.pid_ns -> PidName -> canonical target`.
- Semantic checks and mutations use canonical PROCESS identities and DLLs.
- Return values render canonical targets through the caller or projection pid namespace.
- Visibility holes are syscall/projection policy; they do not rewrite canonical topology.

---

## 10. Projections for procfs

<!-- txdoc:PROCESS-PROJECTIONS-FOR-PROCFS-1 -->

Procfs renders process state via projections. Readers under epoch guards; no locks.

**Per-process:**

- `/proc/<pid>/status` — pid, ppid (via parent binding), tgid (= pid for leader), state (running/sleeping/stopped/zombie), thread count, uid/gid (via cred), memory (via VM), etc.
- `/proc/<pid>/stat` — machine-readable form of status.
- `/proc/<pid>/cmdline` — process command line (read from user memory in argv region; VFS concern).
- `/proc/<pid>/environ` — environment variables (similarly).
- `/proc/<pid>/cwd` — symlink to working directory (via Frame.cwd).
- `/proc/<pid>/exe` — symlink to executed binary (written at execve).
- `/proc/<pid>/fd/<n>` — file descriptors (via Frame.fd_table; VFS concern).
- `/proc/<pid>/task/<tid>/*` — per-thread equivalents.
- `/proc/<pid>/children` — children pids (walks parent.children DLL, filters/renders through the procfs pid namespace view).

**System-wide:**

- `/proc/<pid>` enumeration — walks `PidNamespace.numbers` for the procfs pid namespace view, selecting process-kind names.
- `/proc/<pid>/task` enumeration — walks ProcessPayload.threads DLL.

**Consistency:**

- Per-field reads are atomic (underlying atomics on individual fields).
- Cross-field not atomic (e.g., thread count and thread list may be briefly inconsistent during thread creation/exit).
- Acceptable: procfs is advisory for dynamic state. Linux has identical behavior.

---

## 11. POSIX alignment

<!-- txdoc:PROCESS-POSIX-ALIGNMENT-1 -->

### 11.1 Committed (v1)

<!-- txdoc:PROCESS-COMMITTED-V1-1 -->

- **fork** (via clone with no flags + SIGCHLD) — create new process.
- **clone** for threads (CLONE_VM | CLONE_FILES | CLONE_FS | CLONE_SIGHAND | CLONE_THREAD + CLONE_SETTLS + CLONE_CHILD_CLEARTID + CLONE_PARENT_SETTID + CLONE_CHILD_SETTID).
- **execve** for static ELF binaries from the thread-group leader.
- **exit, exit_group** — ordinary single-thread exit and group cascade.
- **wait, waitpid, waitid, wait4** with options WEXITED, WSTOPPED, WNOHANG — reap children.
- **kill, tkill, tgkill, killpg** — signal delivery.
- **setsid, setpgid, getpgid, getsid, getpgrp** — session/pgroup.
- **getpid, getppid, gettid** — identity reads.
- **set_tid_address** — thread exit notification registration.
- **Leader-exit-with-survivors** — leader exits; siblings continue; process exits when last thread exits.
- **Single pid namespace** — root namespace only.
- **Reparenting to init** — orphan children reparent to pid 1.
- **SIGCHLD to parent on process exit** — standard notification.
- **Orphan pgroup SIGHUP/SIGCONT** — POSIX job-control rules.
- **Session leader death** — tty detachment, SIGHUP to foreground pgroup.

### 11.2 Deferred

<!-- txdoc:PROCESS-DEFERRED-1 -->

Explicit scope cuts:

- **Non-leader execve.** Exec from a non-leader thread in a multi-threaded process returns ENOSYS in v1. Non-leader exec requires tid-rename semantics which need more design.
- **Nested pid namespaces.** CLONE_NEWPID, setns for pid namespace. Requires multi-level `PidName` resolution, `pid_for_children`, and namespace-scoped init processes.
- **CHILD_SUBREAPER** (prctl PR_SET_CHILD_SUBREAPER). Linux 3.4 feature; outside 2.6 parity.
- **vfork** (CLONE_VFORK rendezvous). Rare in practice; programs that need it can use fork + exec.
- **Linux-specific wait flags** WCONTINUED, __WALL, __WCLONE.
- **pidfd:** pidfd_open, pidfd_send_signal, and signalfd/pidfd as bus-subscriber fd adapters are committed in SIGNAL_v1 Phase 1. CLONE_PIDFD in clone3 is Phase 2 (requires clone3 syscall work; Phase 1's clone covers the flag combinations needed).
- **prctl** (most operations). Specific operations added as needed.
- **Scheduler policy** (nice, setpriority, sched_*). Scheduler policy is REACTOR_v0 non-goal.
- **Credential changes via setuid/setgid/setresuid/etc.** Phase 2 cred spec.
- **Capability changes via capset.** Phase 2.
- **Resource-limit mutations via setrlimit for non-self processes.** Phase 2.
- **Full ptrace.** Observation subsystem.

### 11.3 Readers must not assume

<!-- txdoc:PROCESS-READERS-MUST-NOT-ASSUME-1 -->

For anything not in §11.1:

- The behavior is not committed.
- Code relying on deferred features should fail gracefully or check for availability.
- Deferred features will be added in subsequent specs; they will be clearly versioned.

---

## 12. Cross-doc corrections

<!-- txdoc:PROCESS-CROSS-DOC-CORRECTIONS-1 -->

This section summarizes corrections to prior docs made necessary by PROCESS_v1's pinned decisions.

### 12.1 Credential service draft

<!-- txdoc:PROCESS-CREDENTIAL-SERVICE-DRAFT-1 -->

**Correction:** `ProcessPolicy.signal_mask: SigMask` is incorrect. Credential service policy must not include it.

**Explanation:** signal_mask is per-thread (lives on `ThreadPayload` per THREAD_RUNTIME_v1 §5.1); it was never a process-level concern. The field in older credential sketches was a placeholder from pre-thread-runtime factoring.

**When applied:** credential service implementation.

### 12.2 Archived architecture drafts

<!-- txdoc:PROCESS-ARCHIVED-ARCHITECTURE-DRAFTS-1 -->

**Correction:** older `PolicyBag` sketches showed `signal_mask` alongside `cred` and `rlimits` — same issue as the credential service draft.

**When applied:** no active implementation doc should place `signal_mask` on process policy.

### 12.3 THREAD_RUNTIME_v1 §5.6

<!-- txdoc:PROCESS-THREAD-RUNTIME-V1-5-6-1 -->

**Note:** process-directed signal routing discussion (§5.6) names "step_post_process_signal" and "step_post_group_signal" in its prose. These functions have since been repositioned: the canonical producer entry point is `deliver_posix_signal` per SIGNAL_v1 §12; PROCESS_v1 §7.6's `post_to_group_pending` and `process_group_fanout` are internal helpers invoked by `deliver_posix_signal` for Process/ProcessGroup target kinds. THREAD_RUNTIME v1.5 has been updated to reflect this; no PROCESS_v1 correction needed.

---

## 13. What this document does not specify

<!-- txdoc:PROCESS-WHAT-THIS-DOCUMENT-DOES-NOT-SPECIFY-1 -->

- **Concurrency implementation details.** See BINDING_v1 and object_model_v2.
- **Thread execution semantics.** See THREAD_RUNTIME_v1.
- **Signal delivery mechanism.** Two-site discipline in THREAD_RUNTIME_v1 §5.4.
- **Full signal disposition catalog.** Signal spec (future).
- **ptrace mechanics.** Observation subsystem (future).
- **tty mechanics.** TTY_v1 (future); this doc describes process's participation (session, foreground pgroup).
- **VFS / fd / mount / path resolution.** VFS subsystem.
- **VM / mmap / fault handling.** VM_v1_2.
- **Cred specifics.** credential service spec.
- **Rlim specifics.** rlim v1.1.
- **Scheduler policy.** Deferred per REACTOR_v0.
- **ELF loader details.** VFS + execve internals; separable.
- **procfs rendering format.** Procfs subsystem.

---

## 14. Open questions

<!-- txdoc:PROCESS-OPEN-QUESTIONS-1 -->

- **pgroup/session reclamation precise timing.** When is a pgroup's last Cap released? Members hold Caps via their pgrp binding; session holds Cap via session.members DLL entry. Refcount-driven; specific release ordering during process exit cascade needs careful tracing. Not blocking v1.
- **Race between setpgid and process exit.** Process P is exiting; simultaneously another thread calls setpgid(P's visible pid, new_pgid). Depending on ordering: setpgid sees P with payload=None (zombie), should fail with ESRCH; or sees P live and succeeds, but moves a dying process. Both outcomes acceptable. Need to confirm in step_setpgid's observe phase.
- **Whether CLONE_SIGHAND without CLONE_VM is rejected or weirdly allowed.** Linux rejects. We follow suit. Documented in clone flag support table (§7.1.1).
- **How procfs renders a process during GroupExit collapse.** Transient state (threads decreasing). Currently assumed "rendered normally with current thread count." Probably fine; not a correctness question.
- **Non-leader execve design for Phase 2.** tid-rename vs accept-divergence (option P vs Q from prior discussions). Defer decision to Phase 2.

---

## Short version

<!-- txdoc:PROCESS-SHORT-VERSION-1 -->

> A process is ProcessIdentity + ProcessPayload. Identity carries pid, parent binding, pgrp binding, children DLL, exit_status; persists through zombie. Payload carries threads DLL, Frame (vm/fd_table/sig_actions/fs_context as Shared<T>), ProcessPolicy (cred + rlimits), group_pending signals, group_exit coordination, leader_exit_status; dropped at process exit. ProcessGroup and Session are identity-only entities with member DLLs. Operations follow the class-1 authoritative-binding pattern (BINDING_v1): upward-pointer CAS is the linearization; downward DLLs are materializations. Fork/clone/exec/exit/wait use reservations with drop-rollback; commit phases are infallible and cross-index atomicity is class-3 compositional per SUBSYSTEM_ANATOMY §3.6. GroupExit coordinates thread-group collapse for exit_group and (leader-only in v1) execve. Leader-exit-with-survivors records the leader's status separately; priority rule determines process-visible status at the last-thread-exit. Signal routing (per SIGNAL_v1 §12) dispatches process-level Gewalt ops as fan-outs over thread-level primitives; the process is the dispatcher, not the semantic unit. Non-leader exec, nested pid namespaces, CHILD_SUBREAPER, ptrace, and full cred mutation are deferred.
