# Migration: Blast Radius and Landing Order — v3 (full-retire plan)

<!-- txdoc:TXV3-BLAST-RADIUS-V3 -->

**Status.** v3 (Txv3 refresh, 2026-05; supersedes v2 with measured-not-estimated numbers and a full-retire landing plan).
**Purpose.** Quantify the actual cost of landing the v3 architectural changes against the merged tree and recommend a landing order that retires the v4 vocabulary completely — no compat shim, no parallel-shape transition.
**Audience.** Implementation lead; reviewers of the migration ADR.
**Method.** Four parallel agent surveys against merged mainline as of 2026-05. Grep-based call-site counts with context classification, not estimates.
**Diff vs v2.** v2's numbers underestimated the surface by ~40% (the post-merge code grew further into `step_v3/`). v3 corrects to measured totals, adds the `exit_port` rename axis that v2 omitted, recognises the half-built `step_v3/` scaffold as a real timeline-shortener, and abandons the compat-layer fallback in favour of a focused atomic refactor.

---

## 1. Code volume baseline

<!-- txdoc:BLAST-V3-BASELINE-1 -->

```
mainline (excluding target/, ext4 backend, HAL boards):
  tx-subsystems        33,979 lines  104 files
  tx-shims             15,266 lines   29 files
  tx-substrate         13,061 lines   51 files     (incl. step_v3/ scaffold)
  boards/tx-hal-rv64    8,546 lines   18 files
  tx-reactor            8,427 lines   31 files
  tx-kernel             6,570 lines   16 files
  tx-scripts            3,786 lines    8 files
  tx-fs                 2,794 lines    6 files
  tx-ext4-format        2,706 lines    5 files
  boards/tx-hal-m1dock  2,252 lines    3 files
  tx-hal                1,894 lines    4 files
  boards/tx-hal-la64    1,352 lines    1 files
  tx-ext4               1,318 lines    7 files
  tx-drivers              196 lines    1 files
  tx-policy                 9 lines    1 files     (skeleton)
  tx-services               7 lines    1 files     (skeleton)
```

Total kernel-material LoC: ~88,000. `tx-policy` and `tx-services` still skeleton; `SubjectAuthority` / restriction stack lands there. Everything else has real code.

---

## 2. The scaffold: `step_v3/` is partly built already

<!-- txdoc:BLAST-V3-SCAFFOLD-1 -->

The single largest correction vs v2: **`tx-substrate/src/step_v3/` already exists and is in active use.** This is the migration's load-bearing fact.

| Type | Status | Site count |
|---|---|---|
| `WaitProtocol` | defined; in use | 33 hits / 13 files |
| `StepOp` trait | defined; in use | 102 hits / 13 files |
| `SubjectContext` | defined `step_v3/subject_context.rs` | 28+ hits |
| `EndpointKind` | defined `step_v3/endpoint_kind.rs:22` | ~30 hits |
| `DelegateToken` | defined `step_v3/agent.rs:58` | 8 hits / 4 files |
| `DelegateEndpoint` | defined `step_v3/agent.rs:44` | 8 hits / 4 files |
| `DelegateRequest` | defined `step_v3/agent.rs:71` | 8 hits / 4 files |
| `CancelPolicy` (Agent-side) | defined `step_v3/agent.rs:20` | 9 hits / 3 files |
| `TimerToken` | defined `tx-reactor/src/timer.rs:31`, private | 5 hits |

The v3 work *populates* these scaffolds with the missing state machine, mailbox plumbing, AgentTokenGuard, and the renamed YieldShape — it does not start from green field.

What's net new (zero hits anywhere):

```
DelegateReply, DelegateState, AgentTokenGuard, AgentCancelPolicy,
TokenDropPolicy, EndpointScope, TimerWheel, TimerGuard, TaskMailbox,
WakeHint, WaitGeneration, WaitSource, WaitSourceId, WaitRegistrationGuard,
PreparedWaitRegistration, ActiveWait, ActiveYieldShape, YieldRegistration,
ResumeOutcome, Interruptibility, CancelReason, AbortReason, apply_resume,
ScriptCtx, SubjectAuthority
```

