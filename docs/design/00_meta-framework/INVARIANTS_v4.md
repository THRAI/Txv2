# Architectural Invariants — v4

<!-- txdoc:00-META-FRAMEWORK-INVARIANTS-V4 -->

**Status.** Draft canonical invariant set for meta-framework unification.

**Supersedes.** `INVARIANTS_v3_3.md` after review. v4 promotes the grep-friendly ledger into the canonical invariant set, replaces the stale four-runtime-role framing with architectural homes from `MODULE_MAP_v1`, adds async/reactor and completion invariants, and folds newer service/filesystem/mount/exec/vm/process/device rules into one reviewable surface.

**Purpose.** State txKernel invariants under stable, grep-friendly labels. This file is the bridge between architecture prose, thesis formalization, review comments, and future linter checks.

**Companion documents.**

- [`CONCEPTS_v4.md`](CONCEPTS_v4.md) — vocabulary: basis claims, planes, homes, steps, waits, middleware, and carve-outs.
- [`MODULE_MAP_v1.md`](MODULE_MAP_v1.md) — placement taxonomy and import boundaries.
- [`object_model_v2.md`](object_model_v2.md) — current implementation object model: entities, references, bindings, obligations, retention, and reclamation.
- [`STEP_MODEL_v1.md`](../02_execution/STEP_MODEL_v1.md) — step outcome algebra and in-step discipline.
- [`REACTOR_v0.md`](../02_execution/REACTOR_v0.md) — reactor boundary contract.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — bus primitives and wait integration.
- [`COMPLETION_v1.md`](../02_execution/COMPLETION_v1.md) — completion as specialized wait middleware.
- [`EBR_ZONE_INTERFACE_v1.md`](../01_substrate/EBR_ZONE_INTERFACE_v1.md) — zone/reference interface adaptation, weak/retention boundary, and upper-language translation.
- [`02_EBR_design.en-US.md`](../../ebr-zone/02_EBR_design.en-US.md) and [`03_Zone_Cap_object_storage_design.en-US.md`](../../ebr-zone/03_Zone_Cap_object_storage_design.en-US.md) — foundation implementation sketches adapted to the current interface.

**Label policy.**

- Existing labels from `INVARIANTS_v3_3.md` are preserved: `BIF-*`, `PRED-*`, `WIT-*`, `OBL-*`, `SIG-*`, `STEP-*`, `SCRIPT-*`, `ARCH-*`, `EXC-*`.
- Async/runtime labels use `ASYNC-*`; completion labels use `COMP-*`.
- Zone/epoch substrate labels use `ZONE-*` and `EBR-*`.
- Cross-cutting placement and linter labels use `MAP-*` and `LINT-*`.
- Subsystem-local labels preserve their owning prefix, e.g. `MOUNT-*`, `MOUNT-BDY-*`, `EXEC-PONR`.
- Every row should be short enough to quote in a review comment.

**Linter notes.** `LINT:` lines are implementation-facing checks suggested by the invariant. Some are directly lintable by import/type/API rules; some need annotations or typestate APIs.

---

## 1. BIF — Bifurcation

<!-- txdoc:INVARIANTS-BIF-1 -->

**BIF-1.** Each entity must explicitly declare whether it admits `structural ⟂ payload` factoring. Ad hoc splitting or ad hoc unification is invalid.

LINT: require every entity declaration to choose `CoLocated`, `IdentityPayload`, or `CompoundPayload`.

**BIF-2.** If factored, identity and payload must be independently retained and independently reclaimed.

LINT: split entities must have distinct identity/payload retention fields and no single counter that implies both.

**BIF-3.** Addressability bindings target identity. Operational bindings reach payload through identity-owned payload evidence. Bindings must not target payload directly as namespace truth.

LINT: reject `Binding<*, *Payload, Addressability>` and namespace indexes storing payload caps.

**BIF-4.** Co-located entities have identity and payload coincident; operational evidence is identity retention.

LINT: co-located entities should define `OperationalEvidence = Cap<Self>` or an equivalent direct identity pin.

**BIF-5.** Signal carriers attach to exactly one retention domain: identity or payload, never a cross-domain composite.

LINT: signal declarations must name a single carrier owner type and retention domain.

**BIF-6.** Strong identity retention must not imply strong payload retention. Payload may reach zero while identity remains retained.

LINT: flag APIs where `Cap<Identity>` exposes payload fields without a payload-presence check or payload evidence.

---

## 2. PRED — Predicates and Projections

<!-- txdoc:INVARIANTS-PRED-1 -->

**PRED-1.** Predicates must be pure: no mutation, allocation, I/O, blocking, reservation, or publication.

LINT: in `checks/predicates`, forbid calls to mutating substrate APIs, allocation APIs, reactor wait APIs, and bus fire APIs.

**PRED-2.** Predicates must be guard-scoped. Predicate observations are bounded by an epoch guard and do not upgrade to retention.

LINT: predicate functions may accept `IdentRef<'g, T>` and `Guard`; they must not return `Cap<T>` or operational evidence.

**PRED-3.** Every entity declares its projections. Canonical vocabulary is `structural`, `namespace`, and `payload`, with subsystem-local projection names allowed when declared.

LINT: entity specs/code require a projection declaration block or annotation.

**PRED-4.** Each projection declares exactly one realization mechanism: identity retention, binding-chain reachability, payload retention, or declared disjunction of typed contributions.

