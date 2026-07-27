# Architectural Concepts — v5

<!-- txdoc:TXV3-CONCEPTS-V5 -->

**Status.** v5 (Txv3 refresh, 2026-05).
**Supersedes.** `00_meta-framework/CONCEPTS_v4.md`. v5 retains v4's basis claims, planes, and view-layer split, and adds: the seven-layer architecture, the five primitive cells, the upper/lower script split, the ExecutionScope cleavage from YieldShape, the four-variant StepOutcome with closed YieldShape catalog, and the typed `StepOp`/`StepProgress` traits.
**Companion documents.** `02_INVARIANTS_v5.md` (rules), `03_STEP_MODEL_v2.md` (algebra), `04_SYSCALL_SHAPE_v1.md` (upper/lower split worked examples), `05_DELEGATE_v1.md`, `06_EXECUTION_SCOPE_v1.md`, and [`OBJECT_API_LANES_v1.md`](../design/00_meta-framework/OBJECT_API_LANES_v1.md) (owner/root API factoring).

---

## 1. The seven-layer architecture

<!-- txdoc:CONCEPTS-V5-LAYERS-1 -->

Every txKernel concept belongs to one of seven layers. Each layer names what it owns and what it does not.

| Layer | Owns | Does not own |
|---|---|---|
| **algebra** | `StepOutcome<T,P>`, `YieldShape`, `StepProgress` | the protocol that *resolves* a yield, the policy that *selects* a thread |
| **protocol** | `DriverMode`, wait-protocol families, per-yield-shape resolve methods | semantic state, scheduler decisions |
| **semantic owner** | per-subsystem typed `StepOp` impls; identity/payload split | sequencing across subsystems, identity context |
| **composition owner** | per-syscall scripts; upper/lower half discipline | durable semantic truth |
| **identity context** | `SubjectContext`, `SubjectAuthority`, `ExecutionScope` | per-step witnesses, payload state |
| **scheduler** | reactor, scheduler-policy, AST slots, per-hart runqueues | semantic objects, syscall policy |
| **reclamation** | `Cap` / `OperationalEvidence`, EBR, zone slots | semantic policy, identity context |

Layers compose strictly: algebra is referenced by protocol; protocol is referenced by composition owner; composition owner instantiates semantic-owner step impls; identity context wraps both; scheduler owns runtime mechanism; reclamation underlies everything. No layer reaches across more than one neighbor.

This is the v5 refinement of v4's architectural-homes table. Homes still exist; the seven layers are how those homes stack.

## 2. The five primitive cells

<!-- txdoc:CONCEPTS-V5-CELLS-1 -->

Every POSIX syscall decomposes into operations on five primitive cells. The cells are the kernel's vocabulary; they correspond to the questions POSIX has always assumed but never answered explicitly.

### 2.1 SubjectContext — *who*

<!-- txdoc:CONCEPTS-V5-CELL-SUBJECT-1 -->

```rust
struct SubjectContext {
    process: Cap<ProcessIdentity>,
    thread: Option<Cap<ThreadIdentity>>,   // None for OnBehalfOf scopes
    authority: SubjectAuthority,
}

struct SubjectAuthority {
    cred: Cap<Credential>,
    restrictions: Cap<RestrictionStack>,   // append-only: no_new_privs is structural
    // future: per-namespace lens, namespace authority overrides
}
```

The SubjectContext is established at script entry by either:

- materialization from the running thread's task (`SubjectContext::from_thread`), or
- borrowing from another process (`SubjectContext::borrowed`), under an ExecutionScope.

It is **script-scoped**, not thread-scoped. There is no `current_subject_context()` accessor; helpers take `&SubjectContext` explicitly.

The authority is replaceable only by authorized cred-service transitions (suid exec, set*uid). Replacement is a publication boundary — observers see the old or new authority, not an intermediate.

### 2.2 Signifier Resolution — *what does this name reach right now*

<!-- txdoc:CONCEPTS-V5-CELL-SIGNIFIER-1 -->

A signifier is userspace-facing naming material: path, fd, pid, tid, virtual address, signal target, device node, ABI handle.

