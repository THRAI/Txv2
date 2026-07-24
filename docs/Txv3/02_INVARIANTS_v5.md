# Invariants — v5

<!-- txdoc:TXV3-INVARIANTS-V5 -->

**Status.** v5 (Txv3 refresh, 2026-05).
**Supersedes.** `00_meta-framework/INVARIANTS_v4.md`. v5 carries forward all v4 invariant families unchanged unless explicitly modified, and introduces five new families: SUBJ-*, YIELD-*, DELEGATE-*, SCOPE-*, LANE-*.
**Audience.** Subsystem authors, reviewers, lint-infrastructure authors. Each invariant has a stable identifier; lints cite invariants by identifier.

---

## How to read this file

Each invariant has the form `FAMILY-N: short rule`. Long-form rationale and enforcement notes follow inline. Lints cite the identifier in violation messages. New invariants land here; refinements to wording bump the section header version (currently v5).

Catalog of families:

| Family | Concern | Source |
|---|---|---|
| BIF-* | bifurcation: identity ⊥ payload | v4, preserved |
| PRED-* | predicates and require-discipline | v4, preserved |
| WIT-* | witness scope | v4, extended (yield boundary added) |
| OBL-* | binding obligations | v4, preserved |
| SIG-* | signal attachments and publication | v4, preserved |
| STEP-* | step model | v4, **updated for four-variant outcome** |
| ASYNC-* | async / poll boundaries | v4, preserved |
| SCRIPT-* | script discipline | v4, **extended with upper/lower split** |
| COMP-* | completion | v4, preserved |
| ARCH-* | architectural-extension protocol | v4, preserved |
| EXC-* | publication carve-outs | v4, preserved |
| EBR-* | epoch / reclamation | v4, preserved |
| ZONE-* | zone slot discipline | v4, preserved |
| **SUBJ-*** | **SubjectContext** | **v5, new** |
| **YIELD-*** | **YieldShape discipline** | **v5, new** |
| **DELEGATE-*** | **OnAgent yield** | **v5, new** |
| **SCOPE-*** | **ExecutionScope** | **v5, new** |
| **LANE-*** | **semantic owner API lanes** | **v5, new** |
| PID-*, NSVIEW-*, MAP-*, FS-*, etc. | subsystem-specific | v4, preserved |

---

## SUBJ — SubjectContext (new in v5)

<!-- txdoc:INV-V5-SUBJ -->

SUBJ-1. **Every script frame has exactly one SubjectContext.** A script's identity context is a script-frame quantity, not a thread-local. Helpers that need subject authority take `&SubjectContext` explicitly.

SUBJ-2. **A SubjectContext is established at script entry.** Either by (a) materialization from the running thread's task (`SubjectContext::from_thread`), or (b) borrowing from another process via an `ExecutionScope::OnBehalfOf` (`SubjectContext::borrowed`). The borrow's lifetime bounds the script's lifetime.

SUBJ-3. **`SubjectContext::authority` is replaceable only via cred-service transition commits.** Replacement is a publication boundary. Observers see the old or the new authority, not an intermediate.

SUBJ-4. **The upper half of a script (signifier/identity/authority work) and the lower half (payload transition) compose typed `StepOp`s identically.** Both halves use `StepOutcome` / `YieldShape` / `drive`. The split is about concerns, not mechanism.

SUBJ-5. **A YieldShape resolution does not change the script's SubjectContext.** Authority transfer requires a SUBJ-3 commit; agent replies (DELEGATE-*) are reply *content*, not authority transfer. Any reply that conveys what looks like authority (e.g., a fd injection) is reified through the receiving step's normal upper-half check against the script's existing SubjectContext.

SUBJ-6. **`SubjectAuthority::restrictions` is append-only for the script's duration.** Restrictions are added at clone3 and across cred transitions; they are never removed. `no_new_privs` is the structural rule that this is so.

SUBJ-7. **Restriction-stack walks happen in upper-half observe phases.** The walk produces an `Allow | Deny(Errno) | Trap(endpoint)` outcome. `Trap` becomes a `YieldShape::OnAgent` to the registered tracer/daemon (seccomp-trap, fanotify FAN_OPEN_PERM, LSM userspace mediation). The trap path is the same OnAgent shape used by Delegate; no new mechanism.

---