Existing wait/wake plumbing to retire: **43 `Waker` sites across 9 files**, concentrated in `tx-substrate/src/bus/`. `wait_queue`, `wake_up*`, `Notify*` are *not* in use — clean replacement field.

---

## 3. Measured surface

<!-- txdoc:BLAST-V3-SURFACE-1 -->

### 3.1 Vocabulary rename axis

| Name (→ rename) | Hits | Files | Heaviest |
|---|---|---|---|
| `OnCarrier` → `OnWaitSource` | **118** | **28** | `tty/execution/step_write.rs` 11, `vm/execution.rs` 11, `step_v3/mod.rs` 7 |
| `WakeCarrier` → `WaitSource` + `WaitSourceId` | **39** | **12** | `step_v3/mod.rs` 7, `vm/user_access.rs` 6 |
| `InterestConditions` → `InterestMask` | **38** | **12** | identical to `WakeCarrier` distribution |
| `read_wq` / `write_wq` → `read_source` / `write_source` | **24** | **5** | mostly docs; one site in `tests/bus.rs` |
| `exit_port` → `exit_source` | **164** | **24** | `process/structure.rs` 21, `process/tests/exit_port.rs` 19, plus docs |
| `carrier` identifier (case-by-case) | 71 | 11 | needs per-site judgment |
| **Subtotal: mechanical** | **~383** | | |
| **Subtotal: judgment** | **71** | | |

### 3.2 StepOutcome refactor axis

| | Measured |
|---|---|
| `step_*` free function signatures (StepOp wrap candidates) | **178** across **26** source files |
| `StepOutcome::Advanced/Blocked/AdvancedThenBlocked` construction sites | 423 |
| `StepOutcome::Done(...)` sites (unchanged, but reviewed) | 283 |
| `StepOutcome::Err(...)` sites (unchanged, but reviewed) | 215 |
| `Yield { shape }` match sites | 22 |
| Driver-loop match composition files | 4 |
| Test files touching `StepOutcome` | 16 |
| Heaviest test files | `tty/tests/legacy_phase_a.rs` 91 refs; `tmpfs/tests.rs` 66 refs |

### 3.3 CancelPolicy axis

**9 sites, 3 files, 100% Agent-side**. All in `tx-substrate`:

```
crates/tx-substrate/src/step_v3/agent.rs:8,20         (doc + enum def)
crates/tx-substrate/src/step_v3/mod.rs:132,263        (field + re-export)
crates/tx-substrate/tests/v3_yield_on_agent.rs:×5    (constructors + tests)
```

The rename is `CancelPolicy` → `AgentCancelPolicy`. `TokenDropPolicy` is net-new (lands with `AgentTokenGuard`). **No compat shim needed** because (a) zero `TokenDropPolicy` sites exist to need bridging, and (b) the existing 9 sites all want `Agent-` variant naming anyway.

### 3.4 Progress-type distribution

| Type | Use sites across 178 step_* fns |
|---|---|
| `NoProgress` | 453 (one-shots: open, mkdir, fork, dup, close, …) |
| `ByteProgress` | 139 (read/write/splice/sendfile/pipe/tty I/O) |
| `PageProgress` | 65 (page_backed lifecycle / vm fault) |
| `IoVecProgress` | 34 (scatter-gather) |
| `EntryProgress` | 27 (getdents / vfs walker) |

---

## 4. Per-change blast radius (measured)

<!-- txdoc:BLAST-V3-PER-CHANGE-1 -->