LINT: projection annotations require `mechanism = ...`; reject mixed ad hoc checks in predicates.

**PRED-5.** Each projection declares monotonicity. Monotone projections transition true to false only; non-monotone projections require explicit justification.

LINT: lowering operations must be named; raising a monotone projection on an existing entity is suspicious unless it creates a new entity/generation.

**PRED-6.** Projection implications are per-entity theorems, not universal assumptions.

LINT: require explicit local theorem/annotation for code relying on `namespace => structural`, `payload => structural`, etc., except for globally typed entailments.

**PRED-7.** Concurrent mutation must not cause an operation to succeed against a different entity, context, or semantics. Races degrade to clean failure, not silent incorrect success.

LINT: require observer-safe mutation primitives for binding changes; reject remove-then-insert sequences for already-visible bindings unless annotated as safe.

**PRED-8.** Predicate evaluation for authorization must occur through `require_*`. Direct predicate use outside the require boundary bypasses witness construction.

LINT: external modules may call `require_*`, not raw predicates; raw predicates are private or `pub(crate)` to checks.

---

## 3. WIT — Witnesses

<!-- txdoc:INVARIANTS-WIT-1 -->

**WIT-1.** Witnesses are produced only by require functions.

LINT: witness constructors are private to `checks`; code outside cannot instantiate witness structs.

**WIT-2.** Witnesses carry guard-scoped observation evidence, not retention authority.

LINT: witness fields may contain `IdentRef<'g, T>` and value snapshots, not `Cap<T>` or operational evidence unless explicitly marked metadata.

**WIT-3.** Witnesses are valid only within the guard and context that produced them. They do not cross guard, thread, async, or synchronous-step boundaries.

LINT: witness types must be `!Send`, `!Sync`, non-`'static`, and not captured across `.await`.

**WIT-4.** Witnesses must not be stored in long-lived state.

LINT: reject witness types in struct fields, statics, heap collections, task state, script continuation state, and return types that outlive a step.

**WIT-5.** Cross-step continuation carries strong retention evidence, not witnesses. Resumed steps re-run require under a fresh guard.

LINT: continuation/resume structs may hold `Cap`/operational evidence but not witnesses or `IdentRef`.

**WIT-6.** Witness construction is gated by the producing module.

LINT: require private fields or sealed witness traits; no public all-fields constructors.

---

## 4. OBL — Binding Obligations

<!-- txdoc:INVARIANTS-OBL-1 -->

**OBL-1.** Every externally meaningful binding declares exactly one obligation: `ResolutionOnly`, `Addressability`, or `Operational`.

LINT: all binding/index types include an obligation parameter or equivalent declared evidence type.

**OBL-2.** Binding evidence must match the declared obligation.

LINT: `ResolutionOnly -> Weak<T>|()`, `Addressability -> Cap<T>`, `Operational -> T::OperationalEvidence`.

**OBL-3.** Obligations cover evidence only. Signal publication is a separate per-transition attachment.

LINT: binding declarations must not imply bus wires or fire behavior.

**OBL-4.** Mutating steps upgrade witness observations to evidence matching outgoing binding obligations before committing those bindings.

LINT: commit APIs consume typed evidence/reservations; no commit from `IdentRef`.

**OBL-5.** Obligation declarations are stable across target entity implementation changes; only concrete evidence types vary.

LINT: code should depend on obligation traits/types, not hard-coded payload representation.

---

## 4A. ZONE / EBR — Zone, Cap, Weak, and Epoch Reclamation

<!-- txdoc:INVARIANTS-ZONE-EBR-1 -->

**ZONE-1.** Policy-based zones are the universal upper-subsystem lifetime substrate for reclaimable entities, but upper subsystem APIs expose role-shaped types (`Cap<T>`, `PayloadCap<T>`, `Weak<T>`, `IdentRef<'g, T>`, witnesses, identity slots, projection refs), not raw policy parameters.

LINT: reject reclaimable semantic entities that bypass zone-derived type families; reject `Zone<T, Policy>` or policy-type parameters in semantic subsystem operation code. Policy may appear in substrate internals or entity-zone declarations only.

**ZONE-1A.** Each subsystem spec declares a zone-derived type manifest: zone-backed entities, container-owned binding values, static facts, witness/projection handles, and the public evidence type for each stored reference.

LINT: reject new subsystem entity prose that says only "object" or "reference" without classifying the role that derives its zone/reference type.

**ZONE-2.** Zone publication uses linear reservation plus infallible signing: `zone::reserve` returns `ZoneReservation<T>`, and `zone::sign` consumes it to publish a live slot and return the first `Cap<T>`.

LINT: reject object publication paths that initialize visible zone slots before all required reservations/evidence are held; reject fallible `sign` APIs unless explicitly substrate-internal and proven equivalent.

**ZONE-3.** `Cap<T>` construction is closed: a cap is produced only by `zone::sign`, `Cap::clone`, or guarded upgrade from `IdentRef<'g, T>`.

LINT: reject public `Cap::from_raw`, pointer-to-cap conversions, or test-only constructors reachable outside reviewed substrate/test gates.

**ZONE-4.** `Weak<T>` is non-retaining. It is a generation-tagged stale-tolerant handle; it has no weak refcount, no reclamation Drop path, and cannot satisfy addressability.

LINT: reject `weak_count` in semantic zone metadata, `Weak<T>::drop` reclamation hooks, and addressability bindings storing `Weak<T>`.

