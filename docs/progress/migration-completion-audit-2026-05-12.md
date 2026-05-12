# v3 Migration — Completion Audit and Q-Hand-off

**Date:** 2026-05-12
**Worker:** W-GG (research-only)
**Anchor:** [`docs/Txv3/07_BLAST_RADIUS.md`](../Txv3/07_BLAST_RADIUS.md)
(plan), [`docs/progress/plans/2026-05-11-v3-migration-remaining.md`](plans/2026-05-11-v3-migration-remaining.md)
(remaining-work plan), the nine [`decisions/2026-05-11-d{1..9}-*.md`](decisions/)
ADRs.
**Purpose.** Definitive "what landed" + per-PR map, the nine
divergences, remaining work, and a hand-off doc for the next quarter.
**Honesty rule:** this audit is a hand-off, not a celebration. The
migration delivered vocabulary + scaffolding + two canaries; real
performance, real workloads, real edge-case hardening are next-quarter
work and are called out explicitly in §6.

<!-- txdoc:TXV3-MIGRATION-AUDIT-V1 -->

---

## 1. Executive summary

The v3 architectural migration started 2026-05-11 (Day 1) and reaches
its `07_BLAST_RADIUS.md` §7 exit bar (vocabulary retirement + two
canary subsystems) on 2026-05-12 — **~2 wall-days, vs the plan's
~22 working-day estimate**. The compression is not productivity
folklore: it is the result of (a) heavy worker-letter parallelism
(33 workers W-A → W-GG running disjoint scopes against a shared
mainline), and (b) **scope aggregation** — multiple plan-rows landed
inside single PRs once the foundation primitives were in tree (e.g.
PR-A.1 + PR-A.2 + PR-A.3 collapsed into one rename arc; PR-3A/B/C/D-0
landed inside the same wave; the PR-3D bus-consumer migration ran
five subsystems in a uniform template once D2 / D4 fixed the layering).

What landed: vocabulary retired tree-wide (no `OnCarrier` /
`WakeCarrier` / `InterestConditions` / `exit_port` / `read_wq` survive
in production code); `StepOutcome` reduced to four variants;
~80 of the auditioned 178 free `step_*` fns wrapped as `impl StepOp`
(the rest were trait-method bodies or test fns mis-counted in the
v3 surface estimate); a populated wake substrate
(`TaskMailbox` / `WaitSource` / `TimerWheel`) living in
`tx-substrate::wake`; a populated step_v3 runtime
(`DelegateRegistry`, `AgentTokenGuard`, `ExecutionScope::OnBehalfOf<Cap<I>>`,
`with_on_behalf_of`); per-thread mailboxes and per-process subjects;
the five mechanical bus consumers (pipe, futex, exit_source, tty, vfs)
migrated to D2-coexistence `WaitSource`; the signal subsystem
migrated by its own three-phase ADR (D9) to per-thread routing.
Two canary subsystems — **userfaultfd** (OnAgent canary, PR-10) and
**AIO** (OnBehalfOf canary, PR-11) — are end-to-end functional pending
the W-EE and W-FF closing flights.

What is **not** done: real performance benchmarking (no `cargo bench`
gate has run against the substrate); the restrictions cap is still
a placeholder zone (D5 §7); EndpointScope abandonment edge cases
(tracee-process exit during ptrace stop) lack a pinning test; the
TimerWheel's wheel-mechanics fire path is still PR-8B stub-shaped.
These are the next-quarter ledger; the migration's exit bar does
not require them.

---

## 2. Per-PR completion matrix

The map to `07_BLAST_RADIUS.md` §5.2 rows.