| # | Change | Code surface | Doc surface | New code | Risk |
|---|---|---|---|---|---|
| **A** | Vocabulary rename (`OnCarrier`/`WakeCarrier`/`InterestConditions`/`exit_port`/wq-fields) | **383 mechanical + 71 judgment = 454 sites across ~50 files** | Concept-doc rename PR (separate; already drafted in Txv3/) | type aliases initially zero; identifier rename only | **Low** — entirely mechanical; sed-with-review; no semantic change |
| **B** | `StepOutcome` 5→4 + `YieldShape::OnWaitSource`/`OnAgent`/`OnTimer` | **423 construction sites + 22 Yield matches + driver loops in 4 files** | `STEP_MODEL_v1` superseded | ~50 LoC new variants | **Medium** — exhaustive match catches misses; test re-green real |
| **C** | `StepOp` trait wrap | **178 free step_* fns → 178 `impl StepOp for FooOp` shells across 26 files** | scaffold already exists in `step_v3/` | ~500 LoC mechanical wrap + 5 progress types ~200 LoC | **Medium** — per-subsystem parallelizable; trait already defined |
| **D** | `apply_resume` + `ResumeOutcome` | OnAgent-yielding steps need override (~5–8 files) | trait change in `03_STEP_MODEL_v2` | ~100 LoC trait method + default | **Low** — additive; default impl handles the common case |
| **E** | `CancelPolicy` → `AgentCancelPolicy` + new `TokenDropPolicy` | **9 mechanical + N net-new** | `02_INVARIANTS_v5`, `05_DELEGATE_v1` already updated | ~30 LoC enum + derive | **Low** — trivially small |
| **F** | Wake-substrate (`TaskMailbox`/`WakeHint`/`WaitGeneration`/`WaitSource`) | replaces **43 `Waker` sites across 9 files in `tx-substrate/src/bus/`** | runtime spec already specifies | ~1,500 LoC framework (token zone, endpoint zone, mailbox, source index) | **Medium-High** — new substrate primitive; the most architecturally consequential PR |
| **G** | `OnAgent` delegate runtime (token state machine, install_request, EndpointScope) | populate existing `step_v3/agent.rs` scaffold | runtime spec specifies | ~500 LoC adding DelegateState, install_request, AgentTokenGuard, mark_* methods | **Medium** — builds on §2's scaffold |
| **H** | `OnTimer` + protocol-deadline TimerGuard | uses `TimerToken` (already exists, private) | `03_STEP_MODEL_v2` + `06_EXECUTION_SCOPE_v1` updated | ~300 LoC TimerWheel public surface + TimerGuard | **Low** — small additive |
| **I** | `SubjectContext` + upper/lower split | scaffold exists (`step_v3/subject_context.rs`); needs threading | `04_SYSCALL_SHAPE_v1` specifies | ~300 LoC threading through ~10 canonical syscalls | **Low** — additive |
| **J** | `OnBehalfOf<P>` execution scope | net new | `06_EXECUTION_SCOPE_v1` specifies | ~500 LoC framework | **Medium** — defers cleanly until first user (AIO/SQPOLL) |
| **K** | `SubjectAuthority.restrictions` cell | lands in `tx-policy` skeleton | deferred to later RESTRICTION doc | ~150 LoC framework cell | **Low** — additive |

Total framework LoC for A through I: **~3,500 LoC of net-new code + ~600 sites of rename + 423 sites of variant refactor + 178 step-fn wraps**. J and K defer until their first users materialize.

---

## 5. Full-retire landing plan

<!-- txdoc:BLAST-V3-LANDING-1 -->

**No compat layer. No parallel-shape transition. Each PR retires the v4 vocabulary it touches.** The compat-layer fallback from v2 is dropped because (a) the rename is mechanical and the per-PR review surface is bounded; (b) `step_v3/` already provides the scaffold to migrate into; (c) carrying two shapes simultaneously bloats the cognitive surface for reviewers and ships kernel-week-of-debt.

The plan is **eleven sequential PRs**, two parallelizable PR series, and a freeze window for the foundation.

### 5.1 Freeze window

Days 1–10 (two weeks): foundation PRs (1, 2, 3) land sequentially on mainline. **Hold non-foundation subsystem PRs for the freeze duration.** Communicate the freeze in the implementation ADR.

After day 10: subsystem-by-subsystem `StepOp` wrap PRs (PR-4 series) run in parallel; other feature work resumes.

### 5.2 PR sequence