**ZONE-5.** `Weak<T>` observation is guarded and generation-checked. Stale, retired, absent, or generation-mismatched slots return `None`.

LINT: `Weak<T>::observe` / `upgrade` APIs must require `Guard`; reject weak upgrade paths that dereference slot metadata without guard protection and generation/state checks.

**ZONE-6.** `IdentRef<'g, T> -> Cap<T>` is the single bridge from EBR observation to identity retention. The upgrade CAS checks generation, live state, no-upgrade sentinel, and retention increment atomically.

LINT: reject multi-step check-then-increment upgrade sequences where generation/state can change between check and retain.

**ZONE-7.** Final identity retention drop installs the no-upgrade barrier before queuing retirement. After the barrier, new `Cap<T>` upgrades fail cleanly.

LINT: final-drop paths must perform a `Live -> Dead/SENTINEL_DEAD` transition before `epoch::retire`; reject retire paths that leave an upgrade window open.

**ZONE-8.** EBR-observed slot contents are not destructed or overwritten until epoch quiescence. Final `Cap<T>` drop queues retirement; the reclaim callback runs `T::drop`, increments generation, and returns the slot.

LINT: reject immediate `T::drop` or reuse of `T` storage on final retain drop for types observable through `IdentRef<'g, T>`.

**ZONE-9.** Payload evidence is reached through identity. For split entities, namespace/addressability bindings target identity; operational use obtains `PayloadCap<Payload>` or a typed contribution through identity-owned payload state.

LINT: reject namespace indexes storing payload caps and prose/code that treats `Cap<Identity>` as direct payload authority.

**ZONE-10.** Binding values and static facts do not become zone entities unless they gain independent identity and reclamation.

LINT: reject `Zone<VmEntry>`, `Cap<CharDeviceBinding>`, or similar fake entity wrappers for authoritative container values and static board/platform facts.

**EBR-1.** `Guard` is the public epoch scope. `IdentRef<'g, T>` and witnesses are tied to `Guard` and cannot outlive it.

LINT: reject public `epoch::pin` spelling in architecture-facing APIs; reject `IdentRef` construction without a `Guard` lifetime.

**EBR-2.** `epoch::retire` records the retire epoch and invokes reclaim only after all guards that could have observed the slot have quiesced.

LINT: reclaim callbacks must be reachable only from epoch drain paths; direct slot free or slab-page return from semantic final-drop paths is invalid.

**EBR-3.** Epoch draining is bounded work, with pressure-triggered drain allowed but still budgeted.

LINT: reject unbounded destructor walks or full retired-list drains on hot final-drop paths; large reclaim items need split/requeue behavior.

---

## 5. SIG — Signals and Bus Publication

<!-- txdoc:INVARIANTS-SIG-1 -->

**SIG-1.** Signals are not truth. A signal fire does not authorize action; consumers re-observe through predicates.

LINT: code woken by bus must call a predicate/require/step before acting on semantic state.

**SIG-2.** Bus wires are not state. Stale, coalesced, dropped, and spurious wakeups are admissible.

LINT: reject branches that treat raw queue/port bits as semantic readiness without fresh observation.

**SIG-3.** Signal attachment is opt-in and declared per transition.

LINT: every bus fire from execution must reference a declared signal attachment row.

**SIG-4.** Signal publication occurs after the paired commit point's visibility boundary, within the same step.

LINT: fire calls must be dominated by the matching commit call; annotation may be needed for complex control flow.

**SIG-5.** Signal ordering is per-carrier, not global.

LINT: reject code relying on ordering across different carriers unless an explicit synchronization primitive is used.

**SIG-6.** Signal publication is not substrate mutation. Substrate linearizes writes; bus publishes hints.

LINT: substrate primitives should not fire semantic bus wires internally.

**SIG-7.** Signal publication must satisfy `BIF-5`: one carrier, one retention domain.

LINT: signal attachment declarations must name a single owner/capability.

**SIG-8.** Streaming operations publish per step, not once at operation end.

LINT: multi-step operations that report progress should fire declared progress/readiness signals before returning the progress outcome.

**SIG-9.** Wake delivery does not grant truth. Subscribers must re-establish truth under a fresh guard.

LINT: wake handlers must route into wait-loop recheck or equivalent require call.

**SIG-10.** Subscribers must not infer truth from bus wire state.

LINT: diagnostic bus peeks cannot feed authorization or readiness decisions.

**SIG-11.** Cross-capability firing within a subsystem is allowed if carrier ownership remains single-domain. Cross-subsystem firing must route through the target subsystem's commit path.

LINT: reject direct fire on another subsystem's wire unless through target `execution/` API or explicit reviewed annotation.

**SIG-12.** `AdvancedThenBlocked` may publish for its progress portion before returning.

LINT: composite outcomes should not suppress progress publication.

---

## 6. STEP — Execution Primitive

<!-- txdoc:INVARIANTS-STEP-1 -->

**STEP-1.** Execution is driven by the closed `StepOutcome` algebra: progress, blocked, progress-then-blocked, done, error.

LINT: step APIs return the canonical outcome type, not custom wait/error enums.

**STEP-2.** Steps are synchronous and bounded. They do not suspend, await, construct futures, or interact with the executor.

LINT: forbid `.await`, future construction, blocking I/O, or reactor wait calls inside `execution/step_*`.

**STEP-3.** Step progress is monotone with respect to committed state. Retry resumes from accumulated progress, not an earlier state.