## YIELD — YieldShape discipline (new in v5)

<!-- txdoc:INV-V5-YIELD -->

YIELD-1. **YieldShape is a closed catalog.** Extension requires architecture review under ARCH-3.

YIELD-2. **YieldShape payload may contain only:**
  - (a) `'static` retention evidence (`Cap<T>`, `OperationalEvidence<T>`, `PayloadCap<T>`),
  - (b) operation-local owned values (request descriptors, deadlines, cancellation tokens),
  - (c) replayable signifiers (path strings, fd numbers, addresses validated by the receiving subsystem on resume).

YIELD-3. **YieldShape payload must not contain:**
  - (a) witnesses or any `IdentRef<'g, T>` (observation evidence),
  - (b) reservation guards (RAII-rollback values),
  - (c) `epoch::Guard` or any guard-bound type.

YIELD-4. **A yield's reserve-phase publishes only after any preceding `progress` has been committed.** A `Yield { progress, shape }` outcome with non-empty progress means the progress has been committed and published *before* the yield's reserve-phase commits. Yielding with uncommitted progress in the carry is a STEP-4 violation.

YIELD-5. **Resumption from any YieldShape requires a fresh epoch guard and a fresh `require_*` walk.** The wake/reply itself is not truth; the next step invocation under fresh guard establishes truth (see WIT-3+).

YIELD-6. **Each YieldShape declares its substrate cost in its specifying doc.** New zone primitives, new bus carriers, new rlimit dimensions, and new scheduler hooks must be enumerated.

YIELD-7. **A YieldShape resolution does not change SubjectContext (see SUBJ-5).**

YIELD-8. **A reservation must be either consumed by commit before yield, or explicitly rolled back before yield.** Reservations may not cross yield boundaries; "carry reservation across yield, commit on resume" is forbidden.

YIELD-9. **Driver modes declare which YieldShape variants they accept.** A script that returns a yield shape its driver mode does not accept is a `DriveMode::handle` translation: `Nonblocking` returns `EAGAIN` (no progress) or partial-Ok (progress > EMPTY); modes-without-handler for the shape return `EOPNOTSUPP` (POSIX convention for "operation not supported on this object/mode").

YIELD-10. **`PreparedWaitRegistration` is permitted as `OnWaitSource` payload only if its `PreparedPredicate` satisfies WAIT-2.** That is: the still-blocked predicate must be non-blocking, non-allocating, atomic-load-only, must not acquire locks, must not access user memory, and must not call subsystem callbacks. This forecloses `PreparedPredicate` from becoming a hidden semantic-callback channel through the yield path.

YIELD-11. **Deadlines are `WaitProtocol` attachments, not `YieldShape` fields.** `OnAgent` carries no `deadline` field; timeouts are realized by a driver-installed `TimerGuard` regardless of primary shape. Composing `OnTimer` with `WaitProtocol.deadline` is invalid (the primary timer is itself the deadline).

---

## DELEGATE — OnAgent yield (new in v5)

<!-- txdoc:INV-V5-DELEGATE -->

DELEGATE-1. **A delegate endpoint reverses the normal authority direction; the agent answers a kernel question.** Delegation does not transfer process-identity authority to the agent and does not authorize the agent to execute syscalls on the script's behalf. (Authority on behalf is `ExecutionScope::OnBehalfOf` and is governed by SCOPE-*.)

DELEGATE-2. **Endpoints are typed.** A `Cap<DelegateEndpoint<K>>` constrains the legal `DelegateRequest` and `DelegateReply` shapes via the type parameter `K` (e.g., `Ufd`, `Fuse`, `Ptrace`, `FanotifyPerm`). No covert channel between endpoint kinds.

DELEGATE-3. **A delegation token has both a slot lifecycle and a logical state machine.** The slot lifecycle (substrate): `Cap<DelegateToken>` retain count; `SENTINEL_DEAD` on final drop; EBR reclamation of bytes. The logical lifecycle (runtime): `DelegateState` transitions `Pending → ReplyInstalling → Replied` (or `Pending → Canceled / AgentDied / TimedOut`). The two are independent: the token may reach `Replied` while the slot is still pinned by other holders; the slot reaches `SENTINEL_DEAD` only when no `Cap` retains it. **A reply against any non-`Pending` state is rejected as late and has no observable effect on the script** — regardless of slot lifecycle.

