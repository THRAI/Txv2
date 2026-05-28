# Namespace Views and NsProxy -- v1

<!-- txdoc:00-META-FRAMEWORK-NAMESPACE-VIEW-V1 -->

## Status

<!-- txdoc:NAMESPACE-VIEW-STATUS-1 -->

Draft v1.

This document records the namespace/view decision that falls out of
`PROCESS_v1`, `OBJECT_PATTERN_FIXES_v1`, `INVARIANTS_v4`, and the projected
`RNode` model in `PAGE_BACKED_v1`.

## Purpose

<!-- txdoc:NAMESPACE-VIEW-PURPOSE-1 -->

Linux-compatible namespaces require user-visible names, roots, ids, offsets,
and authority checks to vary by caller. They do not require every semantic
object graph to be cloned per namespace.

The txKernel rule is:

```text
NsProxy chooses the lens.
Namespace objects own their own signifier bindings and local registries.
Canonical subsystems own semantic identity, topology, lifetime, and mutation.
Payloads own live operational state.
RNode projection exposes views through VFS/procfs/sysfs only.
```

## 1. Core placement

<!-- txdoc:NAMESPACE-VIEW-CORE-PLACEMENT-1 -->

`NsProxy` is execution-context state. It is not a process tree, not a mount
tree, and not a semantic owner for the objects exposed through its namespaces.

```rust
pub struct NsProxy {
    pub pid_ns: Cap<PidNamespace>,
    pub pid_for_children: Cap<PidNamespace>,
    pub mnt_ns: Cap<MountNamespace>,
    pub user_ns: Cap<UserNamespace>,        // Tx subject-authority mirror; see below
    pub cgroup_ns: Cap<CgroupNamespace>,
    pub uts_ns: Cap<UtsNamespace>,
    pub ipc_ns: Cap<IpcNamespace>,
    pub net_ns: Cap<NetNamespace>,
    pub time_ns: Cap<TimeNamespace>,
    pub time_ns_for_children: Cap<TimeNamespace>,
}
```

The v1 placement recommendation is `ProcessPayload.nsproxy`. If txKernel later
supports mixed namespace membership among threads in the same process, move or
mirror the active pointer to `ThreadPayload`; the model does not otherwise
change.

```rust
pub struct ProcessPayload {
    pub threads: DllContainer<ThreadIdentity>,
    pub frame: Frame,
    pub policy: ProcessPolicy,
    pub nsproxy: Cap<NsProxy>,
    ...
}
```

`NsProxy` is immutable after publication. `clone`, `unshare`, and `setns`
publish a new bundle or reuse an existing compatible one.

**Linux-conformance note.** Linux's `struct nsproxy` contains UTS, IPC, mount,
`pid_ns_for_children`, network, active time, time-for-children, and cgroup
namespace references. Linux does not store the current user namespace in
`nsproxy`; it stores it in `cred.user_ns`. Tx may keep `user_ns` in `NsProxy` as
a syscall-facing mirror for resolve/render convenience, but the authoritative
capability lens is the current `Cred` / `SubjectAuthority`. Any implementation
that stores both must maintain this invariant:

```text
current_subject_authority.user_ns == current_nsproxy.user_ns
```

unless a step is explicitly constructing a new credential and namespace bundle
before the point-of-no-return commit. A process does not become globally
privileged by entering a child user namespace. It gains capabilities only in
that user namespace, and operations on another namespace or object must check
the user namespace that owns that operation's authority domain.

## 2. Process canonical topology

<!-- txdoc:NAMESPACE-VIEW-PROCESS-CANONICAL-TOPOLOGY-1 -->

The PROCESS identity graph remains canonical:

```text
ProcessIdentity.parent
ProcessIdentity.children
ProcessIdentity.pgrp
ProcessGroup.members
ProcessGroup.session
Session.members
ProcessPayload.threads
```

Namespaces do not create per-namespace copies of this graph. In particular:

- no per-pid-namespace `parent.children` DLL;
- no per-pid-namespace `ProcessGroup.members` DLL;
- no per-pid-namespace `Session.members` DLL;
- no `nsproxy`-owned process, pgrp, session, or thread shadow objects.

