# txKernel v3 — Design Refresh

<!-- txdoc:TXV3-INDEX -->

**Status.** v3 design refresh, 2026-05.
**Scope.** This folder is the v3 architectural spine: the foundational concepts, invariants, step model, syscall shape, and the two new closed-catalog primitives (Delegate, ExecutionScope) that close the framework over current Linux ABI. Together with a migration plan that quantifies the cost.

---

## 1. The docs

<!-- txdoc:TXV3-INDEX-DOCS-1 -->

| # | Doc | Purpose |
|---|---|---|
| 00 | [`00_PREFACE.md`](00_PREFACE.md) | Architectural positioning: this kernel re-explains POSIX, it does not escape it. Comparative position against Linux/BSD, microkernel, Plan 9, cap-OS. The five primitive cells. The architectural-extension protocol. |
| 01 | [`01_CONCEPTS_v5.md`](01_CONCEPTS_v5.md) | The unified vocabulary. Seven-layer architecture. Five primitive cells (SubjectContext, Signifier Resolution, StepOp/StepOutcome, YieldShape, Publication). Upper/lower script split. ExecutionScope vs YieldShape cleavage. Closed-catalog list. Supersedes `00_meta-framework/CONCEPTS_v4.md`. |
| 02 | [`02_INVARIANTS_v5.md`](02_INVARIANTS_v5.md) | Canonical invariant catalog. New families: SUBJ-*, YIELD-*, DELEGATE-*, SCOPE-*. Updated STEP-* over four-variant outcome. Carries forward all v4 families unchanged unless explicitly modified. Supersedes `00_meta-framework/INVARIANTS_v4.md`. |
| 03 | [`03_STEP_MODEL_v2.md`](03_STEP_MODEL_v2.md) | The step algebra. Four-variant `StepOutcome<T,P>`. Closed `YieldShape` catalog. Typed `StepOp` trait. `StepProgress` monoid. `DriverMode` closed catalog with `classify`. Five-stage in-step discipline preserved. Anti-pattern catalog A-1 through A-15. Supersedes `02_execution/STEP_MODEL_v1.md`. |
| 04 | [`04_SYSCALL_SHAPE_v1.md`](04_SYSCALL_SHAPE_v1.md) | The upper/lower split discipline. SubjectContext threading. Five worked examples (native sys_read, OnBehalfOf drive_sqe_read, restricted sys_open, subject-mutating sys_execve, FUSE-delegated read). |
| 05 | [`05_DELEGATE_v1.md`](05_DELEGATE_v1.md) | The `OnAgent` yield shape. Cap-owned `DelegateEndpoint`. Token-as-cap. Typed request/reply with fd_injections and continuation. Cancellation policies. Coverage: userfaultfd, FUSE, fanotify-perm, ptrace. |
| 06 | [`06_EXECUTION_SCOPE_v1.md`](06_EXECUTION_SCOPE_v1.md) | The `OnBehalfOf<P>` execution scope. Borrow primitive. Abandonment via Killable wait. Resource scoping. Coverage: io_uring SQPOLL, AIO, FUSE helper, network softirq. |
| 07 | [`07_BLAST_RADIUS.md`](07_BLAST_RADIUS.md) | Migration cost: code volume, surface counts, per-change blast radius, recommended landing order, risk register. |
| 08 | [`08_SYSV_IPC_v1.md`](08_SYSV_IPC_v1.md) | SysV + POSIX IPC subsystem family (sem / shm / msg) as a worked composition of v3 primitives. Canary doc: a real 30-syscall Linux family lands with zero closed-catalog growth. |
| 08O | [`08_OBSERVATION_L0_L6_REFACTOR_v0.md`](08_OBSERVATION_L0_L6_REFACTOR_v0.md) | Observation subsystem tightening plan: L0-L6 topology, borrowed organization by layer, boundary rules, enforcement, and staged migration. |
| 10 | [`10_SCHED_SMP_v1.md`](10_SCHED_SMP_v1.md) | Cross-hart scheduler behavior under SMP: `current_hart`, cross-hart wake protocol, work stealing with lock-and-recheck, IPI rescheduling, lifecycle state machine, boot bringup. The cpuset prerequisite. Supersedes `SCHEDULER_v0 §5.3–5.5`. |

## 2. Reading orders

<!-- txdoc:TXV3-INDEX-READING-1 -->