DELEGATE-4. **`DelegateReply::fd_injections` is capability transfer.** Each injection is checked at the receiving step's resume-side `require_*` for (a) endpoint-kind authority to inject, (b) cred check against the receiving SubjectContext, (c) `RLIMIT_NOFILE` reservation in the script's resume reserve-phase. Injection failure → `Agent::Refused` or a fresh errno class.

DELEGATE-5. **Per-endpoint in-flight-token cap is an rlimit dimension.** A new `RLIMIT_DELEGATE` (or borrowed `RLIMIT_NOFILE` charge) bounds outstanding delegations per script-side process. Reservation in the step's reserve-phase, sign at publish, drop at resume / timeout / cancel.

DELEGATE-6. **Cancellation has two orthogonal closed catalogs.** `AgentCancelPolicy` (`BestEffort | Synchronous | Detached`) controls the agent-side protocol when the kernel cancels a delegated request and is carried in `YieldShape::OnAgent::cancel`. `TokenDropPolicy` (`CancelOnDrop | Abandon`, reserved `KeepAlive`) controls what `ActiveWait` drop does to the token and is held internally by `AgentTokenGuard`. The two compose: `CancelOnDrop + Synchronous` means "on drop, cancel and wait for agent ack." `TokenDropPolicy::from_agent_cancel` derives the drop policy from the agent-cancel policy at `prepare_active_wait` time.

DELEGATE-7. **Nested delegation has a maximum depth.** Each delegation increments the script's resume-state nesting counter; exhaustion fails with `EDELEGLOOP`. The bound is a build-time constant chosen with adversarial-agent assumptions.

DELEGATE-8. **Endpoint scope is declared as `Thread` or `Process`.** Per-thread endpoints are required for ptrace's per-tracer-thread cases; per-process endpoints are natural for FUSE/userfaultfd. The endpoint type carries a `Scope` discriminator.

DELEGATE-9. **`DelegateReply::continuation` is typed.** Closed sum: `Final | Partial { next_token, accumulated } | Streamed { stream_handle }`. No free-form opaque continuation state.

---

## SCOPE — ExecutionScope (new in v5)

<!-- txdoc:INV-V5-SCOPE -->

SCOPE-1. **ExecutionScope is a script-context modifier, not a YieldShape.** A scope is extent-shaped; a yield is point-shaped. The two compose orthogonally.

SCOPE-2. **`OnBehalfOf<P>` borrows `Cap<ProcessIdentity>`.** The borrow holds the identity Cap for the scope's lifetime. The scope additionally subscribes to the borrowed process's `exit_source` (a `WaitSource`).

SCOPE-3. **Scope abandonment is delivered through the existing Killable wait protocol.** When the borrowed process exits (or its borrow scope is otherwise revoked), the script's drive loop observes a Killed outcome at its next yield and aborts with `EOWNERDEAD`.

SCOPE-4. **Subsystem authority lookups inside `OnBehalfOf<P>` resolve against `P`.** Files, vm, cred, rlimit are all charged against the borrowed identity, not the running task. Predicates that take `&SubjectContext` see the borrowed subject.

SCOPE-5. **Resources held inside an OnBehalfOf scope must not outlive the scope.** Fixed-buffer pins, in-flight requests, registered subscriptions are all scope-bounded; their drop is part of the scope's drop. A long-lived pin requires a longer-lived scope.

SCOPE-6. **A kernel task may enter at most one OnBehalfOf scope at a time.** Nested OnBehalfOf is forbidden; instead, the task's scope is replaced (with an explicit transition, dropping all scope-held resources).

SCOPE-7. **`current_subject_context()` does not exist.** Helpers receive `&SubjectContext` by parameter; SUBJ-1 is the wider statement of which this is the SCOPE-specific consequence.

---

## STEP — updated for four-variant outcome

<!-- txdoc:INV-V5-STEP -->

STEP-1. **The step outcome algebra is a closed four-variant sum: `Continue { progress }` | `Yield { progress, shape }` | `Done(T)` | `Err(Errno)`.** The five-variant v4 algebra (`Advanced` / `Blocked` / `AdvancedThenBlocked` / `Done` / `Err`) is retired. New yield primitives plug into `YieldShape`, not into the StepOutcome enum.