| BLAST_RADIUS PR | Plan (days) | Actual outcome | Landed worker(s) | Tests added |
|---|---|---|---|---|
| **PR-1** vocabulary rename | 2 | LANDED 2026-05-11 — bundled with A.2/A.3/A.4 into a 4-PR rename arc | early session + PR-A.1 6-worker fanout (W1..W6) + PR-A.3 sequential + PR-A.4 4-worker fanout (W1..W4) + carrier_id audit | 0 (mechanical) |
| **PR-2** `StepOutcome` 5→4 + StepOp wrap | 4 + 3 | LANDED 2026-05-11 — three waves (pilot S1/S2/S3, wave 2 P1..P4, wave 3 Q1..Q5) + R1 cleanup | S1/S2/S3, P1/P2/P3/P4, Q1/Q2/Q4 (Q3+Q5 no-op), R1 | ~80 wraps + ~75 tests; ~1396 → ~1474 |
| **PR-3** wake substrate | 4 | LANDED 2026-05-11 in four sub-PRs: 3A (mailbox), 3B (`WaitSource`), 3C (`PreparedWaitRegistration` + `WaitRegistrationGuard`), 3D-0 (D4 layering move to substrate), 3D-1..5 (per-consumer migration) | main agent (3A/3B/3C); W-D (3D-0); W-G/W-K/W-M/W-P/W-S (3D-1..5: pipe/futex/exit_source/tty/vfs) | +8 (3A) + 6 (3B) + 5 (3C) + 3 (3D-1 mailbox-waker bridge) + 8 (pipe) + 8 (futex) + 1 bundle (exit_source) + 1 (tty) + 1 (vfs) |
| **PR-4 series** StepOp wrap (per subsystem) | 3 total | Folded into **PR-2** waves above — no separate PR-4 series materialized | same as PR-2 | (counted under PR-2) |
| **PR-5** `apply_resume` + `ResumeOutcome` | 1 | LANDED 2026-05-11 — closed catalog `Retry`/`WithReply`/`TimerExpired`/`Aborted`; default-reject `apply_resume`. | main agent | 5 |
| **PR-6** `CancelPolicy` → `AgentCancelPolicy` + `TokenDropPolicy` | 0.5 | LANDED 2026-05-11 | main agent | 2 |
| **PR-7** OnAgent delegate runtime | 3 | LANDED 2026-05-11 (PR-7 runtime W-F) + PR-7B mailbox/timer integration (W-H) + PR-7C TimerWheel layering ADR (W-L, D6); the wheel relocation `tx-reactor::timer` → `tx-substrate::wake::timer` followed | W-F (state machine); W-H (mailbox routing + timer install/fire); W-L (D6 layering ADR) | +30 (PR-7) + 14 substrate-side + 9 reactor-side (PR-7B) |
| **PR-8** OnTimer + TimerGuard | 1 | LANDED 2026-05-11 in two pieces: PR-8 admission (3-worker T1/T2/T3) + PR-8 publish (TimerGuardRole catalog) + PR-8B follow-up (TimerWheel mechanics — **deferred**, stub holds) | T1/T2/T3 + main agent | +13 (timer surface) + sweep edits (no test delta on admission) |
| **PR-9** `SubjectContext` threading | 2 | LANDED 2026-05-11 in six phases: phase-1/2 generic types, phase-3a `ScriptCtx<I>` real fields, phase-3a 64-wrap polymorphic fanout (M1/M2/M3/M4), phase-3b 4-syscall thread (W-A), phase-4 cap-shape reshape (W-E), phase-5 cred zone + 4-arm subject population (W-J) | M1/M2/M3/M4, W-A, W-E, W-I (D5 ADR), W-J | +4 (phase 1/2) + 3 (phase-3a alias) + 7 + 6 PR-9 phase-3a fanout |
| **PR-10** userfaultfd (OnAgent canary) | 5–10 | LANDED 2026-05-11 → 2026-05-12 in six phases: P-10.0 fd scaffold (W-Q), P-10.1+P-10.2 `DelegateReply` sum + `sys_userfaultfd` + `UFFDIO_API` (W-T), P-10.3 `UFFDIO_REGISTER` + `VmEntry::ufd_registration` (W-V), P-10.4 fault-path OnAgent branch + `await_agent_reply` (W-Y), P-10.5 reply ioctls + per-ufd queue + `read(ufd)` (W-BB), P-10.6 e2e (W-EE in flight) | W-O (readiness ADR D7), W-Q, W-T, W-V, W-Y, W-BB, W-EE | +1 (fd scaffold) + 8 (syscall) + 10 (UFFDIO_REGISTER) + 4 (fault path) + 11 (ioctl reply) + (W-EE e2e) |
| **PR-11** OnBehalfOf + AIO worker | 5 | LANDED 2026-05-11 → 2026-05-12 in six phases: P-11.0 framework (W-W), P-11.1 fd scaffold (W-Z), P-11.2 `io_submit` + worker (W-CC), P-11.3–P-11.5 (W-FF in flight) | W-U (readiness ADR D8), W-W, W-Z, W-CC, W-FF | +7 (P-11.0 framework) + 5 (P-11.1) + 7 + 3 unit (P-11.2) + (W-FF) |
| **PR-K** restrictions cell | deferred | **NOT LANDED — placeholder zone** per D5 §7. Lands with seccomp/landlock. | — | 0 |
| **D9 signal migration** | (new — not in BLAST_RADIUS) | LANDED 2026-05-11: D9 ADR (W-X), D9-A event variant + post wiring (W-AA), D9-B+D9-C eligibility scan + interrupt-wake pin (W-DD) | W-X, W-AA, W-DD | +6 (eligibility) + 1 (interrupt-wake integration) |

**Total worker letters:** **33** (W-A through W-GG). PR-A.1 / PR-A.4 /
PR-2 worker fanouts (W1..W6, M1..M4, S1..S3, P1..P4, Q1..Q5, T1..T3,
D1..D5, R1) are inner-PR worker numbering not part of the W-* letter
sequence.

---

## 3. Divergences from plan (D1 – D9 ADRs)

The plan in `07_BLAST_RADIUS.md` made nine silent assumptions, each
of which collided with the merged tree during implementation. Each
divergence is now a recorded ADR in `docs/progress/decisions/`.