**For a new contributor.**
00 → 01 → skim 02 → 03 → 04 → pick a feature doc (05 or 06) → optionally 08 for a worked subsystem.

**For a feature designer proposing a new YieldShape or ExecutionScope.**
00 §5 → 01 §closed-catalogs → 05 (worked example of new YieldShape) → 06 (worked example of new ExecutionScope) → write your ADR following the same shape.

**For a subsystem author wanting a composition reference.**
03 → 04 → 08 (SysV IPC: a real Linux family composing from existing primitives without catalog growth).

**For a reviewer evaluating an in-flight subsystem.**
02 (canonical) → 03 §10 (anti-patterns) → 04 (upper/lower discipline) → existing v4 subsystem doc.

**For migration planning.**
07 → identify which v4 docs are touched → pick a landing order from §5.

**For a production-fitness audit.**
00 §4 (what this position costs) → 02 (invariants) → 07 §6 (risks) → look at the open-questions sections in 05 §11, 06 §11.

## 3. Relationship to v4 docs

<!-- txdoc:TXV3-INDEX-V4-RELATIONSHIP-1 -->

The v3 docs in this folder supersede the parts of v4 they touch:

| v4 doc | v3 status |
|---|---|
| `00_meta-framework/CONCEPTS_v4.md` | superseded by `01_CONCEPTS_v5.md` |
| `00_meta-framework/INVARIANTS_v4.md` | superseded by `02_INVARIANTS_v5.md` |
| `02_execution/STEP_MODEL_v1.md` | superseded by `03_STEP_MODEL_v2.md` |
| everything else in `00_meta-framework/`, `01_substrate/`, `02_execution/`, `03_memory-vm/`, `04_process-signals/`, `05_filesystem/`, `06_devices/` | still canonical; cross-references update on next routine edit |

The migration is staged: v3 docs are the new spine; v4 subsystem docs continue to apply, with the substitution that `Wait-adapt` reads as `Yield-adapt`, that `StepOutcome` has four variants, and that script entry establishes a `SubjectContext`. No subsystem code is yet broken by these renames; the migration is mechanical (see `07_BLAST_RADIUS`).

## 4. The single-paragraph summary

<!-- txdoc:TXV3-INDEX-SUMMARY-1 -->

txKernel v3 is the form the framework takes after closing the catalog over the Linux ABI surface. The architectural commitments unchanged from v2/v4 are: cooperative kernel with stackless coroutines; typed cap+witness slot discipline; refcnt+EBR reclamation on dedicated zones. v3 adds the structural vocabulary that lets these commitments cover the parts of Linux they previously could not: the four-variant `StepOutcome` factored against a closed `YieldShape` catalog (with `OnAgent` as the universal kernel→userspace-agent primitive), the `SubjectContext` and upper/lower script split (the syscall-side reflection of identity/payload bifurcation), the `OnBehalfOf<P>` execution scope (orthogonal to YieldShape), and the `SubjectAuthority::restrictions` home for seccomp/Landlock/LSM. With these, every Linux feature previously named as a "structural break" — userfaultfd, FUSE, fanotify-perm, ptrace, io_uring SQPOLL, AIO workers, seccomp filters, EPOLLET (deferred), futex PI (deferred) — has a place in the framework, governed by closed-catalog membership and earning architecture review under ARCH-3 to extend. The framework's evolution mechanism is closed-catalog extension; the implementation cost is enumerable and bounded.

## 5. Open work tracked outside this folder

<!-- txdoc:TXV3-INDEX-OPEN-1 -->

These items are referenced by v3 but not specified here:

- **`OnEdge` YieldShape (deferred).** For EPOLLET, edge-triggered inotify, etc. Substrate cost: per-subscription edge-state slots, overflow marking. ARCH-3 review when the first user lands.
- **`OnHandoff` YieldShape + `Owned<T>` reference strength (deferred).** For futex PI, rt-mutex. Substrate cost: priority-donation lattice, owner-CAS, wait-by-priority queue. ARCH-3 review when the first user lands.
- **`OnBehalfOf::KernelIdentity` and `OnBehalfOf::Cgroup` (deferred).** Closed-catalog members for kernel-actor and per-cgroup borrows.
- **Mid-scope authority replacement for OnBehalfOf (deferred).** v1 holds a snapshot; if `IORING_REGISTER_PERSONALITY` semantics are needed, this is the next extension.

Each is named in the relevant doc with a deferral marker; landing one is an ADR.
