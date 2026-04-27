# Object Pattern Fixes — v1

<!-- txdoc:00-META-FRAMEWORK-OBJECT-PATTERN-FIXES-V1 -->

**Status.** Companion to [`OBJECT_PATTERN_AUDIT_v1.md`](OBJECT_PATTERN_AUDIT_v1.md).

**Purpose.** Detail each `OPA-*` violation, explain why it violates the current object model, and propose concrete fixes before implementation.

---

## 1. OPA-1 — Stale PageBacked Backing Variants

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA1-PAGEBACKED-1 -->

**Violation.** [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) still contains older object spellings:

```rust
StructPayload::Tty(Cap<TtyData>)
StructPayload::CharDevice(Cap<CharDeviceBinding>)
PageContainerKind::Device { device: Cap<DevNode>, ... }
```

These violate the newer pattern in three ways:

- `TtyData` no longer exists as the terminal object. [`TTY.md`](../06_devices/TTY.md) split terminal identity from payload: RNodes should carry `Cap<TtyIdentity>`, and operations upgrade through `TtyIdentity.payload`.
- Tier-2 char devices are static board facts. [`DEVICE.md`](../06_devices/DEVICE.md) says `CharDeviceBinding` is `&'static`, not zone-allocated and not refcounted. A `Cap<CharDeviceBinding>` falsely implies reclaimable identity.
- `DevNode` is not a v1 device entity. Dynamic discovered devices are deferred; tier-2 device entries are static references or PageContainer-backed wrappers.

**Fix.** Update the canonical PageBacked shapes to:

```rust
pub enum StructPayload {
    Pipe(Cap<PipeIdentity>),
    Socket(Cap<SocketIdentity>),
    Tty(Cap<TtyIdentity>),
    EventFd(Cap<EventFdData>),
    TimerFd(Cap<TimerFdData>),
    SignalFd(Cap<SignalFdData>),
    Epoll(Cap<EpollData>),

    CharDevice(&'static CharDeviceBinding),
}

pub enum PageContainerKind {
    Anon { swap_policy: AnonSwapPolicy },
    File { fs: PayloadCap<MountPayload>, fs_object_id: FsObjectId },
    DeviceMmio {
        device: &'static PageBackedDeviceRegistration,
        base_ppn: PPN,
        page_count: u32,
    },
}
```

`object_model_v2.md` / `EBR_ZONE_INTERFACE_v1.md` name this as a static-reference carve-out:

> Static board/device records are not entities. They may appear inside backing variants as `&'static` references, but never as `Cap<T>`, `PayloadCap<T>`, or `Weak<T>`.

**Follow-up docs.**