LINT: no rollback of published progress; errors after progress should be deferred/reported per step contract.

**STEP-4.** Mutating steps follow fixed sub-phase order: observe, upgrade, reserve, commit, publish.

LINT: typestate APIs can enforce ordering; otherwise lint for commit before upgrade/reserve, fire before commit, and reservation leaks.

**STEP-5.** No-progress outcomes name their wake carrier and interest conditions.

LINT: `Blocked`/`AdvancedThenBlocked` constructors require carrier and interest mask/protocol.

**STEP-6.** There is no operation-level commit phase and no public `*_prepare` / `*_commit` operation split. Per-mutation commit remains inside a step.

LINT: flag public operation-level prepare/commit pairs; allow private same-step helpers by annotation or visibility.

**STEP-7.** Semantic decisions within a step are based on fresh require under the current guard. Prior observations do not authorize action.

LINT: step functions must call require or consume same-step witnesses before state-sensitive actions.

**STEP-8.** Reported progress is observable and irreversible after its visibility boundary.

LINT: after a progress commit, code must not attempt rollback; fallible work after reported progress must be structured carefully.

**STEP-9.** A step may both advance and declare blocking via `AdvancedThenBlocked`.

LINT: driver loops must accumulate progress before waiting on the named carrier.

---

## 7. ASYNC — Async, Reactor, and Wait Discipline

<!-- txdoc:INVARIANTS-ASYNC-1 -->

**ASYNC-1.** Async suspension is an orchestration boundary. Steps and predicates do not suspend; scripts, thread futures, waits, and protocol combinators may suspend only outside semantic mutation.

LINT: forbid `.await` in `checks/`, `predicates`, and `execution/step_*`; allow `.await` in scripts/reactor helpers by declared API boundary.

**ASYNC-2.** Polling a task future is exclusive. A runnable task is not polled concurrently by multiple reactor workers.

LINT: task state must expose a poll-ownership bit, lock, or equivalent executor guarantee.

**ASYNC-3.** Epoch guards, `IdentRef`, and witnesses never cross poll, yield, or await boundaries.

LINT: future state and async continuation structs cannot contain `Guard`, `IdentRef`, witness types, or borrowed structure views.

**ASYNC-4.** Wait registration is register-or-recheck safe. A waiter that observes a false predicate must either register atomically with the condition source or recheck after registration before parking.

LINT: wait adapters require the pattern `check -> register -> recheck -> park`, or a substrate primitive that proves the same linearization.

**ASYNC-5.** Wake is a hint, not truth. A resumed waiter re-evaluates the predicate or reruns the required step under a fresh guard before acting.

LINT: wake handlers and poll resumption paths must route to `require_*`, predicate recheck, or step retry before semantic action.

**ASYNC-6.** Future cancellation/drop releases wait registrations, operation-local reservations, and private staging resources, unless ownership has already been committed to a named semantic owner.

LINT: async state machines with registrations or reservations need `Drop` cleanup or typestate proving commit/transfer.

**ASYNC-7.** Only async-safe state crosses await: `Cap`, operational evidence, owned immutable snapshots, private staging objects, and linear reservations with cleanup.

LINT: continuation structs may store retention evidence and owned values, not guard-scoped evidence, raw structure pointers, or unowned borrowed state.

**ASYNC-8.** Runnable eligible tasks are eventually polled under a valid scheduler policy.

LINT: scheduler implementations must document fairness/eligibility assumptions; starvation-prone policies require an explicit exception.

**ASYNC-9.** Userspace preemption does not advance syscall script state. Script progress occurs only when the thread future is polled and steps commit progress.

LINT: preemption paths may set AST/preempt flags and enqueue tasks; they must not directly mutate script-local syscall state.

**ASYNC-10.** Every userspace-return boundary checks declared AST/signal/preemption delivery points before userspace resumes.

LINT: architecture-specific return-to-user paths must call the thread-runtime boundary hook or prove equivalent ordering.

---

## 8. SCRIPT — Syscall Composition

<!-- txdoc:INVARIANTS-SCRIPT-1 -->

**SCRIPT-1.** Scripts use the closed driver modes: nonblocking, waiting, selecting.

LINT: script drive helpers must select from canonical modes.

**SCRIPT-2.** Scripts are state-blind. They observe syscall metadata, step outcomes, wake events, and thread context; they do not read subsystem canonical state.

LINT: forbid imports from any subsystem `structure/` under `scripts/`.

**SCRIPT-3.** Scripts compose waits and steps; they do not define truth.

LINT: scripts should call `checks/` and `execution/`, not raw predicates or structure accessors.

**SCRIPT-4.** Script-phase classes are disjoint: observe, intercept, gate, wait-adapt, drive.

LINT: prelude observers cannot reject; gates cannot park; wait-adapt cannot authorize; drive cannot inspect state.

**SCRIPT-5.** Subsystem-internal publications are not script hooks.

LINT: scripts should not fire subsystem signal carriers directly.

**SCRIPT-6.** Drive logic sees semantics only through `StepOutcome`; subsystem-exposed handles must be opaque to scripts.

LINT: script-visible capability/handle types should expose no state-dependent fields.

**SCRIPT-PONR.** Scripts with a point of no return must mark the boundary and place recoverably fallible work before it.

LINT: `#[point_of_no_return]` blocks reject allocation, user-memory access, filesystem I/O, and fallible calls unless the call is marked fatal-only.