The namespace layer owns name/addressability. The process subsystem owns
semantic continuity. Payloads own live resources and execution state.

## 3. Pid namespace and pid names

<!-- txdoc:NAMESPACE-VIEW-PID-NAMESPACE-NAMES-1 -->

Replace the four direct maps in current `PROCESS_v1`:

```rust
pid_map:  nr -> Cap<ProcessIdentity>
tid_map:  nr -> Cap<ThreadIdentity>
pgid_map: nr -> Cap<ProcessGroup>
sid_map:  nr -> Cap<Session>
```

with one shared-number-space index:

```rust
pub struct PidNamespace {
    pub level: u8,
    pub parent: Option<Cap<PidNamespace>>,
    pub user_ns: Cap<UserNamespace>,
    pub numbers: AllocIndex<u32, Cap<PidName>>,
    pub init_proc: Option<Cap<ProcessIdentity>>,
}

pub struct PidName {
    pub kind: PidKind,
    pub target: PidTarget,
    pub numbers: SmallVec<[NamespaceNumber; 2]>,
    pub death_rule: PidNameDeathRule,
}

pub enum PidKind {
    Process,
    Thread,
    ProcessGroup,
    Session,
}

pub enum PidTarget {
    Process(Cap<ProcessIdentity>),
    Thread(Cap<ThreadIdentity>),
    ProcessGroup(Cap<ProcessGroup>),
    Session(Cap<Session>),
}

pub struct NamespaceNumber {
    pub ns: Weak<PidNamespace>,
    pub nr: u32,
}
```

`AllocIndex` is the namespace-number publication surface: lookup, reservation,
commit, withdrawal, and iteration over pid-like signifiers. It deliberately does
not commit this layer to the VM page-cache XArray shape. A simple bitmap-backed
or ordered-map-backed implementation is acceptable as long as `reserve()` and
`reserve_at()` precondition commit so phase 4 cannot fail.

Target identities may carry non-retaining name snapshots for fast rendering,
but they must not retain `Cap<PidName>`.

## 4. Resolve algorithm

<!-- txdoc:NAMESPACE-VIEW-RESOLVE-ALGORITHM-1 -->

Resolution converts a caller-visible number into a canonical target. It does
not perform the semantic operation.

```rust
fn resolve_pid_name(
    viewer_ns: &PidNamespace,
    nr: u32,
    expected: ExpectedPidKind,
    guard: &Guard,
) -> Result<Cap<PidName>, Errno> {
    if nr == 0 {
        return Err(EINVAL);
    }

    let name = viewer_ns
        .numbers
        .load(nr, guard)
        .ok_or(ESRCH)?;

    if !expected.accepts(name.kind) {
        return Err(ESRCH);
    }

    if !name_has_number_in(&name, viewer_ns) {
        return Err(ESRCH);
    }

    if !name_is_alive_for_lookup(&name, expected, guard) {
        return Err(ESRCH);
    }

    Ok(name.upgrade_cap()?)
}
```

Use operation-specific wrappers rather than one universal process resolver:

```rust
resolve_signal_process(pid)
resolve_wait_process(pid)
resolve_thread_tid(tid)
resolve_pgrp(pgid)
resolve_session(sid)
```

The liveness rule is operation-specific:

```text
signal process lookup: payload must be present
wait process lookup: zombie identity is valid
thread lookup: thread payload must be present unless the operation is join-like
pgrp lookup: canonical group identity must still be addressable
session lookup: canonical session identity must still be addressable
```

Special syscall argument meanings are parsed before ordinary lookup:

```text
kill(pid > 0):  resolve process
kill(pid == 0): current canonical pgrp
kill(pid == -1): broadcast over visible/authorized processes
kill(pid < -1): resolve pgrp -pid

waitpid(pid > 0): resolve process, then verify canonical child relation
waitpid(pid == 0): current canonical pgrp
waitpid(pid == -1): any canonical child
waitpid(pid < -1): resolve pgrp -pid
```