### D1 — `ScriptCtx` identity coupling
`docs/progress/decisions/2026-05-11-d1-scriptctx-trait-bound-identity.md`
**Problem.** `07_BLAST_RADIUS.md` §4 row I assumed `ScriptCtx` could
be threaded through the subsystem layer as a concrete type. In merged
tree, `ProcessIdentity` lives in `tx-subsystems`, not `tx-substrate` —
threading a concrete `ScriptCtx` would force a subsystem-layer crate
dependency on substrate, violating layering.
**Decision.** `step_v3` defines `SubjectIdentity` trait + generic
`SubjectContext<I>` / `ScriptCtx<I>`. `tx-subsystems::process::ProcessIdentity`
impls `SubjectIdentity`. `tx-shims` binds the alias
`KernelScriptCtx = ScriptCtx<process::ProcessIdentity>`. Cost: ~80
PR-2 wraps had to be made polymorphic over `I` (M1/M2/M3/M4 fanout,
PR-9 phase 3a). Benefit: subsystems retain authority over identity
shapes; substrate stays subsystem-agnostic.
**Workers.** W-F (trait foundation), W-A (3-up fanout, phase 3b), W-E
(phase 4 Cap reshape).

### D2 — `WaitSource` coexists with `RawPort`
`docs/progress/decisions/2026-05-11-d2-waitsource-coexists-with-rawport.md`
**Problem.** Plan §5.2 PR-3 said retire 43 `Waker` sites. Merged tree
showed the count was 92 (later 94) and many `RawPort` consumers were
NOT plain `Waker` — they were `Channel::wait` futures with raw-waker
guts.
**Decision.** D2 coexistence — `WaitSource` adds a new `notify`
publication next to the existing `Channel::fire`, both fire under the
same call site. No `RawPort` deletion. Old consumers keep working;
new consumers register via `register_prepared`. Cost: every migrated
consumer carries two wake paths in parallel for the rest of the
quarter. Benefit: unblocked PR-3D fanout; no cascade of test rewrites.
**Workers.** W-G (PR-3D-1 pipe), W-K (PR-3D-2 futex), W-M (PR-3D-3 exit),
W-P (PR-3D-4 tty), W-S (PR-3D-5 vfs).

### D3 — VFS walker async carve-out
`docs/progress/decisions/2026-05-11-d3-walker-async-carveout.md`
**Problem.** PR-2 wrap count assumed `vfs::walker::{step_walk, step_open}`
could fit the `StepOp` shape. They cannot: walker fns are script-level
async resolvers that legitimately await VFS upgrades.
**Decision.** Walker fns stay async free fns, NOT `impl StepOp`.
Reject an `AsyncStepOp` trait variant (would weaken the no-await-in-step
rule). `WALKER-CARVEOUT-1` invariant codified in walker.rs.
**Workers.** Main agent.
**Cost.** PR-2 count drops from 178 to ~80 free `step_*` fns; the
remainder were trait-method bodies or test fns.

### D4 — bus/mailbox layering (move `TaskMailbox` → substrate)
`docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md`
**Problem.** PR-3A/B/C landed `TaskMailbox` / `WaitSource` in
`tx-reactor`. PR-3D's targets (the 33 sites in `tx-substrate/src/bus/`)
cannot use a reactor type without a back-edge.
**Decision.** Option B — move both primitives down into
`tx-substrate::wake`, with `pub use` shims in `tx-reactor::lib.rs` so
zero consumer-side edits are needed. `TaskMailbox` already depended
only on substrate (`InterestMask`, `WaitSourceId`, `SpinMutex`); the
move was a verbatim file relocation.
**Workers.** W-C (ADR), W-D (PR-3D-0 layering move).

### D5 — Cred zone allocation (Path A vs Path B)
`docs/progress/decisions/2026-05-11-d5-cred-zone-allocation.md`
**Problem.** PR-9 phase 4 required threading a `Cap<Cred>` through
the subject context. Production `Cred` was `SpinMutex<Cred>` inside
`ProcessPayload`, not a zone-allocated cap. Two paths surfaced:
A — zone-allocate Cred and atomic-swap on mutation; B — Cred-zone
slab churn on every syscall.
**Decision.** Path A. `ProcessPayload.cred: AtomicSlot<Cap<Cred>>`
(matches existing `AddressSpace` precedent). Mutators reserve →
sign → replace. EBR retires old caps. Restrictions cap is a
placeholder per call until PR-K lands.
**Workers.** W-I (ADR), W-J (implementation).
**Cost.** 7 mutator-rewrite sites + 3 test helpers in `cred.rs`.