---

## 9. COMP — Completion Protocol Objects

<!-- txdoc:INVARIANTS-COMP-1 -->

**COMP-1.** Completion is wait middleware: `wait_event(private_channel, done_count > 0, protocol)`. The done predicate is semantic truth; the wake is only publication.

LINT: completion wait paths must re-read `done_count` or equivalent predicate after wake.

**COMP-2.** A producer release-publishes completed work before making completion observable; a waiter acquire-observes completion before consuming completed work.

LINT: completion APIs must use release/acquire ordering or a stronger reviewed synchronization primitive.

**COMP-3.** A one-shot completion consumes one completion credit per successful waiter unless declared broadcast.

LINT: completion `wait` implementations must make credit consumption explicit; broadcast-style APIs require a distinct type/name.

**COMP-4.** Countdown completion has a closed participant count after construction.

LINT: reject dynamic participant additions after countdown construction unless the type is a separate reviewed protocol.

**COMP-5.** Completion is not single-waiter ownership handoff and not a bus signal catalog entry.

LINT: use dedicated handoff primitives for ownership transfer; do not list completion private channels as semantic signal attachments.

---

## 10. ARCH — Architecture-Level Rules

<!-- txdoc:INVARIANTS-ARCH-1 -->

**ARCH-1.** The system has three planes: semantic, execution, publication. Bifurcation constrains all three.

LINT: new mechanisms should declare plane coordinates.

**ARCH-2.** Every mechanism has coordinates on plane, script-phase class, and middleware-vs-fixed axes.

LINT: new module/spec template requires coordinate declarations.

**ARCH-3.** Closed catalogs require architecture review to extend.

Closed catalogs:

- `StepOutcome` variants;
- driver modes;
- wait protocols;
- bus primitives;
- script-phase classes;
- architectural homes;
- conditional-commit primitive family.

LINT: new enum variants or primitive families require explicit `ARCH-3` review marker.

**ARCH-4.** Middleware is a protocol combinator over caller-supplied semantics. Hard-coded policy is not middleware.

LINT: modules named middleware/adapters must take semantic predicates/steps as parameters.

**ARCH-5.** Every derived materialization is justified by a currently-valid authoritative binding. Publication revalidates the justifying binding atomically with publication. Withdrawal invalidates dependents or prevents future publication against the withdrawn binding.

LINT: materialization publication APIs require `justified_by` binding argument/annotation; caches must declare authoritative source.

---

## 11. EXC — Publication Exclusions

<!-- txdoc:INVARIANTS-EXC-1 -->

**EXC-1.** Synchronous fault injection is not bus publication.

LINT: trap/fault delivery must use HAL/thread-runtime/signal fault path, not RawQueue/RawPort.

**EXC-2.** Cross-core barriers are not signals.

LINT: TLB shootdown/IPI synchronization must use synchronous coordination APIs, not bus wires.

**EXC-3.** Single-waiter handoff is not a publication-catalog signal.

LINT: one-waiter ownership protocols should use subsystem-owned handoff primitives, not general bus publication.

---

## 12. MAP — Placement and Import Boundaries

<!-- txdoc:INVARIANTS-MAP-1 -->

**MAP-1.** Every mechanism has one primary architectural home: foundation/HAL, substrate, reactor, scheduler policy, full subsystem, service, filesystem instance, script, shim, projection, or static registry.

LINT: module/spec template requires `home = ...`.

**MAP-2.** HAL/foundation sits below the object model and must not depend on semantic subsystems.

LINT: HAL crates/modules cannot import subsystem/service/script paths.

**MAP-2A.** HAL is an axHal-style static platform family: one platform selected at compile/link time, no runtime HAL manager, no `Box<dyn Hal>`, no HAL-owned semantic entities, and no HAL callback slot for subsystem policy.

LINT: reject `HalManager`, `dyn Hal`, `__ostd_main`, HAL signal-hook slots, or board-independent code that matches on runtime architecture instead of using the selected platform axis.

**MAP-2B.** Portable boot follows the platform-owned handoff shape:
firmware/reset enters the concrete platform crate's `_start`; `_start`
preserves raw firmware arguments and calls the board binary's `rust_entry`;
`tx_hal::entry::<P, K>` translates them through `BootPlatformIf` into
`BootHandoff`; generic `tx-kernel` receives only `P: TxPlatform` and
`BootHandoff`.

LINT: reject `_start`, linker-script ownership, DTB/register decoding, or
firmware protocol switches in generic kernel crates; reject board binaries that
grow beyond `ActivePlatform` selection plus `rust_entry`.

**MAP-3.** Substrate is semantic-free. It provides primitives, not errno policy or domain truth.

LINT: substrate modules cannot import subsystem entity types except generic traits/markers.

**MAP-4.** Reactor owns polling, wait, wake, preemption mechanism, and AST slots; scheduler owns policy.

LINT: scheduler code cannot poll tasks directly; reactor code cannot hard-code scheduling policy beyond trait calls.

**MAP-5.** Full semantic subsystems own their structure/checks/execution/project surfaces.

LINT: only subsystem `execution/` writes its `structure/`; external callers route through public APIs.

**MAP-6.** Service subsystems authorize/account/mutate their own ledgers but do not publish foreign subsystem objects.

LINT: service execution cannot call foreign index commit APIs for fd/mount/vm/process objects.