STEP-2. **Steps are synchronous and bounded.** A step does not `.await`, construct futures, park tasks, or call the executor. Bounded latency is per-subsystem-convention; the canonical bounds are stated in the subsystem's `execution/` doc.

STEP-3. **`StepProgress` is a monoid.** `(Self, EMPTY, extend)` is monoid-shaped: associative, with EMPTY as identity. `extend` is in-place. Progress is monotone: after `extend`, the result is `≥` both inputs in whatever per-type ordering applies.

STEP-4. **Mutating steps follow the five-stage discipline:** observe → upgrade → reserve → commit → publish. Skipping or reordering is a STEP-4 violation. Detail in `03_STEP_MODEL_v2 §4`.

STEP-5 through STEP-10. **Preserved from v4** with mechanical rephrase to refer to the four-variant outcome and the typed StepOp trait. See `03_STEP_MODEL_v2 §10` for the antipattern catalog (A-1 through A-15) that operationalizes them.

STEP-11. **A OneShotStepOp terminates on its first step invocation.** The first call to `step()` must return `Done(T)` or `Err(Errno)`. Returning `Continue` or `Yield` is a kernel invariant violation, not a user-visible `EAGAIN`. This is stronger than the `Nonblocking` driver mode (which translates unexpected yields to `EAGAIN`). Detail in `03_STEP_MODEL_v2 §5.3`.

STEP-12. **A OneShotStepOp has no resume protocol.** A `OneShotStepOp` must not depend on `apply_resume`, `WaitProtocol`, `ActiveWait`, or `DriverMode` translation. It does not register on a `WaitSource` or `DelegateEndpoint`. Its `step()` body must be a single synchronous guard-scoped region that terminates without external notification.

---

## SCRIPT — extended with upper/lower split

<!-- txdoc:INV-V5-SCRIPT -->

SCRIPT-1 through SCRIPT-N. **Preserved from v4.** Scripts own sequencing; do not own truth; do not inspect subsystem `structure/`; do not store witnesses; do not define authoritative indexes.

SCRIPT-V5-1. **A script's upper half does signifier resolution / authority check / restriction-stack walk over `&SubjectContext`.** The lower half does payload transition over object payload. Both halves are lists of typed `StepOp`s composed by drive; both may yield.

SCRIPT-V5-2. **The upper half terminates with one of: a successful `Cap<T>` for the resolved object, a typed errno, or a yield.** Lower-half StepOps consume the upper half's output as their constructor input.

SCRIPT-V5-3. **A point-of-no-return (e.g., execve PoNR) marks where authority replacement (SUBJ-3) crosses, after which lower-half failure must be fatal-only or explicitly deferred.**

SCRIPT-V5-4. **Every syscall entry is classified into exactly one dispatch lane.** The three lanes are `ImmediateSyscall` (no StepOp, no drive, non-yielding by construction), `OneShotStepOp` (StepOp with drive_oneshot, terminates on first step), and `FullDriveScript` (async drive, may yield). The classification is explicit at the dispatch site. Detail in `04_SYSCALL_SHAPE_v1 §6`.

SCRIPT-V5-5. **An ImmediateSyscall body must not call drive, drive_oneshot, construct a StepOutcome or YieldShape, or enter any helper that may yield.** An `ImmediateSyscall` may acquire short-lived guards for reading subject/process state; the guard must be dropped before return and no guard handle may escape the call. Lint rule: `ImmediateSyscall::call()` bodies flagged for `.await`, `drive(`, `drive_oneshot(`, `StepOutcome`, `YieldShape`, `WaitSource`, `DelegateEndpoint`, or VFS/VM resolution calls.

---

## WIT — extended with yield boundary

<!-- txdoc:INV-V5-WIT -->

WIT-1, WIT-2. **Preserved from v4.** A witness is guard-scoped observation evidence carrying `IdentRef<'g, T>`.

WIT-3. **Witnesses must not cross step boundaries.** *(Preserved from v4.)*

WIT-4. **Witnesses must not be stored in `self`-fields, returned in `StepOutcome`, passed to other threads, or held across `.await` points.** *(Preserved from v4.)*

WIT-5. **Witnesses must not cross yield boundaries.** *(New in v5; subsumes the v4 "must not cross await" wording with the explicit yield-shape boundary.)* Resume from any YieldShape requires fresh `require_*` under a fresh guard.