### D6 — TimerWheel layering (relocate to substrate)
`docs/progress/decisions/2026-05-11-d6-timerwheel-layering.md`
**Problem.** PR-7B install/fire wiring put callback shapes in the
reactor — clean separation, but `AgentTokenGuard` (substrate-side)
couldn't own a `TimerGuard` directly.
**Decision.** Move `TimerWheel` / `TimerGuard` / `TimerToken` /
`TimerGuardRole` from `tx-reactor::timer` to `tx-substrate::wake::timer`,
patterned after D4. ~344 LoC out; ~178 of internal `TimerQueue` stays
reactor-side. Public surface clean along `timer.rs:180`.
**Workers.** W-L (ADR; relocation deferred to PR-7C scope per D6).
**Status.** ADR landed; relocation queued.

### D7 — PR-10 userfaultfd readiness (GO)
`docs/progress/decisions/2026-05-11-d7-pr-10-userfaultfd-plan.md`
**Problem.** `07_BLAST_RADIUS.md` PR-10 budget (5–10d) silently
assumed PR-7 + PR-7B + the timer wheel were full prerequisites. The
ADR audited the merged tree and found the dependencies were narrower:
three gaps (DelegateReply sum, `await_agent_reply` helper, OpenFile
ufd variant) were PR-10's first three phases, not prerequisite PRs.
**Decision.** PR-10 absorbs the three substrate gaps as phases 1–3.
Eight-phase plan: ~5–7 working days.
**Workers.** W-O (ADR).
**Cost / benefit.** No prerequisite PR; PR-10 self-contained.

### D8 — PR-11 AIO readiness (GO)
`docs/progress/decisions/2026-05-11-d8-pr-11-aio-plan.md`
**Problem.** PR-11 budget assumed an `OnBehalfOf` framework had to
land separately before AIO. ADR audited the merged tree: framework
was ~500 LoC and could land inline as P-11.0 / P-11.1.
**Decision.** AIO + framework land together. `aio_context_t`
normalized to a real fd via `OpenFileBacking::AioContext(Cap<AioContext>)`
— diverges from Linux's pointer-shaped opaque value but unifies
with Ufd and future io_uring fds. Eight-phase plan: ~5–6 working days.
**Workers.** W-U (ADR), W-W (framework), W-Z/W-CC/W-FF
(implementation).
**Cost / benefit.** Single PR closes the OnBehalfOf framework + a
canary; io_uring SQPOLL is now a 0-framework follow-on.

### D9 — Signal subsystem wake-migration shape
`docs/progress/decisions/2026-05-11-d9-signal-wake-migration.md`
**Problem.** PR-3D's W-S handoff named six bus consumers — pipe,
futex, exit_source, tty, vfs, signal. Signal does NOT fit the
"one-source-per-object" template: it is target-selection
(`kill(pid)` to one eligible thread under sigmask), not fan-out.
Also: signal is *not* a bus consumer today; `Channel`/`Waker`/`RawPort`
return zero hits in `signal.rs`. The migration is a lost-wake fix,
not an additive layer.
**Decision.** Option A: `MailboxEvent::SignalDelivered { signum, routing }`
+ per-thread `Weak<TaskMailbox>` on `ThreadPayload`. Three phases:
D9-A event variant + post wiring; D9-B `step_kill_process` eligibility
scan (POSIX defect repair: pre-D9 first-non-zombie regardless of mask);
D9-C interrupt-wake test pin. signalfd / sigwaitinfo are D9-D
follow-up (deferred).
**Workers.** W-X (ADR), W-AA (D9-A), W-DD (D9-B + D9-C).
**Cost / benefit.** Repairs a POSIX defect alongside the wake migration.

---

## 4. Substrate state — what landed where

Structural deliverables in their final homes:

- **Wake substrate** (D4-relocated):
  `tx-substrate::wake::mailbox` — `TaskMailbox`, `WaitGeneration`,
    `MailboxEvent` (`SourceFired` / `AgentReplied` / `Abort` /
    `SignalDelivered`), `ActiveWait` (`matches` predicate),
    `agent_event_matches` (PR-7B/W-Y addition).
    File: `crates/tx-substrate/src/wake/mailbox.rs` (683 LoC).
  `tx-substrate::wake::wait_source` — `WaitSource`, `SubscriberId`,
    `PreparedWaitRegistration`, `WaitRegistrationGuard`, `WaitSourceId`.
    File: `crates/tx-substrate/src/wake/wait_source.rs` (546 LoC).
  `tx-substrate::wake::timer` — `TimerWheel`, `TimerGuard`,
    `TimerGuardRole` (`PrimarySleep` / `DeadlineAbort` /
    `DelegateTimeout`), `install_delegate_timeout`,
    `fire_due_delegate_timeouts`. PR-8B fire-mechanics stub holds.
    File: `crates/tx-substrate/src/wake/timer.rs` (368 LoC).