## 5. Render algorithm

<!-- txdoc:NAMESPACE-VIEW-RENDER-ALGORITHM-1 -->

Rendering converts a canonical target back into a viewer-visible signifier.

```rust
fn render_pid_name(
    viewer_ns: &PidNamespace,
    target: &PidTarget,
    kind: PidKind,
) -> Option<u32> {
    if let Some(snapshot) = target.name_snapshot(kind) {
        if let Some(nr) = snapshot.number_in(viewer_ns) {
            return Some(nr);
        }
    }

    viewer_ns.numbers.find_by_target(target, kind)
}
```

If rendering fails, the syscall or projection chooses an explicit visibility
policy: return `0`, return `ESRCH`, omit the entry, or render a namespace root
boundary. The view layer must not rewrite canonical topology to hide a
visibility hole.

## 6. Syscall pipeline

<!-- txdoc:NAMESPACE-VIEW-SYSCALL-PIPELINE-1 -->

All namespace-aware syscalls use this shape:

```text
syscall args
    -> resolve through caller.nsproxy
    -> canonical semantic operation
    -> render result through caller/requested namespace
```

Examples:

```text
kill(42):
  caller.nsproxy.pid_ns resolves 42 -> ProcessIdentity
  deliver_posix_signal(ProcessIdentity)

kill(-7):
  caller.nsproxy.pid_ns resolves 7 -> ProcessGroup
  walk canonical ProcessGroup.members

waitpid(42):
  caller.nsproxy.pid_ns resolves 42 -> ProcessIdentity
  verify target is in caller.children
  reap canonical identity when eligible
  withdraw pid name at reap

getsid(pid):
  resolve pid -> ProcessIdentity
  read canonical ProcessIdentity.pgrp -> ProcessGroup.session
  render Session through caller.nsproxy.pid_ns

setpgid(pid, pgid):
  resolve target process and target/create pgrp through caller pid namespace
  mutate canonical ProcessIdentity.pgrp and ProcessGroup.members

setsid():
  create canonical Session and ProcessGroup
  publish sid/pgid PidName entries
  move caller into the new canonical pgrp/session
```

Path syscalls use `mnt_ns` for path resolution and then operate on canonical
VFS/Mount/DEntry/RNode objects. `chroot` and `chdir` mutate the caller's
filesystem context lens, not the canonical filesystem graph.

### 6.1 User namespace authority and id maps

<!-- txdoc:NAMESPACE-VIEW-USERNS-AUTHORITY-1 -->

Linux user namespaces are the authority half of the namespace-view model:

```rust
pub struct UserNamespace {
    pub parent: Option<Cap<UserNamespace>>,
    pub owner_uid: KernelUid,
    pub owner_gid: KernelGid,
    pub uid_map: IdMap,
    pub gid_map: IdMap,
    pub uid_map_written: bool,
    pub gid_map_written: bool,
    pub setgroups: SetgroupsPolicy,
}
```

The exact implementation must factor authority through `Cred` /
`SubjectAuthority`. `NsProxy.user_ns`, when present, is a mirror/cache of the
current subject's credential namespace for namespace-view code; it is not a
second source of truth. The semantics are:

- every subject is a member of exactly one user namespace via its credential;
- `ProcessPayload.nsproxy.user_ns` may mirror that credential user namespace for
  resolve/render helpers, and must be kept consistent at commit boundaries;
- `clone(CLONE_NEWUSER)` creates a child in a new user namespace;
- `unshare(CLONE_NEWUSER)` moves the caller into a new user namespace;
- the child/caller receives a full capability set in the new user namespace
  only, not in the parent namespace;
- a newly created user namespace starts with empty `uid_map` and `gid_map`;
- the initial user namespace has the Linux dummy identity map
  `0 0 4294967295`;
- `/proc/<pid>/uid_map` and `/proc/<pid>/gid_map` are write-once projected
  files that update user-namespace map state, not process credential objects;
- `/proc/<pid>/setgroups` gates the unprivileged `gid_map` path and cannot be
  changed after `gid_map` is written;