Resolution is `(signifier, &SubjectContext) → Result<Cap<T>, Errno>` under epoch guard. The result carries identity retention (`Cap`) or operational evidence; the lookup discharges any authority check imposed by the SubjectContext (cred check at open, restriction-stack walk for seccomp/landlock).

Resolution is itself a script: it composes typed `StepOp`s and may yield (dcache miss → block read; seccomp-trap → `OnAgent` to tracer; fanotify FAN_OPEN_PERM → `OnAgent` to daemon). Both halves of a syscall use the same `StepOp`/`StepOutcome`/`drive` machinery; the upper/lower split is about *concerns*, not about *mechanism*.

### 2.3 StepOp + StepOutcome — *the bounded transaction*

<!-- txdoc:CONCEPTS-V5-CELL-STEP-1 -->

```rust
trait StepOp {
    type Output;
    type Progress: StepProgress;
    fn step(&mut self, ctx: &mut ScriptCtx) -> StepOutcome<Self::Output, Self::Progress>;
}

enum StepOutcome<T, P> {
    Continue { progress: P },
    Yield    { progress: P, shape: YieldShape },
    Done(T),
    Err(Errno),
}
```

A step is a synchronous, bounded, outcome-returning unit of work. Its five-stage internal discipline (Observe → Upgrade → Reserve → Commit → Publish) is preserved from STEP_MODEL_v1.

Progress is operation-specific and typed by `StepProgress` (see `03_STEP_MODEL_v2 §3`). The driver accumulates progress across steps via `StepProgress::extend`.

### 2.4 YieldShape — *what the kernel may wait on*

<!-- txdoc:CONCEPTS-V5-CELL-YIELD-1 -->

```rust
enum YieldShape {                          // closed catalog
    OnWaitSource {
        source: WaitSourceId,
        interests: InterestMask,
        registration: PreparedWaitRegistration,
    },
    OnAgent {
        endpoint: Cap<DelegateEndpoint>,
        request: DelegateRequest,
        token: Cap<DelegateToken>,
        cancel: AgentCancelPolicy,
    },
    OnTimer {
        deadline: Deadline,
    },
    // deferred catalog members:
    // OnEdge   { subscription: Cap<EdgeSubscription>, interests: EdgeInterests },
    // OnHandoff { owned: Cap<OwnedSlot<T>>, priority: PriorityHint },
}
```

A YieldShape is the closed enumeration of things a script may pause-and-resume on. Each member states its substrate cost, its resume protocol, and its abandonment semantics.

A YieldShape is *not* an ExecutionScope. ExecutionScope is extent-shaped (the script runs within a borrowed identity); YieldShape is point-shaped (the script halts at a point and is resumed when a condition resolves). The two compose orthogonally.

**Deadlines are not yield-shape fields.** Timeouts are protocol attachments via `WaitProtocol.deadline` and are realized as a driver-installed `TimerGuard` regardless of primary shape. The same mechanism handles `OnWaitSource + timeout` (poll/select), `OnAgent + timeout` (FUSE/ufd), and standalone `OnTimer`. Composing `OnTimer` with `WaitProtocol.deadline` is invalid (the primary timer is itself the deadline).

### 2.5 Publication / Projection — *what becomes visible*

<!-- txdoc:CONCEPTS-V5-CELL-PUBLICATION-1 -->

After a commit, two render channels expose the change:

- **Synchronous return** — the script's final `Done(T)` value, pushed to the caller as the syscall return.
- **Asynchronous publication** — bus primitives (`RawQueue`, `RawPort`, `RawTrace`), signal attachments, projection-row updates, /proc renders.

Both are governed by the publication rule:

> Every derived materialization must be justified by a currently-valid authoritative binding. Publication of the materialization must revalidate the justifying binding atomically with publication.

Publication is opt-in and declared per transition. Carriers fire after the paired visibility boundary; subscribers re-observe.

#### Owner API lanes

<!-- txdoc:CONCEPTS-V5-OWNER-API-LANES-1 -->

Semantic owner/root objects expose a limited lane language above their private
storage: `BindingLane` for authoritative bindings, `ProjectionLane` for
read-only views, and `ReadinessLane` for level state plus object-owned wait
endpoints. Identity/evidence is the existing role-shaped type language;
reservation is a binding-change phase; publication/RCU is an owner-private
backend. Lanes therefore do not add a sixth primitive cell or expose a storage
algorithm. The complete contract is `OBJECT_API_LANES_v1.md`.

