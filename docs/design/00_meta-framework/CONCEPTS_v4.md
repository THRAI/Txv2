# Architectural Concepts — v4

<!-- txdoc:00-META-FRAMEWORK-CONCEPTS-V4 -->

**Status.** Draft v4 for meta-framework unification.

**Supersedes.** `CONCEPTS_v3.md` after review. v4 preserves the three basis claims and publication rule from v3, replaces the stale four-role framing with the placement taxonomy from `MODULE_MAP_v1`, promotes the async step/script/reactor story, makes middleware/protocol-combinator vocabulary explicit, and adds the canonical-topology/view-layer split needed for namespaces and projections.

**Purpose.** Define the shared vocabulary for txKernel: basis claims, planes, canonical topology, view layers, architectural homes, references, predicates, steps, scripts, waits, publication, middleware, and carve-outs. This document is the conceptual spine; the invariant ledger states enforceable rules.

**Companion documents.**

- [`MODULE_MAP_v1.md`](MODULE_MAP_v1.md) — placement taxonomy and import boundaries.
- [`object_model_v2.md`](object_model_v2.md) — current implementation object model: entities, references, bindings, obligations, retention, and reclamation.
- [`INVARIANTS_v4.md`](INVARIANTS_v4.md) — canonical grep-friendly invariant set and linter notes.
- [`REACTOR_v0.md`](../02_execution/REACTOR_v0.md) — reactor boundary.
- [`STEP_MODEL_v1.md`](../02_execution/STEP_MODEL_v1.md) — step outcome algebra and five-stage in-step discipline.
- [`THREAD_RUNTIME_v1.md`](../02_execution/THREAD_RUNTIME_v1.md) — thread future, AST, signal delivery, and thread exit.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — bus primitives and wait integration.
- [`COMPLETION_v1.md`](../02_execution/COMPLETION_v1.md) — completion as specialized wait middleware.

---

## 1. Core Model

<!-- txdoc:CONCEPTS-CORE-MODEL-1 -->

txKernel is a constrained transition system.

Userspace names enter as signifiers. Signifiers resolve through authoritative binding containers under epoch guards. Checks turn current predicate truth into guard-scoped witnesses. Steps consume witnesses, upgrade them into retention, reserve private resources, commit observer-visible transitions at visibility boundaries, and publish selected hints on bus carriers. Scripts compose steps into syscalls, using the reactor to wait and retry. Semantic subsystems own truth. Services own policy ledgers. Filesystem instances are mounted backends. HAL and selected static registries sit outside the object model when forcing zone identity would be false structure.

Canonical topology is the identity graph owned by semantic subsystems. A view layer may sit above that topology to resolve user signifiers, filter visibility, choose roots, apply offsets, interpret authority, and render results. The view layer must not become a second owner of the same topology.

All derived materializations remain subordinate to authoritative bindings. Races degrade to clean failure, not silent retargeting.

---

## 2. Three Basis Claims

<!-- txdoc:CONCEPTS-BASIS-CLAIMS-1 -->

### 2.1 Resolution

<!-- txdoc:CONCEPTS-RESOLUTION-1 -->

A kernel maintains `(signifier, consistency, binding)` triples.

- A signifier is userspace-facing naming material: path, fd, pid, tid, virtual address, signal target, device node, or ABI handle.
- Consistency is the rule that says whether a signifier still names the same thing under concurrent mutation.
- A binding is an authoritative relation from a key/context to identity or evidence.

Resolution answers: "which identity does this user-visible name reach right now?"

### 2.2 Lifecycle

<!-- txdoc:CONCEPTS-LIFECYCLE-1 -->

Every entity decomposes into identity, capability, and payload layers.

- Identity answers "which object?"
- Capability answers "which operations may retain or address it?"
- Payload answers "which operational resources still exist?"

Some entities are co-located. Some split identity and payload. Some expose compound payload predicates with typed pin contributions. The object model defines the exact factoring rules.

### 2.3 Publication

<!-- txdoc:CONCEPTS-PUBLICATION-1 -->

Authoritative bindings justify derived materializations.

> Every derived materialization must be justified by a currently-valid authoritative binding. Publication of the materialization must revalidate the justifying binding atomically with publication.

This is not an appendix to lifecycle. It is a peer claim because concurrency bugs often happen when cached or pre-materialized state outlives the binding that justified it.

---

## 3. Three Planes

<!-- txdoc:CONCEPTS-PLANES-1 -->

Planes classify what kind of information a mechanism operates on.