**MAP-7.** Filesystem instances are hosted by Mount and consumed by VFS/PageBacked; they do not own global path topology or RNodes.

LINT: fs backend modules cannot construct global DEntry/RNode bindings directly.

**MAP-8.** Scripts own sequencing, not durable truth.

LINT: scripts cannot define authoritative indexes or long-lived semantic objects.

**MAP-9.** Shims translate compatibility ABI onto native mechanisms and must not duplicate native truth.

LINT: shim state must be ABI table/routing state or explicitly justified; no shadow process/vm/vfs truth.

**MAP-10.** Projections render owner state and never authorize or mutate.

LINT: projection modules cannot call mutating APIs or require functions as authorization gates.

**MAP-11.** Static registries use `&'static` identity outside the zone/ref hierarchy; dynamic lifetime requires promotion.

LINT: static registry entries must not be wrapped in fake `Cap<T>`.

---

## 13. SVC — Service-Subsystem Patterns

<!-- txdoc:INVARIANTS-SVC-1 -->

**SVC-CRED-1.** Credential state is the authoritative basis for authorization within its scope.

LINT: grant-minting operations must name the credential basis used at publication.

**SVC-CRED-2.** Subsystem-local grants are derived materializations owned and published by the subsystem that installs them, not by cred.

LINT: cred service cannot install fd/open/mount/ptrace grants directly.

**SVC-RLIMIT-1.** Rlimit uses ledger plus stable usage counters plus operation-local reservations.

LINT: resource-consuming operations must reserve against usage counters, not rely only on stale snapshots.

**SVC-RLIMIT-2.** Rlimit reservations are linear and must be committed or dropped.

LINT: forbid `mem::forget` or leaked credit reservations outside substrate internals.

---

## 14. FS — Filesystem Instance Boundaries

<!-- txdoc:INVARIANTS-FS-1 -->

**FS-1.** Mount owns topology; VFS owns DEntry/RNode/OpenFile semantics; FS backends own backend object IDs and metadata.

LINT: backend code cannot walk global paths or mutate mount indexes.

**FS-2.** PageBacked owns page-indexed content behavior; FS backends provide `FsPageBacking` fetch/flush, not PTE management.

LINT: fs backend modules cannot call pmap/PTE APIs.

**FS-3.** Procfs/devfs/devpts are filesystem instances when mounted; their projections do not become separate truth stores.

LINT: projection filesystems cannot maintain shadow tables for owner state unless declared authoritative.

**FS-4.** Backend caches must be either derived materializations or explicitly promoted to authoritative bindings.

LINT: cache structures require `derived_from = ...` or `authoritative = true`.

---

## 15. MOUNT — Mount-Local Invariants

<!-- txdoc:INVARIANTS-MOUNT-1 -->

**MOUNT-1.** Mount attaches `MountIdentity` to a directory `DEntry` in a `MountNamespace`.

**MOUNT-2.** While attached, path resolution crossing that DEntry enters the mounted filesystem root.

**MOUNT-3.** A covered DEntry remains structurally present but its directory contents are hidden from path resolution.

**MOUNT-4.** `..` from a mounted root crosses back to the parent mount and mountpoint parent.

**MOUNT-5.** `stat.st_dev` changes across mount boundaries; each `MountPayload` supplies a stable dev id.

**MOUNT-6.** Active mountpoints reject unlink, rmdir, and ordinary rename with `EBUSY`.

**MOUNT-7.** Existing open files, cwd/root cursors, and in-flight resolvers are not retargeted by mount or umount.

**MOUNT-8.** Umount withdraws namespace reachability before dropping payload; detached-but-held mounts are valid degraded states.

**MOUNT-9.** Mount and umount are privileged operations.

**MOUNT-10.** Mount table is authoritative; `/proc/mounts` is a projection, not separate state.

**MOUNT-11.** v1 supports only normal mount plus normal/lazy umount; bind, move, propagation, and real namespaces are deferred.

**MOUNT-12.** Mount flags minimally include read-only, nosuid, nodev, noexec, and noatime where specified.

**MOUNT-UNPUBLISHED.** VFS objects created or mutated by `step_mount` phase-3 helpers remain unpublished until `mountpoint_index` installation succeeds.

LINT: mount preparation helpers return reservations/private objects; they cannot publish global DEntry/RNode/mount-table facts before mountpoint index commit.

**MOUNT-COVER.** When the walker resolves a component covered by an attached mount, it substitutes the mounted root before child lookup, hiding the covered DEntry's children.

LINT: VFS walker mount-crossing rule must run before child lookup under the covered directory.

**MOUNT-BDY-1.** Mount is the sole owner of mount topology: `mountpoint_index`, parent edges, mount-tree membership, and mount-table membership.

LINT: no peer subsystem may maintain a mount topology cache or mutate mount structure directly.

**MOUNT-BDY-2.** Other subsystems consume mount topology only through guard-scoped checks or stable Caps.

LINT: no long-lived raw mount pointers; no `IdentRef` across await; no cached traversal beyond step scope.

**MOUNT-BDY-3.** Mount hosts filesystem instances and supplies topology/viewpoint; it does not own file objects, process objects, drivers, or VM mappings.

LINT: mount modules cannot implement file I/O, process lifecycle, block I/O, terminal semantics, or PTE management.

---

## 16. EXEC — Exec Script Invariants

<!-- txdoc:INVARIANTS-EXEC-1 -->