## 3. Three planes (preserved from v4)

<!-- txdoc:CONCEPTS-V5-PLANES-1 -->

Planes classify what kind of information a mechanism operates on. v5 preserves v4 §3.

| Plane | Meaning | Examples |
|---|---|---|
| Semantic | Truth about entities and legal transitions | predicates, bindings, obligations, projection definitions |
| Execution | How work is sequenced and committed | steps, scripts, reactor waits, scheduler dispatch |
| Publication | Hints and materializations made visible after transitions | bus fires, wait wakeups, tracepoints, completion wakes |

Bifurcation (BIF-2: identity ⊥ payload) constrains all three planes.

## 4. Canonical topology and view layer (preserved from v4)

<!-- txdoc:CONCEPTS-V5-TOPOLOGY-1 -->

v5 preserves v4 §3.5 unchanged. Topology is owner-state graph; view layer is lens-over-topology; namespaces own their introduced bindings (signifier maps, roots, render rules) but never shadow foreign topology.

The interaction with v5 cells: a view layer is a *visibility filter* in Signifier Resolution; namespace-owned bindings become signifier-map sources consulted during resolution.

## 5. Architectural homes (preserved from v4, lightly extended)

<!-- txdoc:CONCEPTS-V5-HOMES-1 -->

Homes classify where mechanisms live. v5 preserves v4 §4 with one addition: *ExecutionScope owner* (a specialization of script-home for borrowed-identity scripts).

| Home | Owns | Does not own |
|---|---|---|
| Foundation / HAL | platform boot, traps, hardware facts | semantic objects, syscall policy |
| Substrate | generic allocation, indexing, mutation, publication, epoch, pmap | errno, policy, domain truth |
| Reactor | polling, wait, wake, preemption, AST slots | scheduler policy, semantic state |
| Scheduler policy | task selection and budgets | task polling, semantic objects |
| Full semantic subsystem | user-visible or kernel-semantic entities and transitions | cross-syscall sequencing |
| Service subsystem | policy ledgers and accounting state | foreign bindings or namespaces |
| Filesystem instance | mounted backend operations and backend IDs | VFS graph, mount topology |
| Script | per-syscall sequencing over checks, services, steps, waits | durable truth |
| **ExecutionScope owner** (new) | borrowed-identity scope lifecycle, abandonment routing | identity itself, script semantics |
| Shim | compatibility ABI translation | native truth already owned elsewhere |
| View / Projection | lens bindings, read-only rendering | canonical foreign topology |
| Static registry | compile-time tables outside zone identity | dynamic lifecycle |

`MODULE_MAP_v1` remains canonical for placement details.

## 6. Reference hierarchy (preserved from v4)

<!-- txdoc:CONCEPTS-V5-REFERENCE-HIERARCHY-1 -->

Four reference strengths form a hierarchy:

```
Weak<T> → IdentRef<'g, T> → Cap<T> → T::OperationalEvidence
```

A fifth strength, `Owned<T>` (transferable single-holder), is reserved for `OnHandoff` and remains deferred until a PI-futex / RT-mutex implementation lands.

Upgrades are fallible. Downgrades are free. Witnesses may contain `IdentRef`; cross-step continuation may contain `Cap` or operational evidence; cross-yield continuation may contain only `'static` retention evidence (see YIELD-1 in `02_INVARIANTS_v5`).

## 7. Predicates, witnesses, the require-discipline (preserved from v4)

<!-- txdoc:CONCEPTS-V5-PREDICATES-1 -->

v5 preserves v4 §6. Predicates are pure guard-scoped functions; require-functions produce witnesses; witnesses are guard-scoped and must not cross step / await / thread / yield boundaries.

The anti-TOCTOU rule:

```
observe under guard → witness → upgrade before mutation → re-require after wait/yield
```

The yield boundary is added explicitly to the rule in v5. After resume from any YieldShape, the script must re-require under fresh guard; the resume itself is not truth.

