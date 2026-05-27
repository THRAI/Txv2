# Architectural Concepts

**Status.** v3 (2026-04-20).

**Supersedes.** v1, v2. v2 added §2.5 ("The four runtime roles") and associated renames (dispatch class → script-phase class). **v3 promotes the framework to three basis claims** by adding the publication principle as a peer to the resolution and lifecycle halves (§1), and introduces a new §8 ("Authoritative bindings and derived materializations") that formalizes the conditional-commit publication rule, unifying the substrate mutation primitive family and closing two open questions previously tracked in ADR-resolution-half Part VII. Sections after §8 renumber accordingly. The underlying concepts from v2 are preserved; v3 adds a load-bearing generalization that subsystem specs (notably VM) depend on.

**Purpose.** The master vocabulary document for txKernel's object and execution model. Defines the terms, decompositions, and primitives that all other architecture documents reference. Read this first.

**Audience.** Designers, reviewers, agents. Consulted before writing subsystem specs, reviewing proposals, or reasoning about where a mechanism belongs.

**Relationship to other documents.**

- `INVARIANTS.md` states enforceable rules using the vocabulary defined here.
- `object_model_0417.md` provides the underlying entity model (identity, payload, bindings, evidence, obligations).
- `LIVENESS.md` catalogs per-subsystem projections using the semantic plane's vocabulary.
- `MODULE_MAP.md` organizes the codebase around the four runtime roles introduced in §2.5.
- Subsystem specs (VFS, process, futex, etc.) apply this vocabulary to their specific domains.

---

## 1. The three basis claims

The architecture rests on three claims, which this document elaborates but does not replace:

**Resolution half.** A kernel maintains `(signifier, consistency, binding)` triples. Signifiers reach entities through bindings in namespace containers.

**Lifecycle half.** Every entity decomposes into `(identity, capability, payload)` layers. Identity is what the entity *is*; capability is what operations it supports; payload is its operational resources.

**Publication principle.** The system maintains a set of **authoritative bindings** and a set of **derived materializations**. Every materialization must be justified by a currently-valid authoritative binding. Publication of a materialization must re-validate its justifying binding atomically with the publication.

The first two claims come from v11. The third (new in v3) makes explicit what earlier versions implicitly relied on — the mechanism for keeping materializations consistent with bindings under concurrency. §8 elaborates the publication principle into its full framework. Everything below applies all three claims in combination.

---

## 2. The three planes

The architecture decomposes into three interacting planes plus one structural constraint. §2.5 introduces a complementary four-role decomposition that classifies runtime responsibilities rather than information content.

### 2.1 Semantic plane

**Defines truth.** Pure, guard-scoped, referentially transparent within an epoch.

- **Predicate.** Pure function `p(obj, ctx) → bool` over entity state and context. Defines what it means for a projection to hold.
- **Require.** The only authorized consumer of predicates at entry. Invokes predicates under an epoch guard; produces a witness on success; returns errno on failure.
- **Witness.** Proof that preconditions were satisfied at a point in time. Carries `IdentRef<'g, T>` values; `!Send + !Sync` by construction; lifetime-bound to the producing epoch guard.

### 2.2 Execution plane

**Makes transitions real.** Mutating, linearized, substrate-disciplined.

- **Step.** The execution primitive. A bounded synchronous unit of work that observes state, may mutate, and reports outcome. See §5.
- **Commit discipline.** A three-part internal pattern within a step that performs mutation: observe under guard, upgrade observations to retention, mutate via substrate primitives. Commit is not a phase; it is a classification of mutations within a step.
- **Obligation.** What a binding's commit makes true. Declared on binding type; determines evidence retention required at commit time.
- **Substrate primitives.** Observer-safe mutation primitives (`zone::sign`, `index::commit`, `index::withdraw_commit`, `index::swap_commit`, `credit::commit`) that enforce linearization at their call site.

### 2.3 Publication plane

**Makes waiting efficient.** Commit-disciplined, per-carrier-ordered, never authoritative.

- **Signal.** Commit-disciplined publication of selected transitions. Fires from within a mutating step, after the corresponding state write. Hints only.
- **Wire.** A bus carrier. Level-triggered (RawQueue) or edge-triggered (RawPort) or passive (RawTrace).
- **Signal attachment.** A per-subsystem declaration that a specific transition publishes on a specific wire. Selected, not automatic. Cataloged separately from projections.
- **Waiter contract.** Wake → re-observe via predicate or step → act. Signals never authorize action; fresh observation does.

### 2.4 Structural constraint: bifurcation

Per `object_model §8.1.1`. Entities whose partial order admits `structural ⟂ payload` (zombies, unlinked-but-open files, detached mounts, closed-but-queued sockets) factor into two zone-allocated types: `<n>Identity` and `<n>Payload`. Each zone reclaims independently.

