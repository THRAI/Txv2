# txKernel v3 migration — remaining work

**Status:** proposed; supersedes the forward-looking sections of
[`2026-05-09-v3-tdd-migration.md`](2026-05-09-v3-tdd-migration.md).
**Date:** 2026-05-11.
**Scope:** Plan the work that remains to retire v4 vocabulary and
populate the v3 substrate, after PR-0 and PR-1 (waves 1 → 9h-ε)
landed.
**Anchor:** [`docs/Txv3/07_BLAST_RADIUS.md`](../../Txv3/07_BLAST_RADIUS.md)
PR-1 → PR-11.

## 1. Status reality (measured 2026-05-11)

What landed before this plan:

- **PR-0.** Algebra pin tests + A-3 lint + txdoc harvest. Baseline
  locked at 1152 → 1179 passed; current baseline 1328 / 0 / 11.
- **PR-1 waves 1 → 9h-ε.** `StepOutcome` 5→4 essentially complete:
  variant counts in the tree are now `Done` 302 / `Err` 112 /
  `Yield` 47 / `Continue` 24 with **`Blocked` 4**, **`Advanced` 0**,
  **`AdvancedThenBlocked` 0**. Tests re-greened including the
  heavy `legacy_phase_a.rs` and `tmpfs/tests.rs` files.
- **v3 substrate scaffold present.** `WaitProtocol` (88 hits),
  `EndpointKind` (41), `SubjectContext` (26), `WakeHint` (24) all
  defined under [`crates/tx-substrate/src/step/`](../../../crates/tx-substrate/src/step/)
  and [`crates/tx-reactor/src/scheduler.rs`](../../../crates/tx-reactor/src/scheduler.rs).
  Files exist for `agent.rs`, `subject_context.rs`,
  `execution_scope.rs`, `wait_protocol.rs`, `endpoint_kind.rs`,
  `binding_obligations.rs`, `restriction_stack.rs`, plus four
  progress types.

What is **not** done despite the blast-radius doc treating it as
"already in the scaffold":

| v3 surface | State |
|---|---|
| `OnCarrier` vocabulary | **63 sites still using v4 spelling.** Rename to `OnWaitSource` not yet attempted. |
| `WakeCarrier` / `InterestConditions` | 29 / 29 sites. Renames not started. |
| `exit_port` → `exit_source` | 67 sites. Not started. |
| `Waker` retirement | **92 sites** across `tx-substrate/src/bus/` + reactor. The substrate spec says `TaskMailbox` replaces these; **`TaskMailbox` does not exist in the tree** (0 hits). |
| `WaitSource` / `WaitSourceId` / `WaitGeneration` / `PreparedWaitRegistration` | 0 hits. |
| `DelegateState` / `AgentTokenGuard` / `install_request` / `mark_agent_died` / `mark_timed_out` | 0 hits. `step_v3/agent.rs` defines the *types* (`DelegateToken`, `DelegateEndpoint`, `DelegateRequest`, `CancelPolicy`) but not the state machine. |
| `StepOp` wrap of free `step_*` fns | **5 `impl StepOp` blocks** vs **178 free `step_*` fns**. Approximately 173 fns to wrap. |
| `apply_resume` / `ResumeOutcome` | 0 hits. |
| `OnBehalfOf` ExecutionScope (PR-J) | 0 hits beyond `execution_scope.rs` skeleton. |
| `SubjectAuthority.restrictions` (PR-K) | 0 hits. |
| v4 `FsOps` trait declarations + 21 impls | **Live and deferred** per wave-9h-ζ postmortem (see §2). |

The headline correction: doc 07 treated the `step_v3/` directory
as proof that PR-2/PR-3/PR-7 were partially built. Reading the
files shows it's the **type sketch**, not the runtime. Net-new
code volume for PR-3 (wake substrate), PR-7 (OnAgent runtime),
and PR-2 (StepOp wrap) is full, not reduced.

## 2. The wave-9h-ζ blocker — recommendation

Wave 9h-ζ tried to delete the v4 `FsOps` trait and was reverted.
The decision note in `STATUS.md` (2026-05-09) names two real
blockers:

1. **Inherent-vs-trait same-name ambiguity inside trait impls.**
   `Self::method` UFCS inside `impl Trait for X` resolves to the
   *trait* method, not the inherent same-name method, when the
   trait is in scope. So `impl FsPageBacking for Devfs { fn
   fallocate(...) -> V3Outcome { Self::fallocate(self, ...) } }`
   re-enters itself instead of calling the v4 inherent helper.
2. **~17 tty-test sites match v4 `StepOutcome` shape directly
   off fixture methods.** Removing the v4 trait leaves them
   reaching for either private inherent methods or a v3 trait
   whose variants don't match their arms.

Three paths were named: rename inherent methods, inline v4 logic
into v3 impls, or migrate tests to v3 outcome shape.

**Recommendation: take path 3 (migrate tests), not as part of
v4 trait deletion, but as a prerequisite that lands separately.**
Reasons:

- Path 1 invents a private vocabulary (`_v4_inner_lookup`) that
  exists solely to dodge the language rule. Future readers will
  not understand why the inherent helper has a suffix; the
  suffix encodes commit-history, not structure.
- Path 2 duplicates ~1,500 lines of fs logic across two trait
  worlds with no inflight invariant catching divergence. The
  cost is paid twice on every future bug.
- Path 3 deletes the v4 outcome shape from the only places
  that still consume it (tests). After test migration, the v4
  trait itself becomes dead with no dispatchers — the deletion
  PR shrinks from "rename + paste + audit" to plain garbage
  collection. The work was always going to happen for v3's
  invariant story; doing it now retires v4 trait declarations
  as a side effect.

The work split:

- **PR-1.5** (new, ~17 + N test-site migrations, 2 days):
  Rewrite v4 outcome match arms in `tty/tests/legacy_phase_a.rs`,
  `process/tests/exit_port.rs`, and the other tests STATUS named.
  After this PR, the v4 `StepOutcome` shape exists nowhere in
  the tree.
- **PR-1.6** (mechanical, 1 day): delete v4 `FsOps` trait,
  delete 21 v4 trait impls, delete 4 v4 factory methods. No
  source-level surprises because PR-1.5 already retired the only
  v4-outcome consumers.

This unblocks the bookkeeping for "PR-1 retires v4 vocabulary"
in the success criteria.

## 3. Remaining PR sequence

Numbered to match `docs/Txv3/07_BLAST_RADIUS.md` §5.2 PR letters,
with the local-history wave annotations preserved.