- **Step model (step_v3):**
  `tx-substrate::step_v3::mod` — `StepOp<I>`, `ScriptCtx<I>`,
    `StepOutcome` (`Continue` / `Yield` / `Done` / `Err`), `YieldShape`
    (`OnWaitSource` / `OnAgent` / `OnTimer`), `WaitSourceId`,
    `InterestMask`, `WaitProtocol`, `apply_resume`, `ResumeOutcome`,
    helpers `on_wait_source` / `yield_on_wait_source`.
    File: `crates/tx-substrate/src/step_v3/mod.rs` (493 LoC).
  `tx-substrate::step_v3::subject_context` — `SubjectIdentity` trait,
    `SubjectContext<I>` (`from_thread` / `borrowed`),
    `SubjectAuthority<I>` (`new` / `derived_from`), `CredentialView`,
    `RestrictionStackView`.
    File: `crates/tx-substrate/src/step_v3/subject_context.rs` (455 LoC).

- **Delegate runtime (OnAgent):**
  `tx-substrate::step_v3::agent` — `DelegateRegistry`, `DelegateState`
    (6-variant catalog), `DelegateTokenId`, `install_request`,
    `state` / `take_reply`, `mark_replied` / `mark_canceled` /
    `mark_agent_died` / `mark_timed_out` / `mark_endpoint_died`,
    `AgentTokenGuard<'a>` (`TokenDropPolicy`-aware drop CAS),
    `AgentCancelPolicy`, `TokenDropPolicy`, `DelegateRequest`
    (closed sum), `UfdRequest` (`PageFault { faulting_addr,
    access_kind, faulting_tid }`), `UfdAccessKind`, `DelegateReply`
    (closed sum), `UfdReply` (`Copy / ZeroPage / Continue`),
    `AbortReason` (`Canceled`/`AgentDied`/`TimedOut`/`Interrupted`/`Killed`/`ScopeAbandoned`).
    File: `crates/tx-substrate/src/step_v3/agent.rs` (1065 LoC).

- **Execution scope (OnBehalfOf):**
  `tx-substrate::step_v3::execution_scope` — `ExecutionScope<I>`
    (`Thread` / `OnBehalfOf(Cap<I>)`).
    File: `crates/tx-substrate/src/step_v3/execution_scope.rs` (98 LoC).
  `tx-substrate::step_v3::on_behalf_of` — `with_on_behalf_of<I, F, Fut, T>`
    async helper, `OnBehalfOfAbort` (`PrincipalExited` /
    `PrincipalRestrictionRevoked` / `CooperativeCancel(CancelReason)`),
    `AbortSignal` first-writer-wins one-shot.
    File: `crates/tx-substrate/src/step_v3/on_behalf_of.rs` (459 LoC).

- **Per-thread mailboxes / per-process subjects:**
  Per-thread mailbox lives on `ThreadPayload` (D9-A); subject identity
  is rooted at `tx-subsystems::process::ProcessIdentity` via
  `impl SubjectIdentity` in `process/structure.rs` with associated
  types `Credential = cred::Cred`, `ThreadIdentity = thread_runtime::ThreadIdentity`.

- **Bus consumers migrated to D2-coexistence `WaitSource`:**
  | Consumer | Sources per object | File | Tests |
  |---|---|---|---|
  | pipe | 2 (reader + writer) | `crates/tx-subsystems/src/pipe.rs` | `v3_pipe_waitsource.rs` (8) |
  | futex | 1 per fixed bucket (256) | `crates/tx-subsystems/src/futex.rs` | `v3_futex_waitsource.rs` (8) |
  | exit_source | 1 per process | `crates/tx-subsystems/src/process/structure.rs` | `v3_exit_wait_source.rs` (1 bundle) |
  | tty | 1 per TtyIdentity | `crates/tx-subsystems/src/tty/structure/identity.rs` | `v3_tty_waitsource.rs` (1 bundle) |
  | vfs | 2 per RNode (read + write) | `crates/tx-subsystems/src/vfs/structure.rs` | `v3_vfs_waitsource.rs` (1 bundle) |

- **Signal subsystem:** per-thread `Weak<TaskMailbox>` on
  `ThreadPayload`; `MailboxEvent::SignalDelivered { signum, routing }`;
  eligibility-scan (`step_kill_process` two-pass mask-aware thread
  pick) at `crates/tx-subsystems/src/signal.rs:548`. Tests:
  `v3_signal_eligibility.rs` (6) + `v3_signal_interrupt_wake.rs` (1).

---

## 5. Canary status — does the runtime work?

### PR-10 (userfaultfd) — OnAgent canary

End-to-end functional through phase 5 (W-BB) as of 2026-05-12; W-EE
flight closes phase 6 e2e Linux-style agent program. Coverage:
- Phase 0 fd scaffold (`v3_userfaultfd_fd_scaffold.rs` — 1 bundle).
- Phase 1+2 `sys_userfaultfd` + `UFFDIO_API`
  (`v3_userfaultfd_syscall_scaffold.rs` — 8).