**Signal attachment rule.** Each signal attachment names exactly one carrier slot (Identity or Payload), never a cross-layer composite. Cross-layer coordination requires explicit pairing, not shared wires.

### 2.5 The four runtime roles

Planes classify *what* information a mechanism operates on. A complementary decomposition classifies *what role* a mechanism plays at runtime. The runtime has four roles, organized around the central claim that the kernel is a constrained transition system driven by syscalls:

| Role | Responsibility |
|---|---|
| **step** | Construct one candidate transition observer-safely (the five-phase discipline: observe → upgrade → reserve → commit → publish). |
| **reactor** | Schedule coroutines that compose transitions; mediate wait and wake; own fault-AST and cross-core-barrier carve-outs. |
| **semantics** | Own objects; define which transitions are legal from which states; preserve invariants (the union of all object-owning subsystems). |
| **signifier** | Reverse-map userspace names (path, fd, va, pid) to object witnesses through binding containers. An aspect of specific semantic subsystems, not a separate class. |

Two categories sit alongside the roles:

- **Foundation / HAL.** Primitives and architecture-specific plumbing; below all four roles.
- **Scripts.** Per-syscall programs. Users of the runtime, not a role. Drive time by composing signifier resolutions and semantic step calls. Script-phase classes (§12) redistribute across scripts and reactor.

The four-role catalog is closed (ARCH-3; see §15.6). A proposal for a fifth role requires revisiting the invariants that determined this membership.

**How the roles relate to the planes.** Step is an execution-plane primitive with a publication tail (phase 5). Reactor is infrastructure that spans planes without owning them. Semantics is the union of object-owning subsystems and therefore spans all three planes — each semantic subsystem has predicates (semantic plane), step functions (execution plane), and signal attachments (publication plane). Signifier is a semantic-plane aspect (reverse-map queries over binding containers) of specific semantic subsystems.

For the placement of subsystems, services, filesystem instances, and leaf modules within the four-role structure, see [`MODULE_MAP.md`](./MODULE_MAP.md).

---

## 3. The reference hierarchy

Four reference types form a hierarchy; each downgrade is free, each upgrade is fallible.

```
Weak<T>            — nullable, epoch-independent
    ↓ observe under epoch
IdentRef<'g, T>    — epoch-guarded, stack-bound, no retention
    ↓ pin under epoch (SENTINEL_DEAD CAS)
Cap<T>             — refcounted identity retention, 'static
    ↓ upgrade payload (payload-counter CAS)
T::OperationalEvidence — pins payload, entails Cap<T>, 'static
```

`'static` forms (Cap, OperationalEvidence) may cross thread boundaries, be held in struct fields, and survive across `.await`. `'g`-forms (IdentRef, witnesses) may not.

---

## 4. Projections and monotonicity

Every entity has at least three independent projections:

- **Structural.** Retention > 0; entity is semantically alive.
- **Namespace / addressability.** Reachable through a chain of bindings from a namespace root.
- **Payload.** Operational state is available; for indirected entities, payload box exists; for compound-predicate entities, disjunction of typed contributions.

**Monotonicity.** Each projection transitions only from true to false, never back. This is the load-bearing property for race-degradation (§7.3) and waiter correctness.

Subsystem projection catalogs (in `LIVENESS.md`) enumerate projections per entity type and state the realization mechanism (identity retention / binding-chain reachability / payload retention / compound disjunction).

---

## 5. The step model

**The execution primitive is `step`, not `commit`.** Every kernel operation is a sequence of bounded synchronous steps.

### 5.1 Step outcome

```rust
enum StepOutcome<T> {
    Advanced(Progress),                            // made progress, may step again
    Blocked(Channel, Mask),                        // no progress, wait on channel
    AdvancedThenBlocked(Progress, Channel, Mask),  // made progress, then stalled
    Done(T),                                       // terminal with value
    Err(Errno),                                    // terminal with error
}
```

### 5.2 Step contract

A step:

- Runs **synchronously.** No `.await`, no future construction inside. If a step cannot proceed synchronously, it returns `Blocked` (or `AdvancedThenBlocked`).
- Is **bounded.** Completes in upper-bounded time. Work that exceeds the bound splits across multiple steps.
- **Observes under a fresh epoch guard.** Acquires its own guard; releases at step end.
- **May mutate** via substrate primitives, following the three-part in-step commit discipline.
- **May fire signals** via the temporal bus, after each linearizing write (I-7).
- **Returns one outcome.** Never partial, never ambiguous.

### 5.3 Witness scope