| # | Work | Estimate | Risk | Notes |
|---|---|---|---|---|
| ~~**PR-1.5**~~ | ~~Migrate test files off v4 `StepOutcome`~~ — **ALREADY DONE** as of 2026-05-09 PR-1 waves. Re-verified empty in PR-A.1 audit. | — | — | — |
| **PR-1.6 (REVISED)** | **Keep `FsOps`.** Doc + naming cleanup only, no trait deletion. Per [`2026-05-11-pr-1-6-keep-fsops.md`](../decisions/2026-05-11-pr-1-6-keep-fsops.md): v3 invariant is StepOutcome shape unification, not trait-identity unification. Wave-9h-ζ blocker dissolves. | ~½ day | Low | Done in this session: FsOps doc header marks it canonical v3, links the decision. No production dispatch change. |
| ~~**PR-A.1**~~ | ~~Vocabulary rename: `OnCarrier` → `OnWaitSource` (63 sites)~~ **LANDED 2026-05-11** via 6-worker parallel dispatch (1 foundation + 6 workers + integration sweep, ~15 min wall). Bundled struct renames (A.2) into the same PR. | — | — | — |
| ~~**PR-A.2**~~ | ~~`WakeCarrier`/`InterestConditions` renames~~ **landed with A.1**. | — | — | — |
| ~~**PR-A.3**~~ | ~~`exit_port` → `exit_source` rename~~ **LANDED 2026-05-11** sequentially (no fanout — surface too small to justify worker coordination). 9 files, 67 sites. File rename `tests/exit_port.rs` → `tests/exit_source.rs`. `read_wq`/`write_wq` (3 sites total) deferred to PR-A.4 micro-rename. | — | — | — |
| ~~**PR-A.4**~~ | ~~bare `carrier` identifier audit (71 sites)~~ **PARTIAL: module/method axis LANDED 2026-05-11** via 4-worker parallel dispatch. `wait_carrier` module → `wait_source`, `WaitToken::carrier()` → `source_id()`, 150 sites across 19 files. **Remaining**: `*_carrier_id` suffix audit (33 sites: `wait_carrier_id` field/method/local, `reader_wait_carrier_id`, `writer_wait_carrier_id`, `exit_source_carrier_id`) — deferred to a follow-up PR (judgment-heavy: rename to `_source_id`, drop `_carrier_` infix, or keep as-is per-site). | residual ~half-day | Low | — |
| ~~**PR-2 (COMPLETE)**~~ | ~~`StepOp` wrap of ~178 free `step_*` fns~~ **LANDED 2026-05-11** through 3 waves of fanout + R1 cleanup. **Actual production surface: ~80 fns** (the 178 grep count was inflated by test fns and trait method bodies). 80 wraps cover: page_backed lifecycle/cross_variant/.rs/user_buffer, futex, cred, pipe, process/execution, signal, thread_runtime, tty/execution/{step_read, step_write, step_poll_hardware, step_ioctl, step_openpty, step_master_close, step_hangup, step_ingest}, vfs/execution (OpenFile methods), tx-reactor/hart_loop. **Carve-outs**: `vfs::walker::{step_walk, step_open}` async (per [D3](../decisions/2026-05-11-d3-walker-async-carveout.md)); vm/* and tx-fs/* have no free `step_*` (methods/scripts only). Pattern validated: `&'a Guard<'a>` single-lifetime, `Cap<T>` by value, `CredChange`/`Result<T,E>` lifted via `StepOutcome::Done(_)`. **One R1 test regression** (wrong EOF assertion poisoning `EPOCH_TEST_LOCK`) caught and reverted. | — | — | — |
| **PR-3 (PARTIAL LANDED)** | Wake substrate per [`2026-05-11-pr-3-wake-substrate-shape.md`](../decisions/2026-05-11-pr-3-wake-substrate-shape.md). **3A LANDED 2026-05-11**: `TaskMailbox` / `WaitGeneration` / `MailboxEvent` / `ActiveWait` in `tx-reactor/src/mailbox.rs` (8 tests). **3B LANDED 2026-05-11**: `WaitSource` / `SubscriberId` with subscriber list, `notify` posts overlap-not-full mask, compacts dead subscribers (6 tests). **3C LANDED 2026-05-11**: `WaitSource::prepare` → `PreparedWaitRegistration` → `install_if(predicate)` → `WaitRegistrationGuard` RAII chain; lost-wake-fix primitive surface (5 tests). **3D step 1 LANDED 2026-05-11**: `TaskMailbox` stores optional `core::task::Waker`, `post` wakes on enqueue + overflow paths (3 tests). **3D mass migration DEFERRED**: 94 direct `Waker` sites concentrated in `tx-substrate/src/bus/` (31 sites: queue.rs 11, port.rs 11, graph.rs 9) need careful per-site migration order — a directed PR with bench gate. | 1 day done; ~1 day deferred | — | All 22 new tests green; workspace baseline 1367 → 1389. |
| ~~**PR-5**~~ | ~~`apply_resume` + `ResumeOutcome`~~ **LANDED 2026-05-11**. `ResumeOutcome` 4-variant closed catalog + default-reject `apply_resume`. Placeholder types `DelegateReply`/`TimerId`/`AbortReason` added. 5 tests. | — | — | — |
| ~~**PR-6**~~ | ~~`CancelPolicy` rename + `TokenDropPolicy`~~ **LANDED 2026-05-11**. 9 sites renamed; new `TokenDropPolicy` enum (`CancelOnDrop`/`Abandon`; `KeepAlive` reserved). 2 tests. | — | — | — |
| **PR-7** | `OnAgent` runtime: `DelegateState` state machine, `install_request`, `AgentTokenGuard`, `mark_agent_died`, `mark_timed_out`, `EndpointScope` abandonment | 3 days | Medium | Lands the dormant `agent.rs` types into a working runtime. Needs DTOK-3 invariant test + reply-vs-timeout race test. |
| ~~**PR-8 (admission half)**~~ | ~~`YieldShape::OnTimer` variant + classify rules + match-arm sweep~~ **LANDED 2026-05-11** via 3-worker fanout (T1 substrate tests, T2 page_backed+futex, T3 shims+tmpfs). 13 sites swept; most production paths reject OnTimer with `EIO` parallel to OnAgent; tmpfs uses `unreachable!`; tests panic. Wildcard `_ => ...` sites in `cross_variant`, `targeted_read`, `core_tests`, `futex`, `tmpfs` outer-match required no edit. | — | — | — |
| **PR-8B (TimerGuard surface)** | `TimerGuard` (`PrimarySleep`/`DeadlineAbort`/`DelegateTimeout` roles) + make `tx_reactor::TimerToken` public. Wires the OnTimer admission into actual reactor timer infrastructure. | 1 day | Low | Single author; `TimerToken` private today in `tx-reactor/src/timer.rs`. |
| **PR-9** | `SubjectContext` threading through canonical syscalls (sys_open / read / write / fork / execve / close / pipe) | 2 days | Low | Threads existing scaffold through the shim layer. |
| **PR-10** | First agent canary — userfaultfd | 1–2 weeks | Medium | Validates PR-7 against a real user. ARCH-3 review for the new YieldShape consumer. |
| **PR-11** | `OnBehalfOf<P>` framework + first user — AIO worker | 1 week | Medium | Defers cleanly until needed. |
| **PR-K** | `SubjectAuthority.restrictions` cell | Deferred | Low | Lands with first seccomp/landlock work. |

Total to fully retire v4 vocabulary and have a populated v3
substrate (PR-1.5 through PR-9): **~20 working days**, of which
~13 days are PR-2 + PR-3 + PR-7 — the three substantive PRs.

## 4. Wave / batching strategy

PR-A.1 through PR-A.4 (vocabulary renames) and PR-1.5 / PR-1.6
(v4-shape retirement) are mechanical. They can land as fast as
review allows; no foundation freeze needed.

PR-2 (StepOp wrap) is parallelizable per subsystem. Reuse the 14
workers from the 2026-05-09 plan §4 partition with reduced scope
(several workers will be no-ops; `page_backed`, `tty`, `vfs`, `vm`
do the heavy lifting). Wave 1: substrate-adjacent subsystems
(`vm`, `page_backed`). Wave 2: upper subsystems (`tty`, `vfs`,
`process`, `pipe`, `futex`). Wave 3: shims and scripts.

PR-3 (wake substrate) is **the foundation freeze candidate** in
this remaining plan. Hold non-substrate subsystem PRs for the
duration. Concentrated retire surface in 9 files in `bus/` + 1
in `reactor/`.

PR-7 (OnAgent runtime) follows PR-3 and PR-6 (which renames the
enum it consumes). It can run in parallel with PR-8 and PR-9
because the surfaces are disjoint.

## 5. Verification gates

Inherited from 2026-05-09 plan §7; explicitly enforced for every
PR in §3:

- `cargo build --workspace --tests` clean.
- `cargo test --workspace --lib --tests -- --test-threads=1` ≥
  baseline (1328 today).
- `cargo xtask lint arch` — no new warnings.
- `cargo xtask test busybox-smoke --target rv64-qemu` still
  passes (running canary).
- `cargo xtask progress validate` — JSON records green.
- `docs/progress/STATUS.md` updated per CLAUDE.md catch-up rule.
- After PR-A.* / PR-1.6: re-run blast-radius surface counts;
  expected zeros for `OnCarrier`, `WakeCarrier`,
  `InterestConditions`, `exit_port`, `Advanced`, `Blocked`,
  `AdvancedThenBlocked`, `StepOutcome` v4 variants.
- After PR-3: bench `page_backed.rs` / `tmpfs.rs` / bus hot
  paths; no regression > 5%.
- After PR-7: DTOK-3 reply-vs-timeout race test passes; agent
  death during pending request produces `SENTINEL_DEAD`.

## 6. Risks (delta from doc 07 §6)

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| PR-1.5 test rewrite exposes v4-outcome semantics that don't fit v3 shape (e.g. test depended on `AdvancedThenBlocked` semantics that `Yield { progress }` rounds away) | Medium | Medium | Per-test review; if mapping is genuinely lossy, split the test into "progress observed" + "shape observed" assertions. |
| PR-A.4 (bare `carrier` audit) misclassifies a domain "carrier" as the wait-source kind | Low | Low | Commit message lists every rename decision; reviewer veto-able. |
| PR-2 `StepOp` wrap regresses inlining on `page_backed::step_range`, `step_range_with_user_buffer`, `materialize_page` (named in 2026-05-09 plan §9d) | Low | Low | `#[inline]` small impls; bench before/after. |
| PR-3 `TaskMailbox` substrate regresses bus performance | Low | High | Performance gate in CI; benchmark before merge. |
| PR-7 OnAgent deadline race | Low | Medium | DTOK-3 invariant test + dedicated reply-vs-timeout test before merge. |
| `step_v3/` scaffold has divergent intent from spec (doc 07 §6's highest-impact item — never resolved) | Medium | High | **Before PR-3**, walk through `step_v3/agent.rs`, `wait_protocol.rs`, `endpoint_kind.rs` line-by-line vs `03_STEP_MODEL_v2` and `05_DELEGATE_v1`. If divergence is real, reconcile in a pre-PR-3 ADR. |
| Reviewer fatigue across the 20-day stretch | Medium | Low | Rotate primary reviewer per PR family; PR-A.* and PR-1.* are cheap reviews, front-load them. |

## 7. Next concrete action

**Vocabulary migration arc COMPLETE (2026-05-11).** PR-A.1 + A.3 + A.4
landed via parallel-dispatch + targeted sequential cleanups. Tree
green, baseline 1367/0/11 preserved.

**Next: PR-2 (StepOp wrap) but has a hard dependency.** Currently
`ScriptCtx` is an empty placeholder (`pub struct ScriptCtx { _private:
() }`). Each `step_*` fn signature accepts varied params (`tty`,
`out`, `guard`, `&SubjectContext`, ...). Wrapping into `impl StepOp
for FooOp` means each `step()` call gets only `&mut ScriptCtx` — the
varied params have to come from either `self` (owned by op) or
`ScriptCtx` (threaded through the script). Without resolving this
shape, workers can't dispatch — every site has the same design
question.

**PR-9 phases 1 + 2 LANDED 2026-05-11.** `step_v3::SubjectAuthority`,
`SubjectContext`, `ScriptCtx`, and `StepOp` trait are now generic
over `I: SubjectIdentity` with default `I = ProcessIdentity` (the
step_v3 placeholder). 80 PR-2 wraps compile unchanged. Production
aliases live in `tx-shims/src/lib.rs`: `KernelScriptCtx`,
`KernelSubjectContext`, `KernelSubjectAuthority`. **Phase 3
remaining**: populate `ScriptCtx<I>` with real fields (subject,
deadline, mailbox handle) and thread `&mut KernelScriptCtx` through
the 7 canonical syscalls. That's a coherent follow-on session.

**Original PR-1.5.** Migrate v4-outcome match arms out of test files. The
specific sites named in STATUS (2026-05-09 wave-9h-ζ postmortem):

- `crates/tx-subsystems/src/tty/tests/legacy_phase_a.rs` (DevptsInstance.lookup / readdir, ~17 sites).
- `crates/tx-subsystems/src/process/tests/exit_port.rs` (19 sites).
- Whatever the "N others" audit surfaces — full sweep with `rg
  "StepOutcome::(Advanced|Blocked|AdvancedThenBlocked)" --type
  rust` (returns 4 hits today).

Single author, one session, mergeable. After this lands, the
PR-1.6 GC commit becomes trivial.

## 8. What this plan does *not* do

- **Does not relitigate the wave-9h-ζ decision.** Wave-9h-ε is
  the stable resting point; PR-1.5 picks up the work the
  deferred wave left on the table without reverting it.
- **Does not promote `step_v3/` to canonical naming.** That
  rename can wait until after PR-9 when the v3 substrate is
  populated; doing it sooner just churns import paths.
- **Does not land `OnEdge` / `OnHandoff` YieldShapes.** Deferred
  per doc 07.
- **Does not start seccomp / landlock work** (PR-K). The
  `SubjectAuthority.restrictions` cell waits for its first user.
- **Does not change the migration's "no compat layer" rule.**
  PR-1.5 is *not* a compat layer; it retires v4 outcome shape
  from the only consumers that still need it, ahead of v4 trait
  deletion.