| Plane | Meaning | Examples |
|---|---|---|
| Semantic | Truth about entities and legal transitions | predicates, bindings, obligations, projection definitions |
| Execution | How work is sequenced and committed | steps, scripts, reactor waits, scheduler dispatch |
| Publication | Hints and materializations made visible after transitions | bus fires, wait wakeups, tracepoints, completion wakes |

Bifurcation constrains all three planes: a mechanism must know whether it attaches to identity, payload, or a declared composite.

---

## 3.5 Canonical Topology and View Layer

<!-- txdoc:CONCEPTS-TOPOLOGY-VIEW-LAYER-1 -->

Topology is the owner-state graph: parent/child edges, process-group membership, session membership, mount edges, cgroup edges, and any other relation that is semantically mutated by one owning subsystem.

A view layer is a lens over topology. It may own the bindings required for that lens:

- signifier maps, such as pid/tid/pgid/sid numbers in a `PidNamespace`;
- roots and traversal entry points, such as a mount namespace root;
- visibility filters, such as pid namespace or cgroup namespace projection;
- rendering rules, such as number, path, uid/gid, or clock-value rendering;
- authority interpretation, such as user namespace capability meaning.

A view layer does not own the canonical graph it renders unless that graph is the namespace's own domain. The syscall shape is therefore:

```text
user signifier + caller view
    -> resolve visible canonical identity/object
    -> check and mutate canonical owner state
    -> render result through requested/viewer view
```

The key split is between **view-owned bindings** and **shadow topology**. A namespace may own namespace-local registries or roots that it introduces. It must not maintain a second process tree, session tree, cgroup tree, or foreign ownership graph merely because that graph is rendered differently to some task.

This is a major concept but a small object-model addition: it introduces a layer boundary, not a new identity kind for every projected relation.

---

## 4. Architectural Homes

<!-- txdoc:CONCEPTS-HOMES-1 -->

Homes classify where mechanisms live. Every mechanism has one primary home.

| Home | Owns | Does not own |
|---|---|---|
| Foundation / HAL | platform boot, traps, low-level hardware facts | semantic objects, syscall policy |
| Substrate | generic allocation, indexing, mutation, publication, epoch, pmap primitives | errno, policy, domain truth |
| Reactor | polling, wait, wake, preemption mechanism, AST slots | scheduler policy, semantic state |
| Scheduler policy | task selection and budgets | task polling, semantic objects |
| Full semantic subsystem | user-visible or kernel-semantic entities and transitions | cross-syscall sequencing |
| Service subsystem | policy ledgers and accounting state | foreign bindings or namespaces |
| Filesystem instance | mounted backend operations and backend IDs | VFS graph, mount topology |
| Script | syscall sequencing over checks, services, steps, and waits | durable truth |
| Shim | compatibility ABI translation | native truth already owned elsewhere |
| View / Projection | lens bindings, read-only rendering, visibility/root/number interpretation | canonical foreign topology, authorization except declared authority lenses, mutation of owner state |
| Static registry | compile-time tables outside zone identity | dynamic lifecycle |

`MODULE_MAP_v1` is canonical for placement details. This concepts document uses the homes as vocabulary.

### 4.1 Policy-based zones and public roles

<!-- txdoc:CONCEPTS-POLICY-ZONES-1 -->

Full semantic subsystems use policy-based zones as the common lifetime substrate
for reclaimable entities. That does not mean subsystem APIs expose allocator
policy. Public subsystem language stays role-shaped:

| Role | Public vocabulary |
|---|---|
| owned identity | `Cap<T>` |
| payload use | `PayloadCap<T>` or `T::OperationalEvidence` |
| stale hint | `Weak<T>` |
| guard-scoped observation | `IdentRef<'g, T>` in a witness |
| identity table entry | identity slot or index row storing typed evidence |
| read-only projection | projection row / projection reference with revalidation |

Raw policy parameters belong in substrate internals or entity-zone
declarations. Subsystems decide semantic roles; the role derives the zone-backed
type family.

---

## 5. Reference Hierarchy

<!-- txdoc:CONCEPTS-REFERENCE-HIERARCHY-1 -->

Four reference strengths form a hierarchy:

```text
Weak<T>
  -> IdentRef<'g, T>
  -> Cap<T>
  -> T::OperationalEvidence
```

**Weak<T>.** Epoch-independent nullable reference. It can become stale and must be upgraded or resolved before use.

**IdentRef<'g, T>.** Guard-scoped observation. It is memory-safe because the guard defers reclamation, but it carries no retention authority.