**EXEC-PONR.** After phase 6, the address-space visibility boundary, exec performs no allocation, no user memory access, no filesystem I/O, and no recoverably fallible computation. Past-boundary kernel-detected failure is fatal process termination, not errno recovery.

LINT: mark post-PONR code; reject fallible APIs, allocation, usercopy, and filesystem reads unless annotated fatal-only/infallible.

**EXEC-DETACHED-AS.** The new address space is built detached, populated, and reduced to an infallible swap before the point of no return.

LINT: exec must not tear down the old address space before all fallible image/stack/table preparation succeeds.

**EXEC-COW-SHARED.** Exec prepares COW replacements for shared fd table and signal actions before installing them.

LINT: post-PONR fd/signal table install must be atomic store or infallible prepared-plan application.

**EXEC-TRACE-NOT-TRUTH.** `process_execd` trace publication is diagnostic; authoritative state is process payload and VM recipes.

LINT: ptrace/proc consumers must re-read process/VM state; trace event payload cannot authorize behavior.

---

## 17. VM / PageBacked Invariants

<!-- txdoc:INVARIANTS-VM-PAGEBACKED-1 -->

**VM-JUSTIFY-PTE.** For every visible PTE in address space `A` at VA `X`, `A.recipes.range_containing(X)` must return a `VmEntry` whose permissions and backing justify that PTE.

LINT: PTE install APIs require recipes binding re-read or RangeLock/slot-locked proof token.

**VM-RANGELOCK.** Operations that mutate recipes or publish PTEs over ranges coordinate overlapping ranges through RangeLock or an equivalent reviewed primitive.

LINT: mmap/munmap/mprotect/mremap/fault paths must hold required range token for overlapping recipe/PTE effects.

**PB-PC-AUTH.** Within PageBacked scope, `PageContainer.pages` is authoritative for materialized frames at offsets; PTEs and read buffers are derived from it.

LINT: page materialization must use install-if-absent/match style publication into the PC page index.

**PB-BACKING-SPLIT.** `RNodeBacking` determines file behavior: page-backed, struct-backed, or projected; no hidden per-inode vtable should bypass this classification.

LINT: reject new ad hoc `FileOps`/`InodeOps` dispatch paths not routed through declared backing/FS instance traits.

---

## 18. PROCESS / THREAD Invariants

<!-- txdoc:INVARIANTS-PROCESS-THREAD-1 -->

**PROC-ID-PAYLOAD.** Process identity persists through zombie; process payload drops at exit. Wait addressability and signal addressability are distinct projections.

LINT: `kill`/signal routing must require signal-addressable state; `wait` must use wait-addressable state.

**PROC-BIND-UPWARD.** Parent, pgrp, session, and controlling-tty bindings are authoritative identity-level bindings; derived DLLs/lists must be justified by them.

LINT: membership DLL updates must validate or be sequenced after the authoritative binding update.

**PROC-GROUPEXIT-EPISODE.** GroupExit is one-shot per episode, not process lifetime; exec collapse clears the episode after commit.

LINT: group-exit state must have explicit episode reset path.

**VIEW-1.** Canonical topology is owned by its semantic subsystem; view layers may resolve, filter, root, offset, interpret, or render that topology, but must not become co-owners of it.

LINT: reject a view/namespace object that stores authoritative parent, member, edge, or ownership lists for another subsystem's canonical graph.

**VIEW-2.** Syscall-facing operations decompose into view resolution, canonical semantic operation, and view rendering.

LINT: syscall scripts must keep `resolve_visible`, `operate_on_identity`, and `render_for_viewer` phases separate when namespace views are involved.

**VIEW-3.** View-owned bindings are authoritative only for the lens they introduce.

LINT: namespace maps, roots, offsets, visibility filters, and authority lenses must declare their owned binding domain and must not silently duplicate foreign owner state.

**VIEW-4.** Visibility holes are projection policy, not topology mutation.

LINT: code handling invisible parents, process groups, sessions, cgroups, mounts, or credentials must choose an explicit render/error policy instead of rewriting canonical relations.

**PID-1.** `PidNamespace` owns numeric signifier bindings for pid, tid, pgid, and sid lookup.

LINT: process/thread/group/session numeric lookup must route through `PidNamespace`, not ad hoc global maps.

**PID-2.** Namespace maps bind pid/tid/pgid/sid numbers to `Cap<PidName>` / `Cap<PidStruct>`, not directly to mixed target types.

LINT: reject separate direct-target `pid_map`, `tid_map`, `pgid_map`, and `sid_map` in the canonical model.

**PID-3.** A pid-name object may retain the semantic target as addressability evidence; target identities do not retain pid-name objects.

LINT: reject `Cap<PidName>` / `Cap<PidStruct>` fields inside `ProcessIdentity`, `ThreadIdentity`, `ProcessGroup`, or `Session`; allow non-retaining name snapshots.

**PID-4.** A pid number is withdrawn at the semantic object's POSIX-visible name-death point, not merely payload death.

LINT: process pid binding withdraws at reap, not process exit; pgrp/session names withdraw only when membership and retainers permit.

**PID-5.** Future nested namespace support extends `PidName` / `PidStruct.numbers`; it does not change target entity identity.

LINT: code must not assume `PidName` / `PidStruct.numbers.len() == 1` outside v1-gated paths.

**PID-6.** Pid numbers are view signifiers, not canonical topology. Parent, pgrp, and session bindings target identities, never pid numbers.