WIT-6. **Reservation guards are not witnesses but are subject to the same yield-boundary prohibition.** *(New in v5; YIELD-8 is the operational form.)*

---

## LANE — semantic owner API lanes (new in v5)

<!-- txdoc:INV-V5-LANE -->

LANE-1. **A semantic owner/root exposes only the lanes it supports.** The shared
catalog is `BindingLane`, `ProjectionLane`, and `ReadinessLane`; there is no
universal domain-object or generic CRUD trait.

LANE-2. **Lane results are domain-shaped and backend-opaque.** Public results
must not expose raw container nodes, lock guards, atomics, zone policies,
substrate reservations, RCU roots, source IDs, or mailbox registries.

LANE-3. **Binding and projection callers own the epoch guard.** A lane receives
the caller's guard and must not create a hidden nested guard. Guard-scoped
results obey WIT-* and YIELD-*.

LANE-4. **Binding observation does not silently acquire retention.** Crossing a
step or yield boundary requires explicit upgrade to `Cap<T>` or
`T::OperationalEvidence`, or an owned copy whose semantics permit replay.

LANE-5. **A successful outer reservation makes commit infallible and bounded.**
The reservation owns every fallible substrate resource, rolls back on Drop,
and never crosses a step or yield boundary.

LANE-6. **Projection is descriptive, not authoritative.** Projection rows do
not authorize operations, substitute for binding witnesses, mutate owner
state, or install waits.

LANE-7. **Readiness reports expose level state and opaque endpoints, not wake
truth.** A wake requires a fresh readiness/binding observation and carries no
guard-scoped evidence.

LANE-8. **Visibility precedes notification.** An owner publishes a new binding
or snapshot before firing any endpoint, bus attachment, completion, or signal
derived from that transition.

LANE-9. **Zone identity and publication storage are orthogonal.** Semantic
entities use role-shaped zone evidence; binding values are container-owned;
observer nodes and published roots have no public `Cap` or `Weak` identity.

LANE-10. **RCU is confined to publication implementation.** Raw atomic roots
and retirement calls are allowed only in reviewed epoch, zone, and publication
internals; owner facades remain unchanged when their backend migrates.

---

## ASYNC — preserved from v4

<!-- txdoc:INV-V5-ASYNC -->

ASYNC-1 through ASYNC-N. **Preserved from v4.** No epoch guard across `.await`; no nested guards; reactor poll boundaries are safe points; userspace preemption does not advance the thread future.

---

## ARCH — preserved from v4

<!-- txdoc:INV-V5-ARCH -->

ARCH-1 through ARCH-5. **Preserved from v4.** ARCH-3 (closed-catalog extension review) governs all v5 catalog additions, including new YieldShape members, new ExecutionScope kinds, new restriction kinds, and new bus primitives.

---

## EBR / ZONE — preserved from v4

<!-- txdoc:INV-V5-EBR -->

All v4 EBR-* and ZONE-* invariants hold unchanged. Notably:

- EBR-6: Guards cannot be nested.
- EBR-7: Guards are `!Send`, `!Sync`.
- EBR-8: IRQ handlers do not create Guards.
- ZONE-3: Old `Weak<T>` cannot observe a reused slot (generation tag).

---

## OBL, PRED, BIF, SIG, COMP, EXC — preserved from v4

<!-- txdoc:INV-V5-V4-PRESERVED -->

All v4 invariants in these families hold unchanged in v5. Cross-references in their text that mention `Wait-adapt` should be read as `Yield-adapt`; references to the five-variant `StepOutcome` should be read as the four-variant outcome with the extra information now carried by `YieldShape`.

---

## v5 changelog

<!-- txdoc:INV-V5-CHANGELOG -->

| Family | Change |
|---|---|
| SUBJ | new family, 7 invariants |
| YIELD | new family, 9 invariants |
| DELEGATE | new family, 9 invariants |
| SCOPE | new family, 7 invariants |
| LANE | new family, 10 invariants |
| STEP-1 | rephrased over four-variant outcome |
| STEP-3 | rephrased over `StepProgress` monoid |
| SCRIPT-V5-1..3 | new sub-rules for upper/lower split |
| WIT-5, WIT-6 | new sub-rules for yield boundary |
| all other v4 invariants | preserved unchanged |