- procfs map/setgroups writes validate the writer's open-file/current subject
  authority against the target `UserNamespace`; they must not authorize by
  borrowing the target process's own credential.

All non-user namespaces carry the immutable user namespace that owned them at
creation:

```rust
pub struct NetNamespace {
    pub user_ns: Cap<UserNamespace>,
    ...
}
```

This applies equally to pid, mount, UTS, IPC, cgroup, network, and time
namespaces. Privileged operations on resources governed by one of these
namespaces check the caller's capability in that namespace's owning
`UserNamespace`.

If `CLONE_NEWUSER` is combined with other `CLONE_NEW*` flags, Linux creates the
new user namespace first. The remaining namespaces are then created as owned by
that new user namespace, which lets an unprivileged caller perform
`unshare(CLONE_NEWUSER | CLONE_NEWNET)` without first holding `CAP_SYS_ADMIN`
in the parent user namespace.

Linux also treats user namespace creation as a credential transition. For
`clone(CLONE_NEWUSER)` and `unshare(CLONE_NEWUSER)`, Tx must construct the new
credential/subject authority first, then create any non-user namespaces against
that authority, and finally publish the credential and namespace bundle
together at the point of no return. Partial publication of `Cred` without the
matching namespace bundle, or vice versa, is forbidden.

## 7. Namespace-changing syscalls

<!-- txdoc:NAMESPACE-VIEW-CHANGING-SYSCALLS-1 -->

`clone`, `unshare`, and `setns` publish a new immutable `NsProxy` and, when
`CLONE_NEWUSER` is involved, a matching credential/subject-authority cap. Linux
validates the full requested namespace set before committing; Tx steps must do
the same observe -> reserve -> validate -> commit split.

```text
unshare(CLONE_NEWUTS):
  create/copy UtsNamespace
  create NsProxy with uts_ns swapped
  commit ProcessPayload.nsproxy old -> new

unshare(CLONE_NEWUSER):
  require caller effective uid/gid mapped in current user_ns
  require single-threaded process for Linux compatibility
  unshare filesystem root/cwd context as Linux does for CLONE_NEWUSER
  create new Cred / SubjectAuthority with user_ns swapped
  create UserNamespace with empty uid/gid maps and inherited setgroups policy
  grant full capabilities in the new user namespace only
  create NsProxy with user_ns mirror swapped
  commit Cred and ProcessPayload.nsproxy old -> new together

unshare(CLONE_NEWUSER | CLONE_NEWNET):
  create UserNamespace / Cred first
  authorize CLONE_NEWNET against the new user namespace
  create NetNamespace owned by the new user namespace
  create NsProxy with user_ns mirror and net_ns swapped
  commit Cred and ProcessPayload.nsproxy old -> new together

unshare(CLONE_NEWNS):
  unshare filesystem root/cwd context as Linux does for CLONE_NEWNS
  create/copy MountNamespace owned by the caller's current user namespace
  create NsProxy with mnt_ns swapped
  commit fs context and ProcessPayload.nsproxy old -> new together

setns(net_fd):
  validate namespace fd and permissions
  require CAP_SYS_ADMIN in both the target net namespace's owning user_ns
  and the caller credential's current user_ns
  create NsProxy with net_ns swapped
  commit ProcessPayload.nsproxy old -> new

setns(mnt_fd):
  validate namespace fd and permissions
  require CAP_SYS_ADMIN in target mnt_ns.user_ns
  require CAP_SYS_ADMIN and CAP_SYS_CHROOT in caller credential user_ns
  install compatible root/cwd view and create NsProxy with mnt_ns swapped
  commit fs context and ProcessPayload.nsproxy old -> new together

setns(user_fd):
  reject re-entering the same user namespace
  require single-threaded process and unshared filesystem context
  require CAP_SYS_ADMIN in the target user namespace
  create new Cred / SubjectAuthority and NsProxy user_ns mirror
  commit Cred and ProcessPayload.nsproxy old -> new together

setns(pidfd, flags):
  snapshot the target task's namespace set
  validate every namespace selected by flags before committing any of them
  commit Cred, fs context, and ProcessPayload.nsproxy as one publication step
```

