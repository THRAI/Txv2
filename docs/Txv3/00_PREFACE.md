# txKernel v3 — Preface

<!-- txdoc:TXV3-PREFACE -->

**Status.** v3 design refresh, 2026-05.
**Audience.** New contributors; reviewers evaluating architectural fitness; anyone reasoning about whether a proposed feature lands within or outside the framework.
**Companion documents.** All Txv3 docs.

---

## 1. What this kernel is

<!-- txdoc:TXV3-PREFACE-WHAT-1 -->

txKernel is an architectural attempt to **re-explain POSIX in its own terms** — to give the syscall surface a closed, named vocabulary for the things POSIX has always assumed but never specified. It is not a new abstraction layer above or below POSIX; it is POSIX as it would be written if it were written down.

The kernel commits to five primitive cells:

| Cell | What it names |
|---|---|
| **SubjectContext** | who the caller is — identity, authority, restrictions |
| **Signifier Resolution** | which entity does this user-visible name reach right now (path → inode, fd → file, pid → process, vaddr → recipe) |
| **StepOp + StepOutcome** | the bounded synchronous transaction that observes, mutates, and either completes or yields |
| **YieldShape** | the closed set of things the kernel may wait on (carrier, agent, edge, handoff) |
| **Publication / Projection** | what becomes externally visible after a commit (signals, /proc, fanotify events, traces) |

Every POSIX syscall is one or more StepOps composed by a script over a SubjectContext, possibly yielding through a closed YieldShape, with effects published per the publication rule.

These five cells are not a layer; they are the conceptual structure POSIX itself has, exposed as kernel vocabulary. Linux implements POSIX as accreted mechanism (kstack, locks, RCU, refcount, dentry, file, inode, …); txKernel implements POSIX as named structure. The mechanism is then derivable from the structure rather than the other way around.

## 2. What this kernel is not

<!-- txdoc:TXV3-PREFACE-NOT-1 -->

txKernel is **not** a microkernel: subsystems share an address space and there is no IPC boundary between them. The audit story protects against logical bugs, not against compromised drivers.

txKernel is **not** a capability OS: signifier resolution and authority check remain two distinct phases, governed by SubjectContext, exactly as POSIX specifies. Caps in this kernel are an *implementation* of POSIX retention semantics, not a replacement of POSIX naming.

txKernel is **not** a verified TCB: there is no formal proof of correctness. Correctness is structural — every invariant maps to an enforceable lint or a closed-catalog membership rule — but it is not theorem-proven. Workloads needing seL4-grade assurance should use seL4.

txKernel is **not** Plan 9: no metaphor purity ("everything is a file"). Signals, mmap, fork, pthread are first-class POSIX features, not file operations.

txKernel is **not** a re-imagining of POSIX: there is no intent to fix POSIX's well-known infelicities (the wait4/wait3/waitid divergence, the SIGCHLD deferral mess, the sigaltstack/exec interaction). Those are POSIX, faithfully.

## 3. Comparative position

<!-- txdoc:TXV3-PREFACE-COMPARATIVE-1 -->

Each kernel family chose a different relationship to POSIX-as-contract. txKernel's choice is in the rightmost column.

| Family | Decision | What got rebuilt | What got lost |
|---|---|---|---|
| Traditional (Linux/BSD) | "POSIX is what the syscall ABI says; the kernel is whatever passes the test." | Internal vocabulary became a grab-bag of mechanisms accreted over decades. | Conceptual coherence; auditability is by intuition not structure. |
| Microkernel (L4 family) | "POSIX is too messy to be in the kernel; make it a server." | Kernel does threads + IPC + addresses; POSIX is a personality (L4Linux, Mach BSD). | Single-process-image performance, in practice POSIX fidelity. |
| Plan 9 | "POSIX is too broad; collapse it to one mechanism." | Everything is a 9P file. | Anything that doesn't fit the file metaphor (rich signals, mmap, fork+fd, pthread). |
| Cap OS (KeyKOS / EROS / partly Zircon) | "POSIX's signifier→authority check is wrong; collapse them." | Naming and authorization fuse; fds *are* caps. | Linux ABI compatibility; "name" itself is rebuilt. |
| **txKernel** | **"POSIX is approximately right but was never spelled out; spell it out."** | **A closed vocabulary that names what POSIX already does.** | **The escape-hatch wins of the alternatives.** |

txKernel competes in the *honest implementation of mainline POSIX* lane — possibly the most underserved of those positions, since every traditional POSIX kernel treats its internal vocabulary as private implementation matter rather than public structure.

## 4. What this position costs

<!-- txdoc:TXV3-PREFACE-COSTS-1 -->

Honest accounting of what's given up:

- **No verified-TCB story.** seL4 sells correctness-by-proof; txKernel sells correctness-by-structure. Different markets.
- **No microkernel-flavored isolation.** A FUSE crash takes down the FUSE daemon, not the kernel; a userfaultfd misbehavior livelocks one fault, not the system. But subsystems are not trust boundaries against each other.
- **No "everything is a file" simplicity.** Plan 9's metaphor purity is given up to keep Linux ABI fidelity.
- **No cap-as-name shortcut.** Path resolution and authority check remain two phases, paid in code volume to preserve POSIX semantics.
- **POSIX is itself a moving target.** Faithfully naming POSIX means tracking what POSIX-as-Linux-ships-it becomes. The closed-catalog gate is the discipline that keeps this honest, but the catalog will grow.

These costs are real. None is fatal to the value proposition; they delimit the lane.

## 5. The architectural-extension protocol

<!-- txdoc:TXV3-PREFACE-EXTENSION-PROTOCOL-1 -->

The framework's evolution mechanism is **closed-catalog extension, not subsystem rewrite**. When a new POSIX-adjacent feature lands, the protocol is:

1. **Name the concept.** What POSIX-shaped thing does this feature do?
2. **Classify under a cell.** Does it speak about subject, signifier, step, yield, or publication?
3. **Propose the closed-catalog member.** New `YieldShape`, new `RestrictionKind`, new `WaitProtocol`, etc.
4. **State substrate cost.** Which existing primitives are extended; what new primitives are added; what zone footprint is implied.
5. **State invariants.** Which existing invariants are preserved; which new invariants are added; how lints encode them.

Each step is a deliverable. Step 5 is the gate — a feature that can't articulate its invariants doesn't enter the framework. The protocol is what keeps the framework's structure stable as features accumulate.

## 6. The current closed catalogs

<!-- txdoc:TXV3-PREFACE-CATALOGS-1 -->

As of v3:

| Catalog | Members |
|---|---|
| Architectural homes | foundation/HAL, substrate, reactor, scheduler-policy, semantic-subsystem, service-subsystem, filesystem-instance, script, shim, view/projection, static-registry |
| Reference strengths | Weak, IdentRef, Cap, OperationalEvidence (deferred: Owned for transferable handoff) |
| Binding obligations | ResolutionOnly, Addressability, Operational |
| Step-phase classes | Observe, Intercept, Gate, Yield-adapt (was Wait-adapt), Drive |
| StepOutcome variants | Continue, Yield, Done, Err |
| YieldShape members | OnCarrier, OnAgent (deferred: OnEdge, OnHandoff) |
| ExecutionScope kinds | Thread (default), OnBehalfOf (deferred: future scopes) |
| Wait protocols | Uninterruptible, Interruptible, Killable, InterruptibleTimeout, KillableTimeout |
| Wait outcomes | Ready, Interrupted, Killed, TimedOut |
| Bus primitives | RawQueue, RawPort, RawTrace |
| Driver modes | Nonblocking, Waiting, Selecting |
| Restriction kinds | (deferred: SeccompFilter, LandlockRule, LsmStack) |
| Publication exclusions | synchronous fault injection, cross-core barriers, single-waiter handoff |
| Conditional-commit primitive family | reservation+commit substrate ops |

Extending any of these requires architecture review under the protocol in §5.

## 7. Reading orders

<!-- txdoc:TXV3-PREFACE-READING-ORDERS-1 -->

**For a new contributor.** This preface → `01_CONCEPTS_v5` → `02_INVARIANTS_v5` (skim) → `03_STEP_MODEL_v2` → `04_SYSCALL_SHAPE_v1`. Then pick a feature doc.

**For a feature designer proposing a new YieldShape or ExecutionScope.** This preface §5 → `01_CONCEPTS_v5 §closed-catalogs` → `05_DELEGATE_v1` (worked example of a new YieldShape) → `06_EXECUTION_SCOPE_v1` (worked example of a new ExecutionScope) → write your ADR following the same shape.

**For a reviewer evaluating an in-flight subsystem.** `02_INVARIANTS_v5` (canonical) → `03_STEP_MODEL_v2 §anti-patterns` → `04_SYSCALL_SHAPE_v1` (upper/lower split discipline) → existing v4 subsystem doc.

**For migration planning.** `07_BLAST_RADIUS` → identify which of the v4 docs are touched → pick a landing order from §"the order I'd land it."

## 8. Relationship to v4 docs

<!-- txdoc:TXV3-PREFACE-V4-RELATIONSHIP-1 -->

The v3 docs (this folder) supersede the parts of v4 that they touch:

| v4 doc | v3 status |
|---|---|
| `00_meta-framework/CONCEPTS_v4.md` | superseded by `01_CONCEPTS_v5.md` |
| `00_meta-framework/INVARIANTS_v4.md` | superseded by `02_INVARIANTS_v5.md` |
| `02_execution/STEP_MODEL_v1.md` | superseded by `03_STEP_MODEL_v2.md` |
| everything else | still canonical; cross-references in their text become v5 references on next routine edit |

The relationship is staged: v3 docs are the new spine; v4 subsystem docs continue to apply, with the substitution that `Wait-adapt` reads as `Yield-adapt`, that `StepOutcome` has four variants, and that script entry establishes a `SubjectContext`. No subsystem code is yet broken by these renames; the migration is mechanical (see `07_BLAST_RADIUS`).