- Patch [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2 and §3.2.
- Keep [`DEVICE.md`](../06_devices/DEVICE.md) as the authoritative source for tier-2 static references.

---

## 2. OPA-2 — Payload Evidence Spelling

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA2-PAYLOAD-EVIDENCE-1 -->

**Violation.** Several docs use `Cap<MountPayload>` as shorthand for operational access to a mount's filesystem instance. This muddies the reference hierarchy:

```text
Weak<T> -> IdentRef<'g, T> -> Cap<T> -> T::OperationalEvidence
```

For split entities, `Cap<Identity>` is identity retention. It must not imply payload retention. If docs casually use `Cap<Payload>`, reviewers cannot tell whether the reference is:

- a direct identity cap on a separately zone-allocated payload object;
- a `PayloadCap<Payload>` operational evidence handle;
- a typed contribution such as `MountPayloadPin`;
- an implementation spelling hidden behind the generic `OperationalEvidence` role.

Mount makes this acute because lazy umount intentionally separates:

```text
MountIdentity.namespace ⟂ MountPayload.payload
```

**Fix.** Use this canonical vocabulary:

```rust
Cap<TIdentity>                 // identity retention
PayloadCap<TPayload>           // generic payload retention
TIdentity::OperationalEvidence // operation-specific evidence, often PayloadCap or typed pin
MountPayloadPin                // typed contribution to MountPayload.payload
```

Direct prose `Cap<MountPayload>` should be replaced with one of:

- `PayloadCap<MountPayload>` when retaining the payload object itself;
- `MountPayloadPin` when contributing to the closed payload-pin accounting catalog;
- `MountIdentity::OperationalEvidence` when the exact pin type is intentionally abstract.

For PageBacked file content:

```rust
PageContainerKind::File {
    fs: PayloadCap<MountPayload>,
    fs_object_id: FsObjectId,
}
```

For VFS/open-file mount retention:

```rust
pub struct OpenFile {
    mount: Cap<MountIdentity>,
    mount_payload_pin: MountPayloadPin,
    rnode: Cap<RNode>,
}
```

**Object-model rule.**

> Payload references are operational evidence, not identity references. If a payload object is separately allocated, its retaining handle may be implemented as a cap-like object, but the architectural role is `PayloadCap` or a typed payload contribution.

---

## 3. OPA-3 — TTY Session/Pgrp Binding

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA3-TTY-BINDING-1 -->

**Violation.** [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) defines `ProcessGroup` and `Session` as identity-only entities. [`TTY.md`](../06_devices/TTY.md) instead says session and pgrp are not entities and stores leader `Cap<ProcessIdentity>` values in `SessionPgrp`.

That is now stale. The user's direction is the right repair:

> TTY should bind to `Session` and control its slave pgrps.

**Fix.** Make `TtyIdentity` bind directly to the PROCESS-owned identity-only entities:

```rust
pub struct TtyIdentity {
    pub payload: PayloadBinding<TtyPayload>,

    // Only meaningful for a controlling terminal. For ptys, this is
    // authoritative on the slave side; the master observes/control-drives it
    // through the slave peer.
    pub controlling_session: AtomicSlot<Option<Binding<Session, Addressability>>>,
    pub foreground_pgrp: AtomicSlot<Option<Binding<ProcessGroup, Addressability>>>,

    pub input_readable: RawQueue,
    pub output_writable: RawQueue,
    pub hangup_port: RawPort<HangupEvent>,
    pub session_ctl_port: RawPort<SessionCtlEvent>,
}
```

For pseudo-terminals:

- `TtyKind::PtySlave` is the controlling terminal candidate.
- `TtyKind::PtyMaster` does not own an independent controlling session.
- Master-side ioctls that affect job control resolve `peer: Cap<TtyIdentity>` and mutate the slave's `controlling_session` / `foreground_pgrp`.
- Signals generated by the line discipline are dispatched to the slave's foreground pgrp.

PROCESS should remain the owner of session and process-group truth:

```rust
pub struct Session {
    pub sid: Cap<PidStruct> or PidName,
    pub members: DllContainer<ProcessGroup>,

    // Materialization or convenience pointer, not the sole truth unless
    // PROCESS chooses to own the binding.
    pub controlling_tty: AtomicSlot<Option<Cap<TtyIdentity>>>,
}

pub struct ProcessGroup {
    pub session: Binding<Session, Addressability>,
    pub members: DllContainer<ProcessIdentity>,
}
```

There are two valid ownership choices. Pick one and make it explicit:

| Choice | Authoritative owner | Derived materialization |
|---|---|---|
| TTY-owned control binding | `TtyIdentity.controlling_session` and `foreground_pgrp` | `Session.controlling_tty` |
| PROCESS-owned control binding | `Session.controlling_tty` and `foreground_pgrp` | `TtyIdentity.session_ctl` snapshot |

Recommended: **TTY-owned control binding for the terminal relationship, PROCESS-owned membership for sessions/pgrps.**

Why:

- The controlling terminal is a property of a terminal endpoint.
- Hangup is a TTY transition; clearing the control binding and firing `hangup_port` are naturally one visibility boundary.
- Session/pgrp membership remains PROCESS-owned and is revalidated when TTY dispatches to a pgrp.

Required invariant:

**TTY-CTL-1.** A controlling-terminal binding names a `Session` identity and a foreground `ProcessGroup` identity. TTY never stores leader-process caps as a substitute for session/pgrp truth.

LINT: reject `SessionPgrp { session_leader: Cap<ProcessIdentity>, fg_pgrp_leader: Cap<ProcessIdentity> }` in TTY structure.

**TTY-CTL-2.** For ptys, only the slave `TtyIdentity` owns controlling-session and foreground-pgrp bindings. Master control operations mutate the slave through its peer cap.

LINT: reject controlling-session fields on `PtyMaster` except as forwarding helpers.

---

## 4. OPA-4 — PidNamespace / PidStruct Classification

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-PID-CLASSIFICATION-1 -->

This is the critical one because it determines the retention direction for every POSIX numeric identity.

### 4.1 Resolved conflict

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-RESOLVED-CONFLICT-1 -->

Earlier drafts of [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) mixed two designs:

```rust
pub struct ProcessIdentity {
    pub pid: Cap<PidStruct>,
}

pub struct ProcessGroup {
    pub pgid: Cap<PidStruct>,
}

pub struct Session {
    pub sid: Cap<PidStruct>,
}

pub struct PidNamespace {
    pub pid_map: PersistentRadix<u32, Cap<ProcessIdentity>>,
    pub tid_map: PersistentRadix<u32, Cap<ThreadIdentity>>,
    pub pgid_map: PersistentRadix<u32, Cap<ProcessGroup>>,
    pub sid_map: PersistentRadix<u32, Cap<Session>>,
}
```

The maps resolve numbers directly to semantic identities, but entities also carry `Cap<PidStruct>`. That creates two candidate authorities for "what does this number mean?"

That conflict is resolved by [`NAMESPACE_VIEW_v1.md`](NAMESPACE_VIEW_v1.md): namespace maps bind numbers to `PidName` / `PidStruct`; target identities keep non-retaining snapshots only.

### 4.2 What `PidNamespace` is

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-PIDNAMESPACE-ROLE-1 -->

Classify `PidNamespace` as a **co-located semantic entity owned by PROCESS**.

```rust
pub struct PidNamespace {
    pub level: u8,
    pub parent: Option<Cap<PidNamespace>>,
    pub numbers: AllocIndex<u32, Cap<PidName>>,
    pub init_proc: Option<Cap<ProcessIdentity>>,
}
```

It is not a service ledger and not a filesystem instance. It owns authoritative signifier bindings for pid/tid/pgid/sid number lookup.

### 4.3 Three viable designs

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-VIABLE-DESIGNS-1 -->

#### Option A — Direct maps only, defer `PidStruct`

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-OPTION-A-1 -->

```rust
pub struct PidNamespace {
    pub pid_map: PersistentRadix<u32, Cap<ProcessIdentity>>,
    pub tid_map: PersistentRadix<u32, Cap<ThreadIdentity>>,
    pub pgid_map: PersistentRadix<u32, Cap<ProcessGroup>>,
    pub sid_map: PersistentRadix<u32, Cap<Session>>,
}

pub struct ProcessIdentity {
    pub pid: PidNameSnapshot,
}
```

Pros:

- Clean v1 retention: pid map entries are addressability bindings directly to semantic identities.
- No extra hop.
- No cycle risk.
- Easy to reason about for wait/kill/pidfd.

Cons:

- Future nested namespace support needs a migration.
- `PidStruct` rationale is absent from the active object model.

This is the simplest v1, but it throws away the future-compatibility object the user already approved.

#### Option B — Active `PidStruct` as the authoritative numeric-name object

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-OPTION-B-1 -->

```rust
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

pub struct PidStruct {
    pub kind: PidKind,
    pub numbers: SmallVec<[NamespaceNumber; 1]>, // v1 length = 1
    pub target: PidTarget,
}

pub struct NamespaceNumber {
    pub ns: Cap<PidNamespace>,
    pub nr: u32,
}

pub struct PidNamespace {
    pub pid_map: PersistentRadix<u32, Cap<PidStruct>>,
}
```

Pros:

- Future nested namespace shape exists now.
- One map shape handles pid/tid/pgid/sid with a `kind` discriminator.
- pidfd-like references can retain the numeric-name object separately from the target.

Cons:

- If `PidStruct.target` holds `Cap<Target>` and target holds `Cap<PidStruct>`, naive designs create retention cycles.
- If `PidStruct.target` holds `Weak<Target>`, the namespace map is no longer an addressability binding to the target.
- It imposes an extra hop and extra object before nested namespaces exist.

This is viable only if retention direction is made very explicit.

#### Option C — Active `PidStruct`, but target owns no strong back-edge

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-OPTION-C-1 -->

This was the first recommended shape, but it needs one refinement: the pid object must be a **numeric view object**, not part of the canonical process tree.

```rust
pub struct PidStruct {
    pub kind: PidKind,
    pub numbers: SmallVec<[NamespaceNumber; 1]>,

    // Addressability obligation. The namespace name retains the semantic
    // target for as long as the numeric binding remains published.
    pub target: PidTarget,
}

pub struct ProcessIdentity {
    // Snapshot for fast getpid/procfs rendering. Not retention.
    pub pid_name: PidNameSnapshot,
}

pub struct ProcessGroup {
    pub pgid_name: PidNameSnapshot,
}

pub struct Session {
    pub sid_name: PidNameSnapshot,
}

pub struct PidNamespace {
    pub map: PersistentRadix<u32, Cap<PidStruct>>,
}
```

Retention direction:

```text
PidNamespace.numbers[nr] -> Cap<PidStruct> -> Cap<TargetIdentity>
TargetIdentity -> PidNameSnapshot only
```

No strong cycle. The target does not retain its pid name. It stores immutable number metadata for `getpid()`, `/proc`, and tracing. The namespace map is the authoritative binding; removing the map entry drops the `PidStruct`, which drops target retention.

For future nested namespaces, `PidStruct.numbers` grows from one `(ns, nr)` to a vector of ancestor namespace numbers. Each namespace map binds its visible number to the same `PidStruct`.

#### Option D — Canonical topology plus namespace views

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-OPTION-D-1 -->

This is the refined version and the new recommendation.

The critical distinction:

```text
canonical topology
    ProcessIdentity.parent
    ProcessIdentity.pgrp
    ProcessGroup.session
    ProcessIdentity.children
    ProcessGroup.members
    Session.members

numeric namespace view
    PidNamespace.numbers[nr] -> PidName/PidStruct -> target identity

tree view
    projection(canonical topology, viewer_pid_namespace)
```

Pid numbers are not the tree. They are signifiers for rendering and lookup from a particular namespace. The process tree, pgrp membership, and session membership remain canonical identity graph facts. A pid namespace changes which identities are visible and which numbers they render as; it does not create a second parent/child tree or a second session tree.

Canonical structure:

```rust
pub struct ProcessIdentity {
    pub parent: Binding<ProcessIdentity, Addressability>,
    pub pgrp: Binding<ProcessGroup, Addressability>,
    pub children: DllContainer<ProcessIdentity>, // materialized from parent

    // Non-authoritative convenience for fast self-rendering.
    pub pid_view: PidViewSnapshot,
}

pub struct ProcessGroup {
    pub session: Binding<Session, Addressability>,
    pub members: DllContainer<ProcessIdentity>, // materialized from proc.pgrp
    pub pgid_view: PidViewSnapshot,
}

pub struct Session {
    pub members: DllContainer<ProcessGroup>, // materialized from pgrp.session
    pub sid_view: PidViewSnapshot,
}
```

Namespace view:

```rust
pub struct PidNamespace {
    pub level: u8,
    pub parent: Option<Cap<PidNamespace>>,
    pub numbers: AllocIndex<u32, Cap<PidName>>,
    pub init_proc: Option<Cap<ProcessIdentity>>,
}

pub struct PidName {
    pub kind: PidKind,
    pub numbers: SmallVec<[NamespaceNumber; 1]>,

    // Addressability evidence for lookup through this name.
    // This is not topology ownership.
    pub target: PidTarget,
}

pub enum PidTarget {
    Process(Cap<ProcessIdentity>),
    Thread(Cap<ThreadIdentity>),
    ProcessGroup(Cap<ProcessGroup>),
    Session(Cap<Session>),
}
```

`PidStruct` can remain the implementation name if desired, but architecture prose should describe its role as `PidName`: a numeric view binding object. The name matters because "struct" sounds like Linux's internal authority object; in txKernel the authority is the namespace index plus the canonical target identity.

### 4.4 Recommended decision

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-RECOMMENDED-DECISION-1 -->

Use **Option D**.

It preserves the approved `PidStruct` concept without importing Linux's object cycles or making pid numbers part of topology. Numeric lookup is an addressability binding, but the binding is a view. Parentage, pgrp membership, and session membership remain canonical graph facts.

Canonical v1 shape:

```rust
pub struct PidNamespace {
    pub level: u8,                         // v1 = 0
    pub parent: Option<Cap<PidNamespace>>, // v1 = None
    pub numbers: AllocIndex<u32, Cap<PidName>>,
    pub init_proc: Option<Cap<ProcessIdentity>>,
}

pub struct PidName {
    pub kind: PidKind,
    pub numbers: SmallVec<[NamespaceNumber; 1]>, // v1 exactly one
    pub target: PidTarget,
}

pub enum PidTarget {
    Process(Cap<ProcessIdentity>),
    Thread(Cap<ThreadIdentity>),
    ProcessGroup(Cap<ProcessGroup>),
    Session(Cap<Session>),
}

pub struct PidNameSnapshot {
    pub ns: Weak<PidNamespace>,
    pub nr: u32,
    pub kind: PidKind,
}
```

Lookup:

```text
pid number + namespace
  -> PidNamespace.numbers[nr]
  -> Cap<PidName>
  -> check kind
  -> Cap<TargetIdentity>
```

Tree rendering:

```text
getppid(proc, viewer_ns):
  parent = proc.parent.load()
  if parent has a visible PidName in viewer_ns:
      return parent.pid number in viewer_ns
  else:
      return 0

/proc/<pid>/children in viewer_ns:
  walk proc.children
  keep only children with visible PidName in viewer_ns
  render each child's number in viewer_ns

kill(-pgid, viewer_ns):
  resolve pgid number in viewer_ns -> PidName(kind=ProcessGroup)
  target = ProcessGroup identity
  walk target.members canonically
  deliver only to members visible/authorized under caller policy
```

No separate tree is constructed for a pid namespace. A pid namespace supplies the lens used by resolution and projection.

Entity creation:

1. Reserve number and index slot in `PidNamespace.numbers`.
2. Reserve target identity/payload slots.
3. Construct target identity with `PidNameSnapshot`.
4. Construct `PidName { kind, numbers, target: Cap<Target> }`.
5. Publish `PidNamespace.numbers[nr] -> Cap<PidName>`.

Reap/free:

1. Withdraw `PidNamespace.numbers[nr]`.
2. Drop `Cap<PidName>`.
3. `PidName.target` drops its `Cap<TargetIdentity>`.
4. Free bitmap number at the target's semantic reclaim point.

For process zombies, do not withdraw the pid binding at process exit. Withdraw at reap, preserving wait and pidfd behavior.

### 4.5 Session and pgrp under pid namespaces

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-SESSION-PGRP-1 -->

The same split applies to sessions and process groups.

Do not create a separate session tree per pid namespace. `Session` and `ProcessGroup` are canonical identity-only objects. Their `sid` and `pgid` are view names, not their semantic substance.

Rules:

- `ProcessIdentity.pgrp` binds to a canonical `ProcessGroup`.
- `ProcessGroup.session` binds to a canonical `Session`.
- `PidNamespace.numbers[nr]` can name a `ProcessGroup` or `Session` for POSIX `getpgid`, `getsid`, `kill(-pgid)`, `tcsetpgrp`, and `/proc` rendering.
- A viewer namespace may fail to resolve or render a canonical group/session if that group/session has no visible `PidName` in the viewer's namespace.

This closes the earlier tension: shared global session tree is fine if it is canonical topology. Pid namespaces only change the tree **view**, not the tree.

### 4.6 Applying the rule to `nsproxy` members

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-NSPROXY-1 -->

The same separation should apply to namespace bundles, but not every namespace member has the same role. Treat `nsproxy` as a process/thread reference bundle, not as a semantic owner.

```rust
pub struct NsProxy {
    pub mnt_ns:  Cap<MountNamespace>,
    pub pid_ns:  Cap<PidNamespace>,   // view for current task
    pub pid_for_children: Cap<PidNamespace>,
    pub uts_ns:  Cap<UtsNamespace>,
    pub ipc_ns:  Cap<IpcNamespace>,
    pub net_ns:  Cap<NetNamespace>,
    pub cgroup_ns: Cap<CgroupNamespace>,
    pub time_ns: Cap<TimeNamespace>,
}
```

This object is a bundle of lenses/roots/authority domains. It is not the owner of the resources those namespaces expose.

Classify namespace members this way:

| Namespace kind | Role | Canonical truth | Namespace effect |
|---|---|---|---|
| `PidNamespace` | numeric view lens | process/thread/pgrp/session identity graph | number lookup/rendering and visibility |
| `MountNamespace` | topology root | mount tree owned by Mount | path resolution root/topology for VFS |
| `UtsNamespace` | small value domain | hostname/domainname values | value seen by uname/sethostname |
| `IpcNamespace` | object registry domain | IPC objects allocated in that namespace | lookup table and lifetime domain |
| `NetNamespace` | object registry/domain | sockets, interfaces, routes owned by net | network object lookup and isolation |
| `CgroupNamespace` | projection lens | cgroup hierarchy | path rendering/root-relative view |
| `TimeNamespace` | value/projection lens | monotonic/boottime offsets | clock values exposed to tasks |
| `UserNamespace` | authority lens/domain | credentials/caps/id maps | capability checks and uid/gid rendering |

The `PidNamespace` rule is the strongest form of the general pattern:

> If a namespace only changes names, visibility, roots, offsets, or authority interpretation, it should not duplicate the canonical object graph.

That means:

- no per-pid-namespace process tree;
- no per-cgroup-namespace cgroup tree;
- no per-user-namespace process ownership tree;
- no per-time-namespace timer object graph unless timers are semantically allocated inside that namespace;
- no `nsproxy`-owned shadow objects.

But `MountNamespace`, `IpcNamespace`, and `NetNamespace` are not merely view filters. They own namespace-local registries/topologies. That is still compatible with the rule: they own **their own namespace bindings**, not duplicate versions of another subsystem's canonical graph.

### 4.7 POSIX behavior impact

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-POSIX-IMPACT-1 -->

This should not trouble POSIX behavior. It helps.

POSIX process behavior wants stable process relations:

- parent/child for `wait*`;
- process group membership for job control and `kill(-pgid)`;
- session membership and controlling terminal behavior;
- signal fan-out over process groups;
- zombie persistence until wait/reap.

All of those are cleaner as canonical identity bindings. Pid namespaces only affect the numeric arguments and rendered values at the syscall boundary:

```text
syscall input number + caller.nsproxy.pid_ns
    -> resolve visible identity
    -> run POSIX operation on canonical topology
    -> render result numbers through caller/viewer namespace
```

Examples:

- `waitpid(pid)` resolves `pid` in the caller's pid namespace, then checks whether the resolved process is a canonical child.
- `getppid()` reads the canonical parent binding, then renders that parent through the caller's pid namespace; if not visible, return the namespace's conventional parent result.
- `kill(-pgid)` resolves the pgrp number in the caller's pid namespace, then walks the canonical `ProcessGroup.members`.
- `tcsetpgrp()` resolves a foreground pgrp in the caller/session view, then binds the TTY to the canonical `ProcessGroup`.
- `/proc` enumerates `PidNamespace.numbers` for the mounted procfs view and renders projected files by re-reading canonical process state.

The only subtlety is Linux-compatibility policy for visibility holes: what to return when a canonical relation points outside the viewer's pid namespace. That is a projection rule, not a reason to duplicate topology.

### 4.8 Invariants to add

<!-- txdoc:OBJECT-PATTERN-FIXES-OPA4-INVARIANTS-1 -->

**VIEW-1.** Canonical topology is owned by its semantic subsystem; view layers may resolve, filter, root, offset, interpret, or render that topology, but must not become co-owners of it.

LINT: reject a view/namespace object that stores authoritative parent, member, edge, or ownership lists for another subsystem's canonical graph.

**VIEW-2.** Syscall-facing operations decompose into view resolution, canonical semantic operation, and view rendering.

LINT: syscall scripts must keep `resolve_visible`, `operate_on_identity`, and `render_for_viewer` phases separate when namespace views are involved.

**VIEW-3.** View-owned bindings are authoritative only for the lens they introduce.

LINT: namespace maps, roots, offsets, visibility filters, and authority lenses must declare their owned binding domain and must not silently duplicate foreign owner state.

**VIEW-4.** Visibility holes are projection policy, not topology mutation.

LINT: code handling invisible parents, process groups, sessions, cgroups, mounts, or credentials must choose an explicit render/error policy instead of rewriting canonical relations.

**PID-1.** `PidNamespace` owns numeric signifier bindings for pid/tid/pgid/sid lookup.

LINT: process/thread/group/session numeric lookup must route through `PidNamespace`, not through ad hoc global maps.

**PID-2.** Namespace maps bind pid/tid/pgid/sid numbers to `Cap<PidName>`/`Cap<PidStruct>`, not directly to mixed target types.

LINT: reject `pid_map`, `tid_map`, `pgid_map`, and `sid_map` as separate direct-target maps in the canonical model.

**PID-3.** A pid-name object may retain the semantic target as addressability evidence; target identities do not retain pid-name objects.

LINT: reject `Cap<PidName>` / `Cap<PidStruct>` fields inside `ProcessIdentity`, `ThreadIdentity`, `ProcessGroup`, or `Session`; allow non-retaining snapshots only.

**PID-4.** A pid number is withdrawn at the semantic object's POSIX-visible name-death point, not merely payload death.

LINT: process pid binding withdraws at reap, not process exit; thread tid binding withdraws at thread reap/join semantics; pgrp/session numbers withdraw when membership and retainers permit.

**PID-5.** Future nested namespace support extends `PidName` / `PidStruct.numbers`; it does not change target entity identity.

LINT: code must not assume `numbers.len() == 1` outside v1-gated paths.

**PID-6.** Pid numbers are view signifiers, not canonical topology. Parent, pgrp, and session bindings target identities, never pid numbers.

LINT: reject canonical `parent`, `pgrp`, or `session` fields storing pid/tid/pgid/sid integers as authority.

**PID-7.** Per-namespace process/session/pgrp trees are projections over canonical topology under a pid-namespace lens, not separately maintained trees.

LINT: reject namespace-local children/member DLLs unless they are explicitly declared derived projections with revalidation against canonical bindings.

**NSVIEW-1.** `nsproxy`-like bundles hold namespace references; they do not own the semantic objects exposed through those namespaces.

LINT: reject `NsProxy` fields that duplicate process, mount, IPC, network, cgroup, or clock state instead of retaining namespace objects.

**NSVIEW-2.** A namespace may own bindings only for the domain it introduces. It must not maintain a shadow copy of another subsystem's canonical graph.

LINT: namespace-local registries must declare whether they are authoritative for their domain or derived projections over another owner.

**NSVIEW-3.** Syscall numeric inputs resolve through the caller's namespace lens; semantic checks and mutations act on canonical identities; results render back through the requested/viewer namespace.

LINT: syscall implementations must separate `resolve_number`, `operate_on_identity`, and `render_number` phases.

---

## 5. Patch Order

<!-- txdoc:OBJECT-PATTERN-FIXES-PATCH-ORDER-1 -->

1. Add `PID-*` invariants to [`INVARIANTS_v4.md`](INVARIANTS_v4.md).
2. Patch [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) §2 and §9 to use Option C or explicitly choose Option A.
3. Patch [`TTY.md`](../06_devices/TTY.md) §2.2 and §4 to bind to `Session` / `ProcessGroup`, with slave-owned pty control.
4. Patch [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) §2 and §3.2 for stale backing variants.
5. Patch Mount/PageBacked/Bdev wording from `Cap<MountPayload>` to `PayloadCap<MountPayload>` / `MountPayloadPin`.