**Witnesses do not cross step boundaries.** A witness produced by `require_*` within a step is valid only for that step's synchronous execution. Between steps, the epoch guard is released; any witness is treated as expired.

Cross-step continuation carries `'static` retention evidence (Cap, OperationalEvidence), not witnesses. On resume, the next step downgrades Caps to IdentRef under a fresh guard and re-runs `require_*` to produce a fresh witness.

### 5.4 Monotone progress under retry

**Operation progress is monotone across steps.** No step undoes a previous step's published progress. A step that returns `Advanced(n)` has committed `n` units of progress that subsequent steps cannot retract.

This is the generalization of projection monotonicity to operation progression. It ensures that `Blocked → wait → retry` is safe: the retry begins from the progress state the previous step left, never from an earlier state.

### 5.5 Trajectories

Operations exhibit characteristic trajectories:

- **One-step operations.** `open`, `unlink`, `fork`, `dup`. Step returns `Done(value)` or `Err(errno)` immediately. No intermediate outcomes. Commits atomically as its single action.
- **Multi-step operations.** `read`, `write`, `splice`, `sendfile`, `copy_file_range`. Step returns `Advanced`, `Blocked`, or `AdvancedThenBlocked` repeatedly until terminating with `Done` or `Err`. Progress accumulates across steps.

The distinction is a property of the operation, not a separate pattern. Both are step functions; they differ only in which outcomes they produce.

---

## 6. The upper/lower halves

Steps are synchronous; operations are asynchronous. The division is formal:

**Lower half (per-step).** Synchronous, bounded, mutation-capable under epoch guard. Produces `StepOutcome` synchronously. No future creation, no awaiting, no reactor awareness.

**Upper half (per-operation).** Asynchronous composition of step outcomes with wait futures. Produces the operation's overall future. Owned by drivers (§10.1).

**Executor (reactor).** Runs the composed future. Handles temporal services (sleep, timeout, signal delivery). Does not see step functions directly.

Each layer has a single concurrency responsibility. Lower half is worker code. Upper half is composition code. Executor is scheduling code.

---

## 7. Bindings, obligations, reachability

### 7.1 Bindings

A **binding** is a structural slot in a container:

```
Binding<K, Obligation> {
    key:      K,
    evidence: Evidence<Obligation>,
}
```

Bindings have no independent lifetime; existence is governed by the container.

### 7.2 Obligations

Every binding declares one of three obligations:

- **Resolution-only.** Evidence: `Weak<T>`. No retention promise.
- **Addressability.** Evidence: `Cap<T>`. Target remains structurally alive.
- **Operational.** Evidence: `T::OperationalEvidence`. Payload projection holds.

**Publication is not part of obligation typing.** Whether a binding-install/withdraw fires a signal is declared separately in the signal attachment catalog, not by the binding's obligation.

### 7.3 Race degradation

Under monotonicity, container-linearized binding mutation, and SENTINEL_DEAD-guarded upgrade, concurrent mutation may cause operations to fail but cannot cause them to succeed against a different entity, a different context, or with different semantics. **Races degrade to failure, not to silent incorrect success.**

---

## 8. Authoritative bindings and derived materializations

The publication principle (§1) partitions system state into two tiers with a directional justification relation. This section elaborates the framework, names the mechanisms that enforce it, and catalogs the primitive family that realizes the publication rule.

### 8.1 The partition

**Authoritative binding.** The source of truth for a kernel fact. Its existence is what makes the fact true. If withdrawn, the fact ceases to be true. Authoritative bindings live in semantic indexes — persistent trees, radixes, hash tables, DLLs, queues — whose substrate-level mutations are observer-safe linearization points.

**Derived materialization.** A cached, computed, or pre-materialized artifact whose existence depends on an authoritative binding. Materializations are *reconstructible* from bindings. PTEs, TLB entries, dcache fast-path entries, protocol retransmit buffers, thread scheduler runqueue entries — these are materializations.

The relation is directional. Bindings justify materializations; materializations do not justify bindings. A materialization may be discarded at any time (under memory pressure, explicit invalidation, scope teardown); its content can be reconstructed from its justifying binding.

### 8.2 Scopes

The partition is relative. Each subsystem scope has its own authoritative bindings and its own derived materializations. Scopes nest: the materializations of an outer scope may themselves be the authoritative bindings of an inner scope.

Examples:

- **On-disk file** ↔ **PageContainer.pages radix**: from the filesystem's perspective, on-disk blocks are authoritative; the in-memory radix is a derived materialization. From VM's perspective, the radix is authoritative for "which Frames are materialized for this PC"; PTEs pointing at those Frames are derived.
- **recipes BTree** ↔ **pmap PTEs**: recipes are authoritative for "what is mapped"; PTEs are derived materializations of specific pages.
- **parent.children binding** ↔ **resolved `IdentRef<ProcessIdentity>`**: the binding is authoritative; a walker's cached IdentRef is a transient materialization valid only within its producing epoch.