**Cap<T>.** Identity retention. It may cross epochs, threads, and awaits. It keeps identity alive, not necessarily payload.

**T::OperationalEvidence.** Operation-specific evidence that entails identity retention and pins whatever projection the operation requires.

Upgrades are fallible. Downgrades are free. Witnesses may contain `IdentRef`; cross-step continuation may contain `Cap` or operational evidence.

---

## 6. Projections, Predicates, and Witnesses

<!-- txdoc:CONCEPTS-PREDICATES-WITNESSES-1 -->

A projection is a named view of entity liveness or usability. Canonical projection families are:

- structural;
- namespace/addressability;
- payload/operational.

Subsystems may declare local projection names such as `wait_addr`, `signal_addr`, `readable`, `mounted`, or `stopped`, but the mechanism for each projection must be stated.

Predicates are pure, guard-scoped functions that evaluate projection truth. Predicates do not mutate, allocate, block, reserve, publish, or wait.

`require_*` functions are the sanctioned predicate consumers at operation entry. They produce witnesses: guard-scoped evidence that predicates held under a specific observation. A witness is not authority. It must not cross guard, thread, async, or step boundaries.

The anti-TOCTOU rule is:

```text
observe under guard -> witness -> upgrade before mutation -> re-require after wait
```

---

## 7. Bindings and Obligations

<!-- txdoc:CONCEPTS-BINDINGS-OBLIGATIONS-1 -->

A binding is an authoritative relation from a key/context to target evidence.

Externally meaningful bindings declare exactly one obligation:

| Obligation | Meaning | Evidence |
|---|---|---|
| ResolutionOnly | lookup hint only | `Weak<T>` or no retention |
| Addressability | target identity remains addressable | `Cap<T>` |
| Operational | target operation remains possible | `T::OperationalEvidence` |

Obligations cover evidence only. They do not imply bus publication. Publication is declared per transition.

Bindings target identity. Payload is reached through identity-owned operational evidence.

---

## 8. Steps

<!-- txdoc:CONCEPTS-STEPS-1 -->

A step is the synchronous, bounded execution primitive. It returns one value from the closed outcome algebra:

```text
StepOutcome<T> =
    Advanced(progress)
  | Blocked(carrier, interests)
  | AdvancedThenBlocked(progress, carrier, interests)
  | Done(T)
  | Err(errno)
```

Steps do not `.await`, construct futures, park tasks, or call the executor.

Mutating steps follow this order:

```text
observe -> upgrade -> reserve -> commit -> publish
```

- **Observe.** Acquire guard; call `require_*`; produce witnesses.
- **Upgrade.** Convert observations into `Cap` or operational evidence.
- **Reserve.** Acquire linear resources that roll back on drop.
- **Commit.** Cross visibility boundaries through observer-safe substrate primitives.
- **Publish.** Fire declared hints after the corresponding write is visible.

After a commit point, rollback is not part of the model. Errors after visible progress are reported through partial progress, deferred failure, fatal termination, or a syscall-specific rule.

---

## 9. Scripts

<!-- txdoc:CONCEPTS-SCRIPTS-1 -->

A script is a per-syscall program that composes signifier resolution, service checks, semantic steps, waits, and cross-subsystem sequencing.

Scripts own sequencing. They do not own durable truth.

Scripts may:

- select driver mode;
- call public checks and execution APIs;
- hold opaque resume state containing retained evidence;
- invoke reactor waits;
- translate step and wait outcomes into syscall results.

Scripts must not:

- inspect subsystem `structure/`;
- store witnesses;
- define authoritative indexes;
- fire subsystem signal carriers directly;
- treat wake, completion, or trace payloads as truth.

Some scripts have a point of no return. After that boundary, recoverably fallible work is forbidden unless the failure mode is fatal-only or explicitly deferred.

---

## 10. Reactor and Thread Future

<!-- txdoc:CONCEPTS-REACTOR-THREAD-FUTURE-1 -->

The reactor owns execution mechanism:

- task submission and polling;
- wait registration and wake delivery;
- userspace-run;
- kernel poll boundaries;
- AST slots;
- cross-core synchronous coordination shell.

The thread runtime builds one long-lived future per userspace thread. The composition is:

```text
reactor task
  -> thread_future
       -> syscall/fault script future
            -> synchronous step calls
            -> reactor waits between steps
```

Only scripts and thread futures suspend. Steps do not.