LINT: reject canonical `parent`, `pgrp`, or `session` fields storing pid/tid/pgid/sid integers as authority.

**PID-7.** Per-namespace process/session/pgrp trees are projections over canonical topology under a pid-namespace lens, not separately maintained trees.

LINT: reject namespace-local children/member DLLs unless they are explicitly declared derived projections with revalidation against canonical bindings.

**NSVIEW-1.** `nsproxy`-like bundles hold namespace references; they do not own the semantic objects exposed through those namespaces.

LINT: reject namespace-proxy fields that duplicate process, mount, IPC, network, cgroup, or clock state instead of retaining namespace objects.

**NSVIEW-1A.** `nsproxy`-like bundles are immutable after publication; changing namespace membership publishes a new bundle or reuses an existing compatible one.

LINT: reject in-place mutation of a published namespace-proxy field.

**NSVIEW-2.** A namespace may own bindings only for the domain it introduces. It must not maintain a shadow copy of another subsystem's canonical graph.

LINT: namespace-local registries must declare whether they are authoritative for their domain or derived projections over another owner.

**NSVIEW-3.** Syscall numeric inputs resolve through the caller's namespace lens; semantic checks and mutations act on canonical identities; results render back through the requested/viewer namespace.

LINT: syscall implementations must separate `resolve_number`, `operate_on_identity`, and `render_number` phases.

**THREAD-EXIT-MONOTONE.** Once a thread enters exiting, it cannot return to running, waiting, or stopped. `ThreadIdentity.payload.is_some()` is monotone true to false.

LINT: no transition from exiting/dead to runnable states.

**THREAD-FUTURE-LAYERING.** Steps are synchronous, scripts are async, and the thread future is long-lived async; these layers must not collapse.

LINT: step code cannot await; scripts may await; thread runtime owns the outer future.

**THREAD-SUMMARY-NOT-TRUTH.** Signal summary atomics are denormalized hints; authoritative pending state lives in pending queues and masks.

LINT: summary bits can fast-path checks but cannot replace pending/mask re-observation.

---

## 19. DEVICE / TTY Invariants

<!-- txdoc:INVARIANTS-DEVICE-TTY-1 -->

**DEV-TIER1-HAL.** Tier-1 devices live in HAL, have no devfs presence, and are reached through HAL traits.

LINT: early UART/timer/interrupt-controller code must not register VFS device nodes directly.

**DEV-TIER2-STATIC.** Tier-2 devices are static board-composed entries outside the zone/ref hierarchy.

LINT: static device entries use `&'static`, not fake `Cap<T>`.

**DEV-TIER3-DEFERRED.** Dynamic discovery/matching is deferred; adding it requires a real dynamic registry and lifecycle model.

LINT: hotplug/dynamic driver code requires explicit tier-3 feature gate.

**TTY-ID-PAYLOAD.** TTY uses `TtyIdentity` / `TtyPayload` factoring for dynamic ptys and line discipline state.

LINT: TTY namespace/addressability references target identity; operations requiring buffers/ldisc need payload evidence.

**TTY-CONTROLLING-BINDING.** A terminal's controlling session/pgrp binding is authoritative for job-control decisions.

LINT: job-control signal paths must re-read controlling binding/session/pgrp state, not cached ids alone.

**TTY-INGEST-LINEARIZER.** Deferred line-discipline side effects are serialized through the TTY-local ingest linearizer.

LINT: ldisc mutation paths cannot directly race with ingest; they must enqueue/compose through the linearizer.

---

## 20. Review Comment Templates

<!-- txdoc:INVARIANTS-REVIEW-TEMPLATES-1 -->

Use these directly in code review:

- `BIF-3`: This binding targets payload directly; addressability bindings must target identity.
- `PRED-1`: This predicate mutates or blocks; checks must stay pure.
- `WIT-4`: This witness escapes into long-lived state.
- `OBL-2`: Binding evidence does not match its declared obligation.
- `ZONE-4`: This weak handle is being used as retention or addressability evidence.
- `ZONE-6`: This upgrade is split across separate checks instead of one generation/state/retain CAS.
- `ZONE-8`: This final-drop path destructs or reuses storage before epoch quiescence.
- `EBR-1`: This guard-scoped reference is constructed or stored without a valid `Guard` lifetime.
- `SIG-1`: This code treats a wake/signal as truth without fresh observation.
- `STEP-2`: This step can suspend or block.
- `STEP-4`: Commit/publish ordering does not follow observe-upgrade-reserve-commit-publish.
- `ASYNC-4`: This wait can lose a wake because it parks without register-or-recheck safety.
- `ASYNC-6`: This async cancellation path leaks a wait registration or operation-local reservation.
- `SCRIPT-2`: Script code is reading subsystem structure directly.
- `COMP-1`: This completion consumer treats wake as truth instead of rechecking completion state.
- `ARCH-5`: This materialization is published without naming and revalidating its authoritative binding.
- `MAP-6`: Service code is publishing a foreign subsystem object.
- `MOUNT-BDY-1`: This creates a second owner/cache of mount topology.
- `EXEC-PONR`: This post-PONR path contains recoverably fallible work.
- `PID-3`: This creates a retention cycle or duplicate authority between pid name and target identity.
- `PID-6`: This makes a pid number authoritative topology instead of a namespace view.
- `NSVIEW-3`: This mixes namespace-number resolution with canonical semantic mutation.