PID namespaces are special:

```text
unshare(CLONE_NEWPID) / setns(pid_ns_fd):
  current pid_ns is unchanged
  pid_for_children is changed

next fork/clone:
  child pid_ns = parent.nsproxy.pid_for_children
  first child in a new pid namespace becomes init_proc / pid 1 there
```

Active PID namespace is derived from the task's `PidName` / pid object, not
from `pid_for_children`. `pid_for_children` controls future children only.
Linux follows this split with `task_active_pid_ns(task)` for rendering the
current task's pid namespace and `nsproxy.pid_ns_for_children` for the namespace
used by the next fork/clone. Tx must preserve this distinction.

Time namespaces follow the same active-vs-for-children shape:

```text
unshare(CLONE_NEWTIME) / setns(time_ns_fd):
  current time_ns is unchanged
  time_ns_for_children is changed

next fork/clone or exec transition:
  child/execed task observes the selected time namespace according to Linux
  time namespace rules
```

## 8. Commit discipline

<!-- txdoc:NAMESPACE-VIEW-COMMIT-DISCIPLINE-1 -->

Namespace publication and withdrawal are explicit commit surfaces.

```text
namespace commits:
  PidNamespace.numbers[nr] -> PidName
  namespace-root/lens swaps
  namespace-local registry insertions

canonical topology commits:
  parent.children
  ProcessIdentity.parent
  ProcessIdentity.pgrp
  ProcessGroup.members
  ProcessGroup.session
  Session.members

payload/operational commits:
  ProcessIdentity.payload Some/None
  ProcessPayload.threads
  signal queues
  group_exit state

context/view commits:
  ProcessPayload.nsproxy old -> new
  FsContext cwd/root old -> new
```

The step model remains unchanged: observe, upgrade, reserve, commit, publish.
Each namespace commit uses the same substrate reservation/commit discipline as
other indexes.

### 8.1 Fork/clone

<!-- txdoc:NAMESPACE-VIEW-FORK-CLONE-1 -->

Reserve:

```text
pid numbers in child pid namespace and visible ancestors
PidName slots
ProcessIdentity / ProcessPayload slots
ThreadIdentity / ThreadPayload slots
parent.children insertion
pgrp.members insertion
payload.threads insertion
namespace index slots
```

Commit:

```text
initialize identities and payloads
commit namespace index slots -> PidName
commit parent binding and parent.children insertion
commit pgrp binding and pgrp.members insertion
commit payload.threads insertion
publish trace/wake events
```

Any object visible through `PidNamespace.numbers` must already point to a fully
initialized target identity.

### 8.2 Exit and reap

<!-- txdoc:NAMESPACE-VIEW-EXIT-REAP-1 -->

Process exit drops payload but keeps the process name:

```text
process_exit:
  commit payload Some -> None
  keep ProcessIdentity
  keep PidName
  keep parent.children
  wake parent
```

Reap withdraws the name:

```text
wait/reap:
  withdraw PidNamespace.numbers[pid] in all namespaces named by PidName
  withdraw parent.children
  withdraw pgrp.members if zombies remain in pgrp until reap
  drop final ProcessIdentity retention
```

### 8.3 setpgid and setsid

<!-- txdoc:NAMESPACE-VIEW-SETPGID-SETSID-1 -->

Joining an existing pgrp mutates canonical topology:

```text
resolve pgid in caller pid namespace -> ProcessGroup
structural_move target.pgrp old -> new
commit old_pgrp.members withdraw
commit new_pgrp.members insert
```

Creating a pgrp or session creates canonical identities and namespace names:

```text
reserve canonical ProcessGroup / Session
reserve PidName and namespace slot
commit canonical identity
commit namespace number -> PidName
commit ProcessGroup.session / Session.members
commit caller pgrp move
```

If one numeric value must serve as both sid and pgid in the same namespace,
represent that as one number entry with a role-capable `PidName`, not as two
conflicting entries at the same key.