## 8. Bindings and obligations (preserved from v4)

<!-- txdoc:CONCEPTS-V5-BINDINGS-1 -->

v5 preserves v4 §7 unchanged.

## 9. The five-stage in-step discipline (preserved from v4)

<!-- txdoc:CONCEPTS-V5-STEP-DISCIPLINE-1 -->

Mutating steps follow:

```
observe → upgrade → reserve → commit → publish
```

Detail in `03_STEP_MODEL_v2 §4`. After a commit, rollback is not part of the model.

## 10. Scripts and the upper/lower split

<!-- txdoc:CONCEPTS-V5-SCRIPTS-1 -->

A script is a per-syscall program composing signifier resolution, service checks, semantic steps, waits, and cross-subsystem sequencing.

**v5 introduces the upper/lower split**, which is the syscall-side reflection of BIF-2 (identity ⊥ payload):

| Half | Concerns | Composes |
|---|---|---|
| **Upper half** | identity, signifier, authority, restriction stack | typed StepOps over `SubjectContext`: cred check, fd/path/pid resolution, seccomp filter, LSM hook |
| **Lower half** | payload transition | typed StepOps over object payload: vfs/pipe/net/vm/proc step functions |

Both halves use `StepOp` / `StepOutcome` / `YieldShape` / `drive` identically. The split is about *concerns*, not *mechanism*. Both halves can yield, both halves can be driven by any DriverMode that accepts the yield shapes they use.

A script's `SubjectContext` is established at entry and remains constant for the script's duration except via authorized authority replacement (cred service transitions). See `04_SYSCALL_SHAPE_v1` for worked examples.

### Dispatch lanes

Not all syscalls are scripts. A syscall enters one of three dispatch lanes after the trampoline materializes the `SubjectContext`:

- **ImmediateSyscall** — pure ABI query (getpid, getuid, umask, times). No `StepOp`, no `drive`, no yield. The call chain is statically non-yielding.
- **OneShotStepOp** — semantic transition (setuid, sigaction, setsid, close). Enters `StepOp` with `drive_oneshot()`; terminates on first `step()` with `Done` or `Err`. Never yields. Benefits from the five-stage discipline without async overhead.
- **Full async script** — progressive or blocking operation (read, write, open, futex_wait, poll). Enters `async drive()`; may `Continue`, `Yield` any `YieldShape`, and requires the complete driver/reactor stack with `ActiveWait` and `apply_resume`.

Detailed classification and dispatch rules in `04_SYSCALL_SHAPE_v1 §6`. Invariants in `02_INVARIANTS_v5.md` (SCRIPT-V5-4/5, STEP-11/12).

## 11. Yield-adapt phase class (renamed from Wait-adapt)

<!-- txdoc:CONCEPTS-V5-YIELD-ADAPT-1 -->

The script-phase catalog stays at five members. v5 generalizes the wait-adapt member to **yield-adapt** to reflect that it consumes any closed `YieldShape`, not only `OnWaitSource`. The five remain disjoint:

| Class | Role |
|---|---|
| Observe | record metadata; cannot reject |
| Intercept | transfer control with resumption protocol |
| Gate | admit or reject; cannot park |
| **Yield-adapt** (was Wait-adapt) | park/resume through any YieldShape |
| Drive | compose steps and yields into syscall result |

A new YieldShape does *not* require a new phase class. Adding `OnAgent` did not. Adding `OnEdge` and `OnHandoff` will not.

## 12. ExecutionScope vs YieldShape

<!-- txdoc:CONCEPTS-V5-SCOPE-VS-YIELD-1 -->

This is the cleavage v5 names explicitly:

> **YieldShape governs script pause/resume.** A yield is point-shaped: the script halts, an external condition resolves, the script continues from where it stopped, with fresh guard and re-required predicates.
>
> **ExecutionScope governs script identity context.** A scope is extent-shaped: the entire script (or a syntactic sub-region of it) runs under a particular `SubjectContext`, with credential, file-table, address-space, and rlimit lookups resolved against that subject.

The two compose orthogonally: a script running inside `OnBehalfOf<P>` may emit any YieldShape; a yield does not enter or leave a scope.