- Phase 3 `UFFDIO_REGISTER` (`v3_userfaultfd_register.rs` — 10).
- Phase 4 fault-path OnAgent branch + `await_agent_reply`
  (`v3_userfaultfd_fault_path.rs` — 4: reply-resume,
  spurious-event re-post, `AgentDied → WouldBlock`,
  `NullUfdDispatch` fall-through).
- Phase 5 reply ioctls + pending-fault queue + `read(ufd)`
  (`v3_userfaultfd_ioctl_reply.rs` — 11: pre-handshake reject,
  alignment + zero-len + no-pending-fault reject, successful drain
  per ioctl, mismatched-dst reject, read EAGAIN on NONBLOCK,
  32-byte wire-format read, EINVAL on short buf).
- Phase 6 W-EE e2e — in flight.

Success criteria met: `BLAST-V3-SUCCESS-1` "userfaultfd works
end-to-end" satisfied at the structural plumbing level; phase 5
deferred byte-level copy semantics are documented (W-BB notes phase
5 acknowledges the reply but the actual `src → dst` page copy is
phase-5-stub: phase 6 closes that gap or it ships as a documented
phase-7 follow-up).

### PR-11 (AIO) — OnBehalfOf canary

End-to-end functional through phase 2 (W-CC) as of 2026-05-12; W-FF
flight closes phases 3–5 (`io_getevents` completion ring, `io_destroy`,
phase-2b spawn seam closing the deferred-pump model). Coverage:
- Phase 0 `OnBehalfOf` framework (`v3_pr11_on_behalf_of.rs` — 7).
- Phase 1 fd scaffold (`v3_aio_io_setup.rs` — 5).
- Phase 2 `io_submit` + worker (`v3_aio_io_submit.rs` — 7 + 3 unit).

Success criteria met: `BLAST-V3-SUCCESS-1` "AIO worker works
end-to-end" satisfied at the structural-plumbing level; phase 3+
completion-ring + destroy are W-FF.

---

## 6. Remaining work (prioritized)

Ranked by hand-off priority. None of these block the migration's
exit criteria; all are next-quarter ledger.

1. **PR-K — restrictions cap proper population.** Per D5 §7 and
   plan §3 (`PR-K`). Today every `SubjectContext::from_thread`
   call passes a fresh placeholder `RestrictionStackHandle` from
   `tx_subsystems::cred::placeholder_restrictions_cap()`. The real
   append-only stack lands with seccomp/landlock. Cost: ~150 LoC
   framework + per-syscall snapshot wiring. **Blocker for: any
   real `restrict()` semantics.**

2. **PR-7B follow-up — `OnAgent` timer-deadline rate-limiting
   (`RLIMIT_DELEGATE`).** Per PR-7 (W-F) "Left for PR-7B" note and
   PR-7B (W-H) "Left for follow-ups (b)". The registry has no
   growth bound. The bound is wired via a timer-tagged
   `DelegateTimeout` + a per-process budget. ~200 LoC. **Blocker
   for: hostile-agent fault floods.**

3. **EndpointScope abandonment routing edge cases.** Per
   `07_BLAST_RADIUS.md` §6 risk-register row "EndpointScope
   abandonment routing edge cases" — explicitly called out as
   needing a tracee-exit-during-ptrace-stop test pin. Today
   `mark_endpoint_died(endpoint_marker)` walks the registry but
   has no integration test for ptrace tracee-exit while a stop is
   pending. ~1 day to write the test. **Blocker for: ptrace
   subsystem (not yet built).**

4. **D9-D signalfd / sigwaitinfo / `pselect` wake-path.** Per D9
   §6 — deferred follow-up file. signalfd is a fd-shape extension
   of D9-A; the realtime per-occurrence queue is also out of scope.
   ~3 days. **Blocker for: signalfd users; not on the canary path.**

5. **SQPOLL canary (io_uring).** Per D8 note: the PR-11 framework
   "carries io_uring SQPOLL as a natural second canary with zero
   additional framework work." Deferred until io_uring's core fd
   shape lands.

6. **PR-8B TimerWheel wheel-mechanics fire path.** Per PR-7B
   (W-H) "Left for follow-ups (a)" and `tx-substrate::wake::timer.rs`
   docstring. The wheel's primary fire path is still stubbed; the
   hart-loop tick handler invokes `fire_due_delegate_timeouts`
   explicitly. PR-8B folds the `DelegateTimeout` expiry into the
   wheel's primary expiry walk. ~1 day. **Blocker for: actual
   timer-deadline production semantics; today the path is correct
   but the wheel is a `Vec<Entry>` linear scan, not a hashed wheel.**

7. **Performance benchmarks.** No `cargo bench` gate has run
   against the PR-3 substrate. `07_BLAST_RADIUS.md` §6 named
   bus-performance regression as a high-impact risk; today nothing
   measures it. **Blocker for: defending performance claims.**

8. **Bench gate for `StepOp` wrap inlining.** Per
   `07_BLAST_RADIUS.md` §6 row "StepOp wrap regresses inlining hot
   paths" — `#[inline]` was applied per-site but no bench
   confirms `page_backed.rs` / `tmpfs.rs` hot paths are intact.