| PR | Lands | Days | Touches |
|---|---|---|---|
| **PR-1** | Vocabulary rename (A): `OnCarrier` → `OnWaitSource`, `WakeCarrier` → `WaitSource`/`WaitSourceId`, `InterestConditions` → `InterestMask`, `read_wq`/`write_wq` → `read_source`/`write_source`, `exit_port` → `exit_source`, audit of `carrier` identifier sites. **No type aliases; no shim.** Direct rename. | 2 | ~50 files |
| **PR-2** | StepOutcome 5→4 + YieldShape catalog (B) + OnTimer admission (H/partial): `StepOutcome::{Continue, Yield, Done, Err}`; `YieldShape::{OnWaitSource, OnAgent, OnTimer}`; rewrite 423 construction sites; re-green 16 test files including `legacy_phase_a.rs` (91 refs) and `tmpfs/tests.rs` (66 refs). | 4 | ~75 files |
| **PR-3** | Wake substrate (F): introduce `TaskMailbox`, `WakeHint`, `WaitGeneration`, `WaitSource`, `PreparedWaitRegistration`, `WaitRegistrationGuard`. **Retire the 43 `Waker` sites in `tx-substrate/src/bus/`.** New zone for tokens; new endpoint zone. | 4 | ~15 files (bus + new zones) |
| **PR-4 series** | StepOp wrap (C): one PR per subsystem (8 subsystems × ~22 fns each). Parallelizable across reviewers after the foundation freeze. | 3 (total) | 26 files split across 8 PRs |
| **PR-5** | apply_resume + ResumeOutcome (D): trait method, default impl, OnAgent-yielding step overrides (~5-8 files) | 1 | ~8 files |
| **PR-6** | CancelPolicy rename + TokenDropPolicy introduction (E): rename 9 sites; add new enum + derivation helper | 0.5 | 3 files |
| **PR-7** | OnAgent delegate runtime (G): `DelegateState` state machine, `install_request`, `AgentTokenGuard`, `mark_agent_died`/`mark_timed_out`, `EndpointScope`-driven abandonment | 3 | populate `step_v3/agent.rs` + new files |
| **PR-8** | Protocol-deadline + TimerGuard (H/full): `TimerWheel` public surface, `TimerGuard` with `PrimarySleep`/`DeadlineAbort`/`DelegateTimeout` roles | 1 | timer.rs + scripts/drive.rs |
| **PR-9** | SubjectContext threading (I): canonical syscalls (sys_open, sys_read, sys_write, sys_fork, sys_execve, sys_close, sys_pipe) threading `&SubjectContext` | 2 | shims/ + scripts/ |
| **PR-10** | First agent kind: userfaultfd | 5–10 | new ufd subsystem |
| **PR-11** | First OnBehalfOf user: AIO worker (lands J framework + AIO subsystem together) — **LANDED** (`docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md`; `crates/tx-subsystems/src/aio.rs`; `crates/tx-shims/src/linux_syscall/aio.rs`; `crates/tx-shims/tests/v3_aio_e2e.rs`) | 5 | tx-scripts + new aio subsystem |

Total foundation (PR-1 through PR-3): **~10 working days, ~140 files touched, ~600 mechanical edits**.
Total full v3 vocabulary retired (PR-1 through PR-8): **~22 working days, ~250 files touched**.

K (`SubjectAuthority.restrictions`) defers until seccomp/landlock work begins.

### 5.3 Heaviest individual files

These need explicit owner-attention during their PR:

| File | Sites | PR | Notes |
|---|---|---|---|
| `tx-subsystems/src/tty/tests/legacy_phase_a.rs` | 91 StepOutcome refs | PR-2 | Largest test re-green |
| `tx-subsystems/src/process/structure.rs` | 21 exit_port + carrier | PR-1 | Single file dominates process rename |
| `tx-fs/src/tmpfs/tests.rs` | 66 StepOutcome refs | PR-2 | Second-largest test re-green |
| `tx-substrate/src/step_v3/mod.rs` | 7+7+7 of three renamed types | PR-1 | Scaffold's central re-export module |
| `tx-subsystems/src/tty/execution/step_write.rs` | 11 OnCarrier + step_* signatures | PR-1, PR-4 | Hot in both axes |
| `tx-subsystems/src/vm/execution.rs` | 11 OnCarrier + step_* signatures | PR-1, PR-4 | Hot in both axes |
| `tx-substrate/src/bus/` (9 files, 43 Waker sites) | Waker → Mailbox | PR-3 | Concentrated retire surface |

---