Poll boundaries are safe points. Guards, witnesses, and step-local state do not cross polls. Userspace may be preempted many times while the thread future is waiting on `request_userspace_run`; timer preemption does not advance the future. The future advances only on interesting traps or explicit wake/resume events.

Signals and termination are observed at two sites:

- wait-adapt while parked;
- AST before return to userspace.

There is no mid-step signal delivery site.

---

## 11. Waits

<!-- txdoc:CONCEPTS-WAITS-1 -->

The wait primitive composes:

```text
channel + condition + WaitProtocol -> WaitOutcome
```

Canonical wait protocols are:

- uninterruptible;
- interruptible;
- killable;
- interruptible timeout;
- killable timeout.

Canonical wait outcomes are:

```text
Ready | Interrupted | Killed | TimedOut
```

The structural loop is:

```text
check condition -> subscribe/arm -> recheck condition -> sleep -> wake -> recheck condition
```

`Ready` means the wait adapter's condition check says the driver may retry the
step. It does not mean the wake itself was truth. Wake does not authorize
action. A wake says only "try again." The condition or the next step invocation
establishes truth under a fresh guard.

The retry-is-recheck identity is load-bearing:

> Calling the step again after a wake is the condition re-evaluation.

---

## 12. Bus

<!-- txdoc:CONCEPTS-BUS-1 -->

The bus is the publication mechanism for transition hints.

The primitive set is closed:

| Primitive | Meaning |
|---|---|
| RawQueue | level-triggered readiness |
| RawPort | edge-triggered lifecycle/event delivery |
| RawTrace | passive diagnostic recording |

Bus carriers are not state. Signals are not truth. Wires are not authorization. Subscriber code must re-observe.

Publication is opt-in and declared per transition. The fire happens after the paired visibility boundary and only on the owning carrier. Ordering is per carrier, not global.

The bus does not schedule, sleep, choose policy, or know subsystem semantics. It invokes reactor-owned wakers.

---

## 13. Completion

<!-- txdoc:CONCEPTS-COMPLETION-1 -->

Completion is a Linux-inspired specialized wait object. It is not a bus primitive.

Conceptually:

```text
Completion = wait_event(
    channel   = completion.private_channel,
    condition = completion.done_count > 0,
    protocol  = caller-selected WaitProtocol
)
```

The default completion consumes one completion credit per successful waiter.
Broadcast or latch semantics require an explicitly named type such as
`BroadcastCompletion` or `LatchCompletion`.

Completion is middleware because it packages a recurring protocol shape; it does not define subsystem truth.

Use completion for:

- one-shot internal rendezvous;
- helper-task completion;
- closed countdowns;
- group-exit collapse coordination;
- initialization barriers after reactor time begins.

Do not use completion for:

- externally visible readiness;
- process/file/mount/device semantic state;
- ownership transfer;
- priority donation;
- rt-mutex handoff;
- bypassing re-observation.

If the mechanism selects a particular waiter or transfers ownership/control, it is not a completion; it falls under subsystem synchronization and `EXC-3`.

---

## 14. Middleware / Protocol Combinators

<!-- txdoc:CONCEPTS-MIDDLEWARE-PROTOCOL-1 -->

Middleware is a higher-order protocol combinator. It accepts caller-supplied semantic operations, predicates, futures, or steps, and supplies execution protocol.

Examples:

- `wait_event(channel, condition, protocol, ctx)`;
- `drive_nonblocking(step_fn, ctx)`;
- `drive_waiting(step_fn, wait_protocol, ctx)`;
- `drive_selecting(carriers, interests, ctx)`;
- `with_timeout(fut, deadline)`;
- `with_cancel(fut, token)`;
- `Completion` and `CountdownCompletion`.

Middleware does not own truth. It must not inspect subsystem structure. It must not hard-code policy that belongs to a caller or owning subsystem.

Not middleware:

- predicates;
- gates;
- ptrace stops;
- bus primitives;
- subsystem step functions;
- subsystem commit publications;
- scheduler policy.

Rule of thumb:

> A protocol combinator supplies control protocol; the caller supplies semantic truth.

---

## 15. Script-Phase Classes

<!-- txdoc:CONCEPTS-SCRIPT-PHASE-CLASSES-1 -->

Script-boundary control flow decomposes into five classes:

| Class | Role |
|---|---|
| Observe | record metadata; cannot reject |
| Intercept | transfer control with resumption protocol |
| Gate | admit or reject; cannot park |
| Wait-adapt | park/resume through reactor wait |
| Drive | compose steps and waits into syscall result |

The classes are disjoint. A mechanism that needs two classes should be split.

---