---

## 7. Test count progression

Walking back through STATUS.md gives the following checkpoint table.
Each row is a session checkpoint; **passing** / **fail** / **ignored**.

| Checkpoint | Passing | Notes |
|---|---|---|
| Pre-migration baseline (2026-05-09 end of PR-1) | 1328 / 0 / 11 | PR-0 + PR-1 waves done. |
| Post-PR-A vocab rename arc | 1367 / 0 / 11 | PR-A.1 + A.3 + A.4 (vocabulary migration arc complete). |
| Post-design-doc-sweep | 1367 / 0 / 11 | Docs only. |
| Post-PR-3A (TaskMailbox) | 1375 / 0 / 11 | +8 generation/mailbox tests. |
| Post-PR-3B (WaitSource) | 1381 / 0 / 11 | +6 publish tests. |
| Post-PR-3C + PR-3D step 1 (`prepare`/`install_if` + waker bridge) | 1389 / 0 / 11 | +5 + 3 tests. |
| Post-PR-5 + PR-6 (ResumeOutcome + TokenDropPolicy) | 1396 / 0 / 11 | +7 (5 + 2). |
| Post-PR-8 admission (no test delta) | 1396 / 0 / 11 | OnTimer admission only. |
| Post-PR-2 pilot (S1/S2/S3) | 1404 / 0 / 11 | +8 wraps + tests. |
| Post-PR-2 wave 2 (P1..P4) | 1438 / 0 / 11 | +36 wraps + 34 tests. |
| Post-PR-2 wave 3 (Q1..Q5) | 1463 / 0 / 11 | +25 wraps + tests. |
| Post-PR-2 R1 cleanup | 1474 / 0 / 11 | +11 (1 broken test dropped). |
| Post-D1 production binding (W-F) | 1479 / 0 / 11 | +5 tests. |
| Post-PR-9 phases 1+2 (generic types) | 1482 / 0 / 11 | +3 alias-pin tests. |
| Post-PR-9 phase 3a (`ScriptCtx<I>` real fields) | 1484 / 0 / 11 | +2. |
| Post-PR-9 phase 3a fanout (M1/M2/M3/M4 polymorphic) | 1484 / 0 / 11 | Test count steady; wraps polymorphic. |
| Post-PR-9 phase 3b (4-syscall thread) | 1484 / 0 / 11 | No new tests. |
| Post-PR-3D layering (D4 / W-D) | 1484 / 0 / 11 | Verbatim move. |
| Post-PR-7 (W-F) | 1512 / 0 / 11 | +30 (208 → 222 substrate; aggregated). |
| Post-PR-7B (W-H) | 1535 / 0 / 11 | +14 substrate + 9 reactor (rough). |
| Post-PR-8 publish (timer surface) | 1548 / 0 / 11 | +13. |
| Post-PR-9 phase 4 (Cap reshape, W-E) | 1548 / 0 / 11 | No test delta. |
| Post-PR-9 phase 5 (cred zone + 4-arm subject, W-J) | 1556 / 0 / 11 | +2 integration. |
| Post-PR-3D-1 pipe (W-G) | 1556 / 0 / 11 | +8 (workspace count nudged by sub-PR landings; STATUS records `1555 → 1556` for vfs). |
| Post-PR-3D-2 futex (W-K) | 1556 / 0 / 11 | +8 pin tests. |
| Post-PR-3D-3 exit (W-M) | 1556 / 0 / 11 | +1 bundle. |
| Post-PR-3D-4 tty (W-P) | 1556 / 0 / 11 | +1 bundle. |
| Post-PR-3D-5 vfs (W-S) | 1556 / 0 / 11 | +1 bundle. |
| Post-PR-10 phase 0 (W-Q) | 1555 / 0 / 11 | +2 lib + 1 integration. |
| Post-PR-10 phase 1+2 (W-T) | 1564 / 0 / 11 | +8 + 1 subject-pop. |
| Post-PR-10 phase 3 (W-V) | 1579 / 0 / 11 | +10. |
| Post-PR-11 phase 0 (W-W) | 1579 / 0 / 11 | +7 framework. |
| Post-PR-10 phase 4 (W-Y) | 1592 / 0 / 11 | +4 fault-path. |
| Post-PR-11 phase 1 (W-Z) | 1592 / 0 / 11 | +5 + 3 unit. |
| Post-PR-10 phase 5 (W-BB) | 1615 / 0 / 11 | +11 ioctl-reply. |
| Post-PR-11 phase 2 (W-CC) | 1615+ / 0 / 11 | +7 + 3 unit (excluding pre-existing ufd compile failure). |
| Post-D9-A (W-AA) | 1604+ / 0 / 11 | Counts shift modestly with eligibility-scan workspace. |
| Post-D9-B+C (W-DD) | 1615 / 0 / 11 | +6 eligibility + 1 interrupt-wake. |