## 6. Risk register

<!-- txdoc:BLAST-V3-RISKS-1 -->

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Foundation freeze runs over 10 days | Medium | Medium | Strict PR-1/2/3 review SLA; second reviewer pre-assigned per PR |
| PR-2 test re-green takes >2 days for legacy_phase_a.rs | Medium | Low | Pre-write the test mechanical-rewrite script before merging PR-2's source changes |
| `carrier` identifier (71 sites) judgment calls miss a domain "carrier" | Low | Low | PR-1 ships with explicit list of each rename decision in the commit message; reviewers veto-able |
| PR-3 (`TaskMailbox` substrate) regresses bus performance | Low | High | Benchmark `tx-substrate/src/bus/` before/after; performance gate in CI |
| `StepOp` wrap regresses inlining hot paths | Low | Low | `#[inline]` on small impls; benchmark `page_backed.rs` and `tmpfs.rs` |
| Per-subsystem PR-4 series conflicts | Low | Low | Merge in dependency order (substrate first, then upper subsystems) |
| `OnAgent` deadline race in PR-7 | Low | Medium | DTOK-3 invariant + dedicated reply-vs-timeout test before merge |
| `EndpointScope` abandonment routing edge cases | Medium | Medium | Land with explicit test exercising tracee-process exit during ptrace stop |
| `step_v3/` scaffold has divergent intent from spec | Medium | High | **Before PR-1**, walk through `step_v3/agent.rs` line-by-line vs spec; if divergence is real, reconcile in a pre-PR-1 ADR |
| Reviewer fatigue across long PR series | Medium | Low | Acknowledge: ~22 working days is real; rotate primary reviewer per PR family |

The **highest-impact item** is the `step_v3/` scaffold reconciliation. If the scaffold already encodes design decisions that conflict with the v3 spec — particularly around `WaitProtocol`, `EndpointKind`, or `DelegateRequest` shape — those conflicts must be surfaced before PR-1, not discovered during it.

---

## 7. Success criteria

<!-- txdoc:BLAST-V3-SUCCESS-1 -->

The migration is "done" when:

- v4 vocabulary is **fully retired** from mainline: no `OnCarrier`, `WakeCarrier`, `InterestConditions`, `exit_port`, `read_wq`/`write_wq` identifiers remain in code (excluding archived docs).
- `StepOutcome` is four-variant; all 423 construction sites use the new shape.
- All 178 `step_*` functions are wrapped as `impl StepOp`.
- `tx-substrate/src/bus/` no longer uses bare `Waker`; `TaskMailbox` is the wake-routing primitive.
- `step_v3/` scaffold is populated: `DelegateState`, `install_request`, `AgentTokenGuard`, `mark_*` methods all exist and pass the runtime-spec tests.
- userfaultfd works end-to-end (PR-10 success).
- AIO worker works end-to-end (PR-11 success).
- v3 docs in `Txv3/` are referenced by the v4 docs they supersede; v4 docs carry `[deprecated by v5]` notes where applicable.
- A migration ADR records the landing in `docs/progress/decisions/`.

The success bar is **vocabulary retirement plus two canary use cases**. Subsystems that don't yet exist (FUSE, fanotify-perm, ptrace, full seccomp) ship on their own schedule against the stable v3 framework.

---

## 8. What this plan does *not* do

<!-- txdoc:BLAST-V3-NEGATIVE-1 -->

To avoid scope creep:

- **No compat layer.** Type aliases / constructor shims are explicitly rejected. The transition is direct.
- **No parallel-shape period.** v4 and v5 vocabulary do not coexist in mainline after PR-1.
- **No per-subsystem opt-in.** The vocabulary rename and `StepOutcome` refactor are tree-wide in their respective PRs.
- **No deferred test re-green.** Tests update in the same PR as their source.
- **No `OnEdge` / `OnHandoff` in this landing.** Deferred to a later phase when their first users (EPOLLET, PI futex) materialize.
- **No restriction-kind implementations.** The `SubjectAuthority.restrictions` cell is reserved for the seccomp/landlock work, which is its own multi-week effort.

These exclusions exist to keep the landing window bounded and the per-PR review surface tractable.