## 9. RNode projection

<!-- txdoc:NAMESPACE-VIEW-RNODE-PROJECTION-1 -->

`RNode` projection is the file-shaped adapter for namespace views. It is not
the general namespace mechanism.

```text
kill(42):
  PidNamespace resolution only; no RNode

/proc/42/status:
  Projected RNode -> pid namespace view -> ProcessIdentity -> bytes

/proc/self/ns/pid:
  namespace-handle RNode retaining PidNamespace

/proc/self/mountinfo:
  Projected RNode over MountNamespace view

/proc/self/cgroup:
  Projected RNode over CgroupNamespace root lens

/proc/self/uid_map, /proc/self/gid_map, /proc/self/setgroups:
  Projected RNode -> ProcessIdentity -> target UserNamespace
  -> validate writer Cred / SubjectAuthority against Linux map/setgroups rules
  -> update UserNamespace state
```

Projected RNodes may retain namespace objects or carry projection keys. They do
not own the semantic object graph they render.

For user namespace map files, Linux write rules are part of the projection
contract:

- writes occur at offset 0, contain at least one line, fit in one page, and use
  valid numeric `inside outside length` fields with nonzero length;
- ranges may not overlap;
- malformed map shape fails with `EINVAL`;
- repeated writes or permission failures fail with `EPERM`;
- the unprivileged `gid_map` path requires `/proc/<pid>/setgroups` to be set to
  `deny` first;
- `setgroups` may be changed only before `gid_map` is written, and `deny`
  cannot be reverted to `allow`.

## 10. Ownership by namespace kind

<!-- txdoc:NAMESPACE-VIEW-OWNERSHIP-BY-KIND-1 -->

Every non-user namespace also carries the immutable `UserNamespace` that owned
it at creation; the table below names the domain-specific state in addition to
that owner link.

| Namespace kind | Owns | Does not own |
|---|---|---|
| `PidNamespace` | pid/tid/pgid/sid numeric bindings and visibility | process tree, pgrp/session membership, thread roster |
| `MountNamespace` | visible mount topology/rooting and propagation state | file payloads, RNode semantics, filesystem driver state |
| `UserNamespace` | uid/gid maps, setgroups policy, namespace-relative capability interpretation | process ownership tree, unrelated namespace topology, or credential object lifetime |
| `CgroupNamespace` | root-relative cgroup rendering lens | cgroup hierarchy or controller state |
| `UtsNamespace` | hostname/domain values | process or network topology |
| `IpcNamespace` | IPC registry domain | unrelated process topology |
| `NetNamespace` | network registry/stack domain and immutable owning `UserNamespace` link | process/session topology or user credential lifetime |
| `TimeNamespace` | clock offsets/rendering plus active/for-children lensing | timer object graph unless timers are semantically allocated there |

## 11. Blast radius estimate

<!-- txdoc:NAMESPACE-VIEW-BLAST-RADIUS-1 -->

This is a medium-to-large architecture change because it touches syscall entry,
process naming, procfs projection, and namespace-changing operations. It is
mostly mechanical once the data model is accepted.