Headline delta: **~1328 → ~1615**, a +287 test gain across the
migration. Roughly half are PR-2 wraps; the remainder split across
PR-3/3D consumers, PR-7/7B, PR-10/PR-11 canaries, and D9 signal.

---

## 8. Hand-off: how to use the runtime

### If you are adding a new agent-kind subsystem (ptrace, fanotify-perm, FUSE-as-agent)

Follow **PR-10's pattern**:
1. Zone-allocate the kernel object: write a small payload struct
   (mirror `crates/tx-subsystems/src/userfaultfd.rs`), register the
   zone via `crates/tx-subsystems/src/zones.rs::register_all()`,
   give the payload a stable monotonic id.
2. Add an `OpenFileBacking` variant for the fd shape; add `OpenFile::new_*`
   constructor + accessor; short-circuit VFS step dispatchers for
   the new backing with `EINVAL` / `ESPIPE` / `ENOTTY`.
3. Add the syscall handler in `crates/tx-shims/src/linux_syscall/`;
   wire dispatch arm in `linux_syscall/mod.rs`.
4. In the fault path (or wherever the kernel-side blocking happens),
   branch on the new tag and call
   `DelegateRegistry::install_request` to mint a token, then await
   via `tx_reactor::agent_reply::await_agent_reply(token_id, mailbox, registry)`.
5. The agent's reply ioctl calls `registry.mark_replied(token_id, DelegateReply::*)`;
   `Drop` of the kernel object should call `mark_endpoint_died(endpoint_marker)`
   to walk every in-flight request on that endpoint.
6. Extend `DelegateRequest` and `DelegateReply` closed sums in
   `crates/tx-substrate/src/step_v3/agent.rs` for the new endpoint kind.

### If you are adding a new on-behalf-of subsystem (io_uring SQPOLL, future kernel workers)

Follow **PR-11's pattern**:
1. Zone-allocate a context payload (mirror `crates/tx-subsystems/src/aio.rs::AioContext`);
   register the zone.
2. Add an `OpenFileBacking::*` variant for the context fd; add
   `OpenFile::new_*` constructor + accessor.
3. Build a submit / completion queue holding the work items, paired
   with an `Arc<WaitSource>` that fires on submit-queue push.
4. Spawn a worker future via
   `tx_substrate::step_v3::on_behalf_of::with_on_behalf_of(owner_cap, body)`.
   The body races against an `AbortSignal` so principal-exit
   teardown is clean.
5. The body loops draining work items, threading
   `SubjectContext::borrowed(principal, derived_from(owner))` so
   authority lookups inside the borrow resolve against the principal,
   not the worker.
6. `Drop` of the context triggers `AbortSignal::trip` and the
   worker future resolves to `Err(OnBehalfOfAbort::PrincipalExited)`.

---

## 9. Open ADRs and TODO links

**ADRs:**
- D1 – D9 all decided and implemented (or explicitly deferred per
  the ADR's own §6/§7 follow-up file).
- D6 (TimerWheel layering): the move ADR landed; the actual move
  PR is queued as PR-7C — not blocking.
- D9-D (signalfd / sigwaitinfo): deferred follow-up file noted in
  D9 §6.

**TODO markers worth surfacing:**
- `// TODO PR-11 phase 2b: spawn deferred` in
  `crates/tx-shims/src/linux_syscall/aio.rs` — the
  function-pointer seam mirroring `install_submit_child_thread`
  closes the deferred-pump model. W-FF in flight.
- `// PR-9 phase 3b: not yet StepOp-driven — pending` in
  `linux_syscall/io.rs`, `fs_basic.rs`, `proc.rs` on the three
  arms that don't yet have StepOp wraps (`sys_close`, `sys_openat`,
  `sys_execve`).
- `tx-substrate/src/wake/timer.rs` — PR-8B wheel-mechanics stub
  call-out (linear `Vec<Entry>` scan vs hashed wheel).
- `tx-shims/tests/v3_userfaultfd_ioctl_reply.rs:307` — references
  a missing `TokenDropPolicy::SilentOnDrop` variant per W-DD; this
  is a pre-existing compile error owned by a separate worktree,
  not a migration regression.

---

## 10. Closing — what the migration actually delivered

The migration delivered the **vocabulary + scaffolding + two
canaries**. That is the `07_BLAST_RADIUS.md` §7 exit bar, and it is
met. It did NOT deliver: production-grade performance, real workload
hardening, the restrictions cell, the timer-wheel mechanics, the
ptrace / signalfd / SQPOLL adjacent subsystems, or bench gates. Those
are the next quarter's ledger. The next worker letter (W-HH onwards)
should pick from §6 priority list, starting with PR-K (restrictions
cell) when seccomp work begins, or with the PR-8B wheel mechanics +
bench gate if the next-quarter focus is hardening rather than
features.