| | Shape | Examples |
|---|---|---|
| **YieldShape** (point) | OnWaitSource, OnAgent, OnTimer, *OnEdge, OnHandoff* | wait on pipe readiness; delegate to FUSE daemon; nanosleep; *epoll-ET subscription; PI-futex handoff* |
| **ExecutionScope** (extent) | Thread (default), OnBehalfOf | native syscall under thread identity; SQPOLL kthread under user identity |

See `06_EXECUTION_SCOPE_v1` for OnBehalfOf details.

## 13. Bus and publication (preserved from v4)

<!-- txdoc:CONCEPTS-V5-BUS-1 -->

The bus is the publication mechanism for transition hints. The primitive set is closed: `RawQueue`, `RawPort`, `RawTrace`. v5 preserves v4 §12 unchanged. A new bus primitive (`RawEdge`, possibly, when `OnEdge` lands) requires the same architecture review as any closed-catalog extension.

## 14. Completion (preserved from v4)

<!-- txdoc:CONCEPTS-V5-COMPLETION-1 -->

v5 preserves v4 §13. Completion is specialized wait middleware, not bus, not truth. It composes through the new `Yield` outcome with `OnWaitSource` (the completion's private `WaitSource`).

The `EXC-3` exclusion (completion may not select a particular waiter / donate priority / transfer ownership) becomes structurally clearer in v5: those are the contract of `OnHandoff` (deferred), which is a different YieldShape.

## 15. Middleware / protocol combinators (preserved from v4)

<!-- txdoc:CONCEPTS-V5-MIDDLEWARE-1 -->

v5 preserves v4 §14. Middleware supplies control protocol; the caller supplies semantic truth. With v5's algebra, middleware is parameterized over `StepOp` / `YieldShape` / `DriverMode` rather than over the older `attempt_*` shapes.

## 16. Authoritative bindings and derived materializations (preserved from v4)

<!-- txdoc:CONCEPTS-V5-MATERIALIZATIONS-1 -->

v5 preserves v4 §16 unchanged. The publication rule applies to all five v5 cells.

## 17. Carve-outs (preserved from v4)

<!-- txdoc:CONCEPTS-V5-CARVE-OUTS-1 -->

v5 preserves v4 §17 unchanged.

| Carve-out | Home |
|---|---|
| Synchronous fault injection | HAL / thread runtime / signal path |
| Cross-core barriers and TLB shootdown | reactor synchronous coordination |
| Single-waiter handoff | subsystem-specific synchronization (formalized as `OnHandoff` YieldShape when added) |

## 18. Closed catalogs

<!-- txdoc:CONCEPTS-V5-CLOSED-CATALOGS-1 -->

The full closed-catalog list is in `00_PREFACE.md §6`. Catalogs require architecture review to extend; the architectural-extension protocol is in `00_PREFACE.md §5`.

v5 changes vs v4:

- **StepOutcome variants** reduced from 5 to 4 (`Continue` / `Yield` / `Done` / `Err`).
- **YieldShape** added as a new closed catalog (`OnWaitSource`, `OnAgent`, `OnTimer` initially; `OnEdge` and `OnHandoff` deferred).
- **ExecutionScope kinds** added as a new closed catalog (`Thread`, `OnBehalfOf` initially).
- **Driver modes** become a closed catalog explicitly (`Nonblocking`, `Waiting`, `Selecting`).
- **Restriction kinds** reserved as a future closed catalog (will populate when seccomp/landlock land).

## 19. v5 rewrite notes

<!-- txdoc:CONCEPTS-V5-REWRITE-NOTES-1 -->

v5 changes relative to v4:

- Promotes the seven-layer architecture to top-level structure (§1).
- Introduces the five primitive cells as the syscall vocabulary (§2).
- Names the upper/lower script split (§10).
- Renames Wait-adapt to Yield-adapt; preserves five phase classes (§11).
- Names the ExecutionScope vs YieldShape cleavage (§12).
- References the four-variant `StepOutcome` and closed `YieldShape` catalog from `03_STEP_MODEL_v2`.
- Treats Owned<T> as a deferred fifth reference strength reserved for `OnHandoff` (§6).
- Preserves all v4 §-content not explicitly listed as changed.