## 16. Authoritative Bindings and Derived Materializations

<!-- txdoc:CONCEPTS-AUTH-BINDINGS-MATERIALIZATIONS-1 -->

An authoritative binding is a source of truth. A derived materialization is a cached, computed, denormalized, or pre-materialized artifact whose correctness depends on a binding.

Examples of materializations:

- PTEs derived from VM recipes;
- dcache fast paths derived from namespace bindings;
- wait registrations derived from a condition/binding snapshot;
- PageContainer pages derived from filesystem or backing state;
- scheduler runqueue entries derived from task state;
- completion wake state derived from a completion condition.

The publication rule:

```text
At materialization publication:
    re-read or revalidate the justifying binding/condition
    atomically publish only if it still matches
```

Two implementation flavors are accepted:

- substrate-linearized conditional commit;
- slot-locked publication with binding re-read.

Binding withdrawal must either invalidate dependents or prevent future publication against the withdrawn binding.

---

## 17. Carve-Outs

<!-- txdoc:CONCEPTS-CARVE-OUTS-1 -->

Some mechanisms are intentionally not bus publication:

| Carve-out | Home |
|---|---|
| synchronous fault injection | HAL / thread runtime / signal path |
| cross-core barriers and TLB shootdown | reactor synchronous coordination |
| single-waiter handoff | subsystem-specific synchronization |

These exclusions are architectural rules, not implementation preferences. Reusing bus publication for them is a category error.

---

## 18. Closed Catalogs

<!-- txdoc:CONCEPTS-CLOSED-CATALOGS-1 -->

Closed catalogs require architecture review to extend.

Current closed catalogs:

- architectural homes;
- view-layer roles;
- reference strengths;
- binding obligations;
- `StepOutcome` variants;
- driver modes;
- wait protocols;
- wait outcomes;
- bus primitives;
- script-phase classes;
- conditional-commit primitive family;
- publication exclusions.

Middleware instances are not individually closed, but new middleware families must satisfy `ARCH-4`: they must be protocol combinators over caller-supplied semantics.

---

## 19. Classification Examples

<!-- txdoc:CONCEPTS-CLASSIFICATION-EXAMPLES-1 -->

| Mechanism | Plane | Home | Script phase | Middleware? |
|---|---|---|---|---|
| pipe read step | execution/publication | full subsystem | subsystem-internal | no |
| pipe `readable` wire | publication | substrate bus carrier owned by pipe capability | subsystem-internal | no |
| `wait_event` | publication/execution | reactor/wait | wait-adapt | yes |
| blocking read driver | execution | script | drive | yes |
| seccomp filter | semantic | shim/service depending on final placement | gate | no |
| ptrace entry stop | execution | observation/process integration | intercept | no |
| `Completion` | publication/execution | reactor/wait | wait-adapt | yes |
| scheduler choice | execution | scheduler policy | none | no |
| procfs process file | semantic projection | filesystem instance/projection | none | no |

---

## 20. What This Document Does Not Cover

<!-- txdoc:CONCEPTS-NONCOVERAGE-1 -->

- Concrete object layout and reclamation details: `object_model_v2.md` and `EBR_ZONE_INTERFACE_v1.md` own this for implementation.
- Enforceable invariant wording: `INVARIANTS_v4.md` or `INVARIANT_LEDGER_v1.md`.
- Full subsystem shapes: `SUBSYSTEM_ANATOMY_v3.md`.
- Projection catalog rows: `PROJECTION_CATALOG_v1.md`.
- Per-transition signal attachment rows: `SIGNAL_ATTACHMENTS_v1.md`.
- Reactor implementation algorithms: `REACTOR_v0.md` and later reactor specs.
- Scheduler algorithms beyond placement: `SCHEDULER_v0.md`.

---

## 21. v4 Rewrite Notes

<!-- txdoc:CONCEPTS-V4-REWRITE-NOTES-1 -->

v4 makes these changes relative to v3:

- replaces "four runtime roles" as a closed role set with the `MODULE_MAP_v1` architectural homes;
- keeps three planes as information classification;
- preserves the three basis claims;
- keeps the reference hierarchy and require/witness discipline;
- promotes stackless async orchestration as `thread_future -> script future -> synchronous step`;
- clarifies that completion is middleware, not bus;
- keeps middleware under the more precise term "protocol combinator";
- adds the canonical-topology/view-layer split for pid namespaces, nsproxy members, and projected filesystems;
- preserves `ARCH-5` as the publication spine;
- keeps carve-outs as named architecture exclusions.