Scope membership determines the justification chain. A materialization in scope `A` may be justified by a binding in scope `B`, which itself is a materialization of a binding in scope `C`, and so on up to an authoritative root (typically on-disk state, hardware state, or an unambiguous in-memory SSoT).

### 8.3 The justification invariant

**Every materialization is justified by a currently-valid authoritative binding.**

This invariant must hold at every observable moment, under arbitrary concurrency. Violation constitutes a silent UAF or TOCTOU window — the failure mode the race-degradation theorem (§7.3) is designed to exclude.

The invariant is load-bearing for several properties already stated:

- **PRED-7** (races degrade to failure, not silent incorrect success): if a materialization could exist without a justifying binding, a read through it would silently succeed against unauthorized state. The invariant prevents this.
- **Monotonicity of projections** (§4): a binding-based projection that becomes false must invalidate all materializations it justified, otherwise derived reads would see state the projection says is gone.
- **Reachability termination** (object_model §8): traversal terminates at entities reachable through binding chains; the invariant guarantees that reached-and-materialized state is still authoritatively current.

### 8.4 The publication rule

**Operations that publish a materialization must validate the justifying binding at the point of publication, atomically with the publication.**

Formally:

> Let `M` be a materialization about to be published with content derived from authoritative binding `B` in observed state `S`. The publication primitive atomically:
> 1. Re-observes `B` at the publication's linearization point.
> 2. If `B` is absent or differs from `S`, aborts with no observable effect (no partial publication).
> 3. If `B` matches `S`, commits `M` at the same linearization point.

This is the **conditional-commit shape**. It is the mechanism by which the justification invariant is preserved under concurrency: a publication whose justifying binding is concurrently withdrawn or changed re-verifies at commit, sees the change, and aborts cleanly.

### 8.5 The invalidation rule

**Operations that withdraw an authoritative binding must either atomically invalidate all dependent materializations, or guarantee that no subsequent materialization-publication against the withdrawn binding can succeed.**

The latter is automatic under the publication rule: once the binding is withdrawn, any concurrent publication's conditional commit will see the binding absent and abort. The former (atomic invalidation) is an optional optimization: explicitly tearing down derived materializations when the binding is withdrawn produces cleaner observable state than lazily letting them die through unreachability.