| Area | Impact | Required change |
|---|---:|---|
| PROCESS entity docs | High | Replace direct pid/tid/pgid/sid maps with `PidNamespace.numbers -> PidName`; add `NsProxy` placement; update fork/clone/exit/wait/setpgid/setsid algorithms. |
| Syscall scripts | High | Add `SysCtx`; route pid/path/cgroup/time signifiers through `ctx.nsproxy`, route authority through `Cred` / `SubjectAuthority`, and split resolve/operate/render phases. |
| PID allocation substrate | Medium | Add `AllocIndex` or extend index substrate with namespace-number reservation, ordered iteration, and withdrawal. Do not require the VM page-cache XArray implementation here. |
| PROCESS commit steps | High | Add namespace index reservations/commits/withdrawals; preserve pid names through zombie; withdraw at reap. |
| THREAD runtime | Medium | Decide whether `nsproxy` is process-payload scoped or thread-payload scoped; update `gettid`, thread creation, and thread exit name withdrawal. |
| VFS path resolution | Medium | Route path resolution through `ctx.nsproxy.mnt_ns`; keep canonical VFS operations unchanged after resolution. |
| procfs/sysfs | High | Carry a projection view; enumerate `PidNamespace.numbers`; projected RNodes re-read canonical state and render through the view. |
| TTY/job control | Medium | Resolve pgid/sid numbers through pid namespace; bind TTY to canonical `ProcessGroup`/`Session`, not leader-process caps or numbers. |
| Cred/User namespace | High | Make `Cred.user_ns` / `SubjectAuthority.user_ns` the authority source of truth; keep any `NsProxy.user_ns` mirror consistent; add namespace-relative capability checks, uid/gid rendering, procfs map writes, setgroups gating, and non-user namespace ownership. |
| Mount namespace | Medium | Clarify which mount topology is namespace-owned and which lower VFS objects remain shared. |
| Cgroup/time namespaces | Low to Medium | Mostly projection/root/offset rendering unless deeper namespace-local allocation is added. |
| Invariants/lints | Medium | Enforce no direct pid maps, no namespace-owned shadow process topology, no resolver helpers mutating canon, no target identities retaining `PidName`. |
| Tests/model checks | High | Add resolve/render tests, zombie name lifetime tests, nested namespace allocation tests, setns/unshare tests, procfs visibility tests, userns map errno tests, and unprivileged `CLONE_NEWUSER | CLONE_NEWNET` authorization tests. |

Recommended migration order:

1. Introduce `NsProxy` and `SysCtx` while all fields point at init namespaces.
2. Add `UserNamespace` identity/map state, namespace-local capability checks,
   and owning-userns links on non-user namespaces.
3. Add `PidName` and `PidNamespace.numbers` behind compatibility wrappers.
4. Convert syscall pid helpers to operation-specific resolve/render APIs.
5. Convert fork/clone/exit/reap commits to publish/withdraw `PidName`.
6. Convert procfs projections to use the pid namespace view and userns map
   projections.
7. Remove direct `pid_map`, `tid_map`, `pgid_map`, and `sid_map`.
8. Add nested pid namespace semantics and `pid_for_children`.
9. Extend the same view discipline to mount, cgroup, time, ipc, net, and uts namespaces.

## 12. Review invariants

<!-- txdoc:NAMESPACE-VIEW-REVIEW-INVARIANTS-1 -->

The following invariants should be treated as review blockers:

```text
NSPROXY-1. NsProxy is an immutable bundle of namespace references.
It does not own the resources exposed through those namespaces.

NSPROXY-2. User namespace authority is credential-owned. If an implementation
stores `user_ns` in `NsProxy`, it is a mirror of `Cred` / `SubjectAuthority`,
not an independent authority source.

PIDNAME-1. PidNamespace owns pid/tid/pgid/sid numeric signifier bindings.
Canonical process/session/group/thread topology is owned by PROCESS.

PIDNAME-2. A namespace number resolves to PidName, then to a canonical target.
Syscalls must not use direct mixed target maps when PidName is active.

PIDNAME-3. Target identities may hold non-retaining name snapshots only.
They must not retain Cap<PidName>.

VIEWRES-1. Syscall code separates resolve, canonical operation, and render.
Resolver helpers must not mutate canonical topology.

VIEWRES-2. Visibility holes are projection policy.
They must not be repaired by rewriting canonical parent, pgrp, or session edges.

RNP-1. Projected RNodes expose namespace views through VFS.
They do not own the canonical object graph being rendered.

NSVIEW-4. Capability checks are namespace-relative.
Entering a child UserNamespace grants authority only in that user namespace,
never global authority in ancestors.

NSVIEW-5. Every non-user namespace records its immutable owning UserNamespace.
Privileged operations scoped by that namespace check capabilities in the owner.

NSVIEW-6. UserNamespace uid/gid maps and setgroups policy are namespace state
exposed through procfs projections. Map writes obey Linux write-once,
offset-zero, non-overlap, and setgroups-gated gid_map rules.
```