Ordering discipline. A binding-withdrawal operation sequences: (1) withdraw the binding via substrate primitive (linearization on the binding's container); (2) optionally tear down dependent materializations; (3) release any held resources.

Between (1) and (2), any materialization that was legitimately published before (1) still exists; materializations attempted concurrently at step (1) either succeed (justification held at their commit) or fail (justification lost before their commit). Either outcome is consistent.

### 8.6 Conditional-commit primitive family

The publication rule is realized by a family of substrate primitives unified by the shape "condition over authoritative binding → commit over materialization, atomically." Existing members of the family:

| Primitive | Identity condition | Payload op | Primary use |
|---|---|---|---|
| `index::commit` | key absent | install new binding | first-time publication (fork, open, mmap new range) |
| `index::withdraw_commit` | key maps to expected | remove binding | unlink, close, munmap |
| `index::swap_commit` | key maps to expected old | replace with new | rename, mprotect prot change |
| `index::install_if_match` | key maps to expected | install companion materialization | futex register-if, cross-structure publication |
| `index::install_if_absent` | key absent | install with fresh identity | O_CREAT with O_EXCL, mknod, mkdir |
| `register_if` | arbitrary predicate | enqueue waiter | FUTEX_WAIT |

The family is **closed by construction**: each member is a specialization of conditional-commit along two axes (what's checked; what's committed). No additional shapes are anticipated; proposals for new publication mechanisms should first express themselves as instances of this family.

### 8.7 Publication mechanisms

Two flavors of publication arise, distinguished by where the atomicity comes from.

**Substrate-linearized.** When publication *is* binding mutation — the new state becomes the new binding — the substrate primitive's atomicity provides linearization. The conditional check and the commit share the same atomic window inside the substrate primitive itself. Examples: `install_if_absent` on a DEntry children table (O_CREAT); `register_if` on a futex queue (FUTEX_WAIT); `commit` on a pid table (fork).

**Slot-locked with binding re-read.** When publication materializes a separately-located derived artifact that must be justified by an authoritative binding in a different structure, the materialization-slot's lock encompasses a lock-free snapshot read of the binding. The binding's own substrate is unlocked; the materialization-slot's lock provides the critical section in which the binding is observed and the materialization is committed. Example: PTE install on pmap leaf conditional on recipes binding (VM fault handler).

Both flavors satisfy the publication rule. They differ only in where the linearization comes from — the substrate primitive itself, or a slot-locked critical section that encompasses a re-read.

### 8.8 Relation to the substrate layer

The conditional-commit family is a subset of `substrate/mutation/` primitives. `SUBSYSTEM_ANATOMY.md §4.5` catalogs them as the "observer-safe mutation primitive" surface. This section (§8) explains *why* those primitives take the shape they do: they are the realizations of the publication rule.

A subsystem that invents a new observer-visible mutation must either use an existing family member or explain why no existing member fits. If no family member fits, the proposal is either misclassified (the "new mutation" is actually a composition of existing ones) or it reveals a genuinely new publication pattern that warrants an addition to the family (and invariants review).

### 8.9 Implications and non-obvious corollaries

**Range-scoped coordination.** When an operation's publication spans a range of potential materializations (VM's mmap/munmap/mprotect act on ranges of PTEs), the publication rule applies per-page but coordination is across the range. The materialization-slot lock (pmap leaf) provides per-page atomicity; a higher-level range-scoped primitive (VM's RangeLock) coordinates overlapping range operations. See `VM.md §3`.

**Multi-index publications.** An operation may publish multiple materializations atomically (e.g., fork commits new pid-table entry + new children-list entry). Each individual commit is a member of the family; the multi-commit atomicity comes from the substrate transactional bundle (future generalization of BindingSet<Pending>).

**No separate rmap.** Because the publication rule places the atomicity responsibility at the publication point (not at a per-Frame back-index), rmap is not required to maintain the invariant. Consequences on reflink and migration are absorbed as tech debt (per the affected subsystem specs) rather than as invariant violations.

**Reconstructibility as a design constraint.** Materializations must be reconstructible from their justifying bindings, because the invalidation rule permits their discard. A subsystem that attempts to stash authoritative state in what it calls a "cache" — content that cannot be regenerated from an upstream binding — violates the partition. The fix is typically to promote the "cache" to a binding, or to identify its justifying binding and record it explicitly.

### 8.10 What this closes

Two open questions from `ADR-resolution-half.md Part VII`:

- **"VM coordination primitive."** Closed. The primitive is `install_if_match` with recipes as authoritative binding and pmap as materialization. Elaborated in `VM.md §1`, §3.
- **"Conditional-commit generalization."** Closed. The family is the realization of the publication rule. §8.6 enumerates current members; additions go through invariants review.

---

## 9. The bus: static and temporal layers

The bus splits into two layers with distinct responsibilities.

### 9.1 Static bus

**What wires exist and who subscribes.** Compile-time-rooted declarations; type-checked subscriptions; no temporal behavior.

- **Wire declarations.** `readiness!`, `lifecycle!`, `tracepoint!` macros declare what a capability-type publishes. Fixed at type definition.
- **Subscription graph.** Who has registered interest on which wires. Queried by epoll at `EPOLL_CTL_ADD`; queried by wait primitive at step `Blocked` outcome.
- **Type-level safety.** Firing a mask bit not in the declared set is a compile error.

### 9.2 Temporal bus

**Runtime fire-and-wake.** The hot path; minimal.

- **Fire.** Called from within a step's mutating portion, after the state write. Delivers wakes to subscribers on the affected wire.
- **Wake delivery.** Per-carrier ordered (I-6, I-7). Wake recipients re-enter their drivers, which invoke the next step.
- **Diagnostic peek.** Read-only wire inspection for debugging. Never authoritative (I-2).

### 9.3 Three primitives

```
RawQueue    — level-triggered readiness, multi-subscriber, non-consuming
RawPort     — edge-triggered lifecycle events, possibly consuming
RawTrace    — passive diagnostic recording, no waiters
```

These are the complete set. Mechanisms requiring structured waitqueues (priority ordering, ownership tracking, counting) are subsystems, not bus primitives.

---

## 10. The wait primitive

**One loop, parameterized by (channel, condition, protocol).**

```
loop:
    check condition under fresh guard
    if ready: return ConditionTrue
    register waiter on channel
    sleep per protocol
    on wake: (loop: recheck)
    on interrupt: return classified outcome
```

### 10.1 The three modes

All operations involving potential waiting fall into three modes:

- **Nonblocking.** Call step once. If `Blocked`, return EAGAIN. No wait primitive invocation.
- **Waiting.** Call step in a loop; on `Blocked`, invoke wait primitive with `WaitProtocol`; resume. Covers both blocking and timed variants via protocol enum.
- **Selecting.** Register on channel without stepping. Used by epoll; returns fired-channel information to caller for further action.

### 10.2 WaitProtocol

Closed enum of named protocols:

```
Uninterruptible
Interruptible
Killable
InterruptibleTimeout(Duration)
KillableTimeout(Duration)
```

Each protocol specifies task state, interrupt policy, and timeout behavior. The set is closed; extension is an architectural decision.

### 10.3 Wait outcomes

```
WaitOutcome ::= ConditionTrue | Interrupted | Killed | TimedOut
```

Subsystems translate outcomes to POSIX errno per their operation's semantics (EINTR, ETIMEDOUT, partial-progress return, etc.).

### 10.4 The retry-is-recheck identity

The driver's retry loop and the wait primitive's recheck loop are the same loop. Calling `step()` after a wake *is* the re-evaluation of the condition; there is no separate retry mechanism. This is structural, not a discipline the driver enforces.

### 10.5 Specialized protocol objects

Generic wait_event takes (channel, condition, protocol) as parameters. Specialized protocol objects (e.g., `Completion`) ossify (condition, channel) into a named type for recurring patterns. Completion is the degenerate wait where condition is "done counter ≥ 1" and channel is the completion's internal queue. Specialized objects are justified when the pair recurs, the condition is simple and fixed, and type-level guarantees beat documented convention.

---

## 11. Middleware as protocol combinator

**Middleware is a higher-order protocol combinator.** It takes caller-supplied semantic operations or predicates as parameters and supplies execution protocol as implementation.

Protocol combinators:

- **Wait adapters.** `wait_event(channel, condition, protocol, ctx) → WaitOutcome`.
- **Drivers.** `drive_*(step_fn, wait_protocol, ctx) → Result<T, Errno>`. Three modes (§10.1).
- **Future combinators.** `with_timeout(fut, deadline)`, `with_cancel(fut, token)`.
- **Specialized protocol objects.** `Completion`, and others if added.

Not middleware (hard-coded policy or fixed mechanism):

- Observers, interceptors, gates (§12).
- Subsystem step functions.
- Bus primitives (fire, subscribe).
- Subsystem commit publications (fire from steps).

Protocol catalogs are closed. Extension requires architectural review.

---

## 12. Script-phase classes

Syscall-boundary control-flow decomposes into five disjoint classes. Under the four-role framing ([`MODULE_MAP.md §1`](./MODULE_MAP.md)) these are **script-phase classes**: they do not form a separate "dispatch layer" — they redistribute across scripts (prelude/postlude/in-script control flow) and substrate (reactor). The classes themselves, their disjointness, and their names are unchanged from earlier drafts; only the framing is updated. References to "dispatch classes" in older documents should be read as "script-phase classes" per this section.

### 12.1 Observe

Passive boundary observation. Hard-coded behavior: record, publish, count. No blocking, no control transfer, no admission decisions.

Examples: syscall tracepoints (enter/exit), audit subsystem publication, credential snapshot prelude, signal postlude check.

*Placement.* Script prelude/postlude machinery (`scripts/prelude/`, `scripts/postlude/`).

### 12.2 Intercept

Boundary control transfer with resumption. May park the thread; may transfer control to an external observer (tracer); requires recovery protocol.

Examples: ptrace entry/exit stops. The intercept class is strictly for control-transfer protocols, not for observation.

*Placement.* Script prelude/postlude with parking/resumption (`scripts/prelude/ptrace.rs`, `scripts/postlude/ptrace.rs`).

### 12.3 Gate

Universal admission decisions at the syscall boundary. Evaluates policy predicates over syscall metadata; admits or rejects. If rejected, the syscall never reaches subsystem execution.

Examples: seccomp BPF filters, LSM syscall-boundary hooks. Gates operate on syscall metadata only — not on subsystem-internal state.

*Placement.* Script prelude (`scripts/prelude/seccomp.rs`, `scripts/prelude/lsm.rs`).

### 12.4 Wait-adapt

The wait primitive (§10). Composes channel, condition, protocol into a blocking operation. Consumed by the waiting mode of drives.

*Placement.* Reactor service (`substrate/reactor/wait/`). Scripts invoke it when a step returns `Blocked*`.

### 12.5 Drive

The three modes (§10.1) that compose step functions with wait futures into operation futures. Drives are state-blind: they see step outcomes, wake events, and thread context; they never inspect subsystem-internal state.

*Placement.* In-script control flow — the per-syscall `resolve_and_compose` block of each script. Not a separate directory; scripts *are* drives plus their prelude/postlude.

### 12.6 Disjointness

The five classes are disjoint. No class mechanism implements another class's decisions: observers do not admit; gates do not park; interceptors do not admit; wait adapters do not admit; drives do not admit or transfer control. Subsystem-internal publications are not script-boundary hooks; they fire through bus primitives from step functions under SIG-4 discipline.

The disjointness property is what SCRIPT-4 enforces (see [`INVARIANTS.md §8`](./INVARIANTS.md)).

---

## 13. The three orthogonal decompositions

Every mechanism in the architecture has coordinates on three orthogonal axes. A mechanism that doesn't fit cleanly on all three is either misclassified or genuinely novel.

**Axis 1: Plane.** Semantic / execution / publication. Answers: what kind of information does this mechanism operate on?

**Axis 2: Script-phase class.** Observe / intercept / gate / wait-adapt / drive, or "subsystem-internal" for mechanisms not at the script boundary. Answers: what control-flow role does this play?

**Axis 3: Middleware vs fixed.** Protocol combinator (parameterized over semantics) or hard-coded (fixed behavior) or semantics carrier (consumed by combinators). Answers: is this parameterized or specialized?

The three axes together place any mechanism. Examples:

- `wait_event` — publication plane (consumes hints), wait-adapt class, middleware.
- `seccomp filter` — semantic plane (evaluates policy predicate), gate class, fixed.
- `pipe write_commit signal fire` — publication plane, subsystem-internal (not script-boundary), fixed.
- `Completion` — publication plane, wait-adapt class, middleware (specialized protocol object).
- `ptrace entry stop` — execution plane (pauses control flow), intercept class, fixed.

---

## 14. Carve-outs

Three notification mechanisms are **not** publication-plane signals and must not be implemented through bus primitives:

- **Fault injection** (SIGSEGV, SIGFPE, SIGBUS, SIGILL). Thread-local AST at trap-return. Specified by the signal subsystem.
- **Cross-core barriers** (TLB shootdown, IPI sync). Synchronous reactor coordination; issuing thread blocks on ack. Specified by the reactor's sync-coordination API.
- **Single-waiter handoff.** Subsystem-owned waitqueue with one-waiter protocol. Not a publication-catalog mechanism.

These are documented here so they are not accidentally forced through the bus model.

---

## 15. The closed catalogs

Catalogs that are closed by framework constraint; extension requires revisiting invariants (ARCH-3).

### 15.1 Step outcomes

```
Advanced(Progress)
Blocked(Channel, Mask)
AdvancedThenBlocked(Progress, Channel, Mask)
Done(T)
Err(Errno)
```

Five variants. No sixth. Extension would require a new execution-primitive concept.

### 15.2 Driver modes

```
nonblocking     — step once; Blocked → EAGAIN
waiting         — loop step with WaitProtocol variant
selecting       — register without stepping; epoll-shape
```

Three modes. Blocking and timed are WaitProtocol variants of the waiting mode, not separate modes.

### 15.3 Wait protocols

```
Uninterruptible
Interruptible
Killable
InterruptibleTimeout(Duration)
KillableTimeout(Duration)
```

Closed enum. Addition is an architectural decision.

### 15.4 Bus primitives

```
RawQueue, RawPort, RawTrace
```

Three primitives. Mechanisms requiring structured waitqueues are subsystems, not bus primitives.

### 15.5 Script-phase classes

```
observe, intercept, gate, wait-adapt, drive
```

Five classes. Disjoint. Adding a sixth requires architectural review. (Formerly "dispatch classes"; the classes are unchanged, the layer name is retired per §12.)

### 15.6 Runtime roles

```
step, reactor, semantics, signifier
```

Four roles. Per [`MODULE_MAP.md §1`](./MODULE_MAP.md). Scripts are users of the runtime, not a role. Foundation/HAL sits below all four. Adding a fifth role requires architectural review.

### 15.7 Conditional-commit primitive family

The substrate-layer realizations of the publication rule (§8.4). Closed by construction; each member is a specialization of "atomically check identity → commit materialization" along two axes (identity condition, payload op).

```
index::commit               — condition: key absent;        op: install new binding
index::withdraw_commit      — condition: key maps to expected; op: remove binding
index::swap_commit          — condition: key maps to expected old; op: replace with new
index::install_if_match     — condition: key maps to expected; op: install companion materialization
index::install_if_absent    — condition: key absent;        op: install with fresh identity
register_if                 — condition: arbitrary predicate; op: enqueue waiter
```

Six members. See §8.6 for their mapping to concrete operations and §8.7 for how they divide into substrate-linearized vs slot-locked-with-re-read flavors. Additions to this catalog go through invariants review; new proposals should first try to express themselves as instances of existing members.

---

## 16. The stacked one-liner

The model in one sentence, revised for three basis claims, the step primitive, and four-role framing:

> **Predicates define truth. Bindings justify materializations. Steps advance state monotonically under retry. Scripts compose steps with wait primitives into operations. Signals make waiting efficient. Only fresh observation authorizes action.**

Six roles, in causal order:

- **Predicates** define truth (semantic plane).
- **Bindings** are authoritative; **materializations** are derived and must be re-validated at publication (§8).
- **Steps** advance state monotonically (execution plane).
- **Scripts** compose (syscall-boundary drivers, invoking reactor waits).
- **Signals** publish hints (publication plane).
- **Fresh observation** authorizes action (waiter contract; re-run of step's require under fresh guard).

---

## 17. Vocabulary summary table

For quick reference:

| Term | Plane | Role |
|---|---|---|
| Predicate | Semantic | Defines truth |
| Require | Semantic | Entry-point consumer, produces witness |
| Witness | Semantic | Proof bound to epoch guard, one-step scope |
| Projection | Semantic | Monotone predicate over entity state |
| **Authoritative binding** | **Semantic** | **Source of truth in a scope; (key, value) in a semantic index** |
| **Derived materialization** | **Execution** | **Reconstructible artifact justified by an authoritative binding** |
| **Justification invariant** | **Semantic** | **Every materialization has a currently-valid authoritative binding** |
| **Publication rule** | **Execution** | **Atomic re-validation of binding at the moment of materialization commit** |
| **Invalidation rule** | **Execution** | **Binding withdrawal either atomically invalidates dependents or blocks their publication** |
| **Conditional-commit family** | **Execution** | **Substrate primitives realizing the publication rule (§15.7)** |
| Obligation | Execution | Binding-declared commit responsibility (evidence only) |
| Step | Execution | Bounded synchronous execution primitive |
| StepOutcome | Execution | Five-variant outcome enum |
| Progress monotonicity | Execution | No step undoes published progress |
| Commit discipline | Execution | Three-part mutation pattern within a step |
| Substrate primitive | Execution | Observer-safe mutation linearization |
| Signal | Publication | Commit-disciplined hint after linearizing write |
| Wire | Publication | Bus carrier (RawQueue/RawPort/RawTrace) |
| Signal attachment | Publication | Per-transition opt-in publication declaration |
| Waiter contract | Publication | Wake → re-observe → act |
| Bifurcation | Structural | Identity/Payload split (see `object_model §8.1.1`) |
| Cap<T> | Reference | Identity retention, 'static |
| IdentRef<'g, T> | Reference | Epoch-guarded observation, bounded lifetime |
| OperationalEvidence | Reference | Payload retention, entails Cap |
| Binding | Structural | Key+evidence slot in a container |
| Reachability | Structural | Binding-chain traversal |
| Driver | Script | Upper-half composition of steps + waits (drive class) |
| Wait primitive | Script | check → register → sleep → wake → recheck (wait-adapt class; reactor-provided) |
| Wait protocol | Script | Closed enum of task-state + interrupt policy |
| Observer / Interceptor / Gate | Script | Three boundary classes (non-middleware) |
| Middleware | Composition | Higher-order protocol combinator |

---

## 18. What this document does not cover

- **Per-subsystem projection catalogs.** See `LIVENESS.md`.
- **Per-subsystem signal attachments.** See `SIGNAL_ATTACHMENTS.md` (forthcoming).
- **Enforceable rules.** See `INVARIANTS.md`.
- **Entity factoring details.** See `object_model_0417.md §8.1.1`.
- **Module layout, import rules, lints.** See `SUBSYSTEM_ANATOMY.md`.
- **Reactor internals, HAL interfaces.** Out of scope for concepts; see reactor and HAL specs.
- **Specific syscall implementations.** Out of scope; see per-subsystem specs.

---

## References

- `object_model_0417.md` — entities, references, bindings, obligations, evidence.
- `INVARIANTS.md` — enforceable rules using this vocabulary.
- `LIVENESS.md` — projection catalog and predicate framework.
- `SUBSYSTEM_ANATOMY.md` — four-module layout and mutation discipline; substrate primitives (§4.5) are the catalog realized by §8.6 here.
- `MODULE_MAP.md` — subsystem placement under the four runtime roles (§2.5).
- `ADR-resolution-half.md` — reactor role; Part VII open questions "VM coordination primitive" and "Conditional-commit generalization" are **closed** by §8.10 of this document.
- `VM.md` — the first concrete subsystem spec built on §8's publication rule framework (recipes as authoritative binding, pmap as derived materialization, RangeLock as per-AddressSpace range-scoped coordination).
- `PAGE_SUBSTRATE.md`, `PAGE_BACKED.md` — page substrate and page-backed content model; build on §8 implicitly (Frame compound payload; PageContainer radix as authoritative at its scope; PTE map_count/CachePin/DmaToken as typed operational evidence).
