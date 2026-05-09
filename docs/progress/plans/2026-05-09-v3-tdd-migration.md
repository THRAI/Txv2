# txKernel v3 TDD migration

**Status:** proposed.
**Date:** 2026-05-09.
**Scope:** Land the v3 design refresh ([`docs/Txv3/`](../../Txv3/)) into tx-* code, in TDD shape, exploiting parallel worker subagents where the work is genuinely independent.

## 1. Framing

The v3 migration is two disciplines welded together:

- **Compile-driven TDD for the catalog refactor.** Doc 07 quantifies the bulk: 354 mechanical sites for `StepOutcome` 5→4, 135 step-fn signatures for the `StepOp` wrap, mostly across `tx-subsystems` (47 files) and `tx-shims` (15 files). Today's algebra lives at [`crates/tx-subsystems/src/execution.rs:87`](../../../crates/tx-subsystems/src/execution.rs) with the v4 five-variant shape. Rust's exhaustive `match` is the strongest closed-catalog test we could write — once we shrink the enum, every consumer breaks until it conforms. The current 1,109-test suite (per `STATUS.md` 2026-05-08) is the behavioral net.
- **Genuine test-first for net-new framework.** `OnAgent` endpoints, `DelegateToken` zone, `SubjectContext` threading, `OnBehalfOf<P>` borrows, and the restriction-stack do not exist today. Tests are written first, the type sits unimplemented and red, then production code goes green.

## 2. Landing order (matches doc 07 §5)

| PR | Shape | Parallelizable? | Lead-time |
|---|---|---|---|
| PR-0 | Pre-flight: baseline lock + algebra pin tests + anti-pattern lints | No | 1–2 days |
| PR-1 | `StepOutcome` 5→4 + `YieldShape::OnCarrier` (354 sites) | **Yes** — fan-out per subsystem | 5 days wall, ~1 day with fan-out |
| PR-2..N | Per-subsystem `StepOp` + `StepProgress` wrap (135 fns) | **Yes** — one worker per subsystem | ~3 days wall |
| PR-3 | `SubjectContext` skeleton in tx-shims/tx-policy | No (single coherent author) | 2 days |
| PR-4 | `OnAgent` zones (`DelegateEndpoint`, `DelegateToken`) | Partial (test file in parallel with stub) | 3 days |
| PR-5 | `Waiting::handle` for OnAgent (synthetic kind) | No | 2 days |
| PR-6 | First agent canary — userfaultfd | No | 1–2 weeks |
| PR-7 | `OnBehalfOf<P>` framework | No | 3 days |
| PR-8 | First borrow canary — AIO worker | No | 1 week |
| PR-9 | `SubjectAuthority::restrictions` stub | No | 1–2 days |

Subsequent (FUSE delegation, fanotify-perm, ptrace, io_uring SQPOLL, seccomp BPF) each their own roadmap entry.

## 3. PR-0 — Pre-flight

Sequential, owned by one author. Output is the safety net that lets PR-1 fan out without fear.

1. **Baseline lock.** Record `cargo test --workspace --lib --tests -- --test-threads=1` per-crate counts in `docs/progress/decisions/2026-05-09-v3-baseline.md`. Re-run after every subsequent PR; deltas are reviewed.
2. **Algebra pin tests.** New file `crates/tx-substrate/tests/v3_algebra.rs`:
   - `StepProgress` monoid laws (`extend` associative, `EMPTY` identity, monotonicity) as proptest properties — the load-bearing invariant for STEP-3.
   - `DriveMode::classify` matrix as a table-driven test, one row per (mode × shape × progress_empty).
   - `YieldShape` and `StepOutcome` exhaustive-match smoke (no wildcard arm).
3. **Anti-pattern lints** in [`xtask/src/lint.rs`](../../../xtask/src/lint.rs):
   - **A-3** (`.await` inside `fn step(`).
   - **A-7** (direct field mutation skipping `substrate::index::*` / `zone::sign`).
   - **A-2** (witness types stored in `&mut self` or statics).
   The unmechanizable invariants (A-11, A-14) stay as PR-template review checklist.
4. **txdoc tag harvest.** Extend `cargo xtask lint` to assert every `txdoc:TXV3-*` cited in code comments resolves to a tag in `docs/Txv3/`.

PR-0 lands green with no behavior changes — only tests, lints, and a baseline doc.

## 4. PR-1 — `StepOutcome` 5→4 + `YieldShape::OnCarrier`

Three-step shape that decouples "introduce new types" from "migrate consumers" and enables fan-out:

### Step 1 — introduce parallel module (sequential, single commit)

- Add `tx_substrate::step_v3` exposing the new `StepOutcome<T, P>`, `YieldShape`, `StepProgress`, `DriveMode`, `StepOp` per [`docs/Txv3/03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md).
- The old `tx_subsystems::execution::StepOutcome` stays untouched; nothing imports `step_v3` yet.
- Workspace tests pass unchanged. This is the **branch point** — every fan-out worker stacks on this commit.

### Step 2 — per-subsystem migration (parallel, fan-out)

Each worker takes one disjoint write scope, flips imports from old to `step_v3`, applies the mapping rules, re-greens its crate's tests, and reports. **Mapping rules** (compile-enforced, documented in the new module's rustdoc):

| Old | New |
|---|---|
| `Done(T)` | `Done(T)` |
| `Err(e)` | `Err(e)` |
| `Advanced(T)` | `Continue { progress: P }` *or* `Done(T)` (per-call-site decision; reviewer gate) |
| `Blocked(WaitToken { carrier, interest })` | `Yield { progress: P::EMPTY, shape: OnCarrier { carrier, interests } }` |
| `AdvancedThenBlocked(T, WaitToken)` | `Yield { progress: P::from(t), shape: OnCarrier{..} }` |

Workers (write-scope partition):

| Worker | Write scope | Files | StepOutcome refs |
|---|---|---|---|
| **W-vfs** | `crates/tx-subsystems/src/vfs/` | 11 | ~150 |
| **W-page-backed** | `crates/tx-subsystems/src/page_backed/`, `page_backed.rs` | 5 | ~250 |
| **W-tty** | `crates/tx-subsystems/src/tty/` | 7 | ~190 |
| **W-vm** | `crates/tx-subsystems/src/vm/` | 4 | ~70 |
| **W-mount-pipe-futex** | `mount.rs`, `pipe.rs`, `futex.rs`, `device.rs`, `sync.rs` | 5 | ~30 |
| **W-process-signal** | `crates/tx-subsystems/src/process/`, `signal/`, `cred/`, `thread_runtime/` | ~10 | ~50 |
| **W-initramfs** | `crates/tx-subsystems/src/initramfs/` | 1 | ~28 |
| **W-fs** | `crates/tx-fs/src/` | 5 | ~200 |
| **W-ext4** | `crates/tx-ext4/src/`, `crates/tx-ext4-format/src/` | 2 | ~25 |
| **W-shims-fs** | `crates/tx-shims/src/linux_syscall/{fs_basic,fs_mut,fs_path}.rs` | 3 | ~85 |
| **W-shims-io** | `crates/tx-shims/src/linux_syscall/{io,vm,signal,time,misc,cred,proc}.rs` | 7 | ~50 |
| **W-shims-mod** | `crates/tx-shims/src/linux_syscall/{mod.rs,tests}` | rest | ~40 |
| **W-scripts** | `crates/tx-scripts/src/` | 2 | ~35 |
| **W-kernel** | `crates/tx-kernel/src/` | 4 | ~20 |

14 workers, disjoint write sets. Each gets the same brief: read [`docs/Txv3/03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) §2–§5, follow the mapping table, run `cargo build -p <crate>` clean, run that crate's tests, report changed-files + new test counts.

**Coordination rule:** workers do **not** edit `tx_substrate::step_v3` (the new module) or any file outside their scope. Cross-crate consumer issues (e.g. tx-shims calls into tx-subsystems) are handled because the new module is shape-stable from PR-1 step 1.

Recommended fan-out: spawn 4–6 workers concurrently, drain, spawn the next batch. Don't spawn all 14 at once — the main agent must integrate each return into a stack of commits and re-run workspace tests between batches.

### Step 3 — sequential flip + delete (single commit)

Once all workers report green:

- Re-export `step_v3::*` as the canonical `tx_subsystems::execution::*` (or rename the source of truth and keep `step_v3` as the home).
- Delete the v4 enum.
- `cargo build --workspace --tests` clean; full workspace test suite ≥ baseline (1,109+).

## 5. PR-2..N — `StepOp` + `StepProgress` per subsystem

Same fan-out shape as PR-1 step 2, but the unit of work is "wrap free `step_*` fns into `impl StepOp`" with a per-op `Progress` associated type. ~135 fns across 32 files (doc 07 §3).

For each worker:

1. **Test first.** Add a `StepOp`-impl test stub for one representative op in the worker's subsystem (e.g. `vfs::ReadOp` for W-vfs). The test drives it through a fake `ScriptCtx` and asserts the new outcome shape.
2. **Wrap.** Convert each `step_*` fn into an `impl StepOp` with the right `Progress` type from the closed list (`NoProgress`, `ByteProgress`, `PageProgress`, `EntryProgress`, `IoVecProgress`).
3. **Re-green.** Subsystem's existing tests pass with no behavior diff.
4. **Inlining check.** `#[inline]` small impls; bench `page_backed.rs` and `tmpfs.rs` (heaviest hot paths) before/after — neutralizes the inlining-regression risk in doc 07's risk register.

Same 14-worker partition can be reused, though several workers (W-initramfs, W-mount-pipe-futex) are likely no-ops here since they have few step fns.

## 6. Net-new framework PRs (sequential, test-first)

For each, the test file is **written and red before** any production code:

| PR | Test file | Production cap |
|---|---|---|
| **SubjectContext skeleton** | `tx-shims/tests/subject_context.rs` — `from_thread`, `borrowed`, SUBJ-1 (no global accessor), SUBJ-3 (replacement is publication boundary) | ~500 LoC |
| **OnAgent zones** | `tx-substrate/tests/delegate_zone.rs` — token liveness, `SENTINEL_DEAD` on endpoint death, `RLIMIT_DELEGATE` bound, DELEGATE-3 cap-typed kind discrimination | compile-only at first |
| **`Waiting::handle` for OnAgent** | `tx-reactor/tests/agent_resume.rs` — synthetic `EndpointKind::Synthetic` round-trip, deadline expiry, `CancelPolicy` matrix | drives synthetic kind |
| **userfaultfd canary** | `tx-subsystems/tests/uffd_delegate.rs` — golden trace of one page-fault round-trip, abort-on-agent-death | ~1,500 LoC |
| **`OnBehalfOf` framework** | `tx-shims/tests/on_behalf_of.rs` — borrow lifetime, `Killable` abandonment → `EOWNERDEAD`, scope-bound resource drop, SCOPE-5/SCOPE-6 | ~500 LoC |
| **AIO worker canary** | `tx-shims/tests/aio_borrow.rs` — `OnBehalfOf` scope across batch, fixed-buffer pin lifetime | ~1,000 LoC |
| **Restriction-stack stub** | `tx-policy/tests/restriction_stack.rs` — append-only, walk order, replacement is SUBJ-3 commit | ~150 LoC |

The test file is the **review artifact** — reviewers read tests first, then production. Parallelization is limited here: a worker can draft the test file in parallel with main agent designing the type, but converging shape needs a single coherent author.

## 7. Verification gates (every PR)

- `cargo build --workspace --tests` clean
- `cargo test --workspace --lib --tests -- --test-threads=1` ≥ baseline
- `cargo xtask lint arch` — no new warnings
- `cargo xtask test busybox-smoke --target rv64-qemu` still passes (the running canary)
- `cargo xtask progress validate` — JSON records green
- `docs/progress/STATUS.md` updated per CLAUDE.md catch-up rule

## 8. Parallelization topology (subagent fan-out)

Per `.agents/skills/tx-agentic-development/SKILL.md`:

- **Workers get disjoint write scopes** (the partition table in §4 step 2).
- **Workers are told they are not alone** — main agent owns integration.
- **Worker brief** is uniform: read [`docs/Txv3/03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) §2–§5 + this plan §4 mapping table; constraint to write scope; run crate-local build + test; report changed-file list + test count delta.
- **Fan-out batch size:** 4–6 concurrent workers. Larger batches saturate review and integration; smaller ones leave wall-time on the table.
- **Sequential carve-outs:** PR-0, the new-module commit at the head of PR-1, the final flip-and-delete commit, and every net-new framework PR.

Locator/analyzer roles are **not** needed for PR-1 — the partition is mechanical and the mapping is enumerated. They become useful for PR-2..N (figuring out per-fn `Progress` choice) and especially for the userfaultfd and AIO canaries (analyzing existing v4 substrate to graft onto).

## 9. Parallelization dry-run findings (2026-05-09)

Three read-only locator/analyzer subagents simulated workers W-vfs, W-page-backed, and W-shims-fs against the partition table in §4. They each enumerated their scope, classified mapping ambiguities, and looked for cross-scope leaks. Headline findings:

### 9a. Site-count undercount

The §4 estimates were **25–50% low** (counting tests as well as production):

| Worker | Plan estimate | Dry-run actual | Notes |
|---|---|---|---|
| W-vfs | 150 | 187 | 7 prod files + tests |
| W-page-backed | 250 | 372 | 5 prod + 7 test files; mostly test margin |
| W-shims-fs | 85 | 99 | 3 files, all consumer-side |

Doc 07's 354 production-site number is plausibly accurate; the §4 totals include tests, which are higher. **Action:** treat per-worker hour estimates as firm but expect wall-time slightly larger than 1 day for the full PR-1 step 2 fan-out.

### 9b. Two real cross-worker dependencies surfaced

- **W-page-backed → W-fs** (type-boundary). page_backed calls `mount.payload().fs_page_backing.fetch_page()`, `.truncate()`, `.fsync()`. Those return `StepOutcome` from the tx-fs trait. Until W-fs flips its trait impls to `step_v3`, page_backed cannot match against the new shape.
  **Resolution:** batch W-fs and W-page-backed in the same fan-out wave, or extend PR-1 step 1 to migrate the `fs_page_backing` trait return types as part of the foundational commit. Recommended: **batch them in wave 1**.
- **W-vfs ↔ W-tty** (semantic boundary). `crates/tx-subsystems/src/vfs/execution.rs` has 5 ioctl-passthrough sites that wrap `tty::execution::step_ioctl_*`. Whether the wrap propagates `Continue` or `Done` depends on TTY's classification (re-invoke vs. terminal-result). W-vfs cannot decide unilaterally.
  **Resolution:** W-tty produces an "ioctl Advanced semantics" decision note as part of its commit; W-vfs's reviewer gates on it, OR W-vfs lands with `AMBIGUOUS-IOCTL-N` markers that a follow-up commit resolves. Recommended: **W-tty first in wave 1**, W-vfs in wave 2 reads the resolved decisions.

### 9c. A simplification surfaced for syscall-arm workers

W-shims-fs (and by extension W-shims-io, W-shims-mod, all syscall-arm workers) discovered: **at the syscall-arm layer, `Done(T) | Advanced(T)` always conflate** — the syscall returns when a complete value is ready and never cares about "made progress mid-step." Pre-decided mapping rule for syscall-arm workers:

> All `Advanced(T)` at the syscall-arm layer maps to `Done(T)` in v3. The `Continue` variant is never produced at this level; it's a step-function concern, not a translator concern.

Adding this rule to the worker brief eliminates ~85% of decision sites for the four shim workers (W-shims-fs, W-shims-io, W-shims-mod, plus tx-scripts to a lesser degree). This is a pure win.

### 9d. Hot-path inlining is concrete

W-page-backed identified the hot fns that need `#[inline]` on their `StepOp::step` impl in PR-2:

- [`crates/tx-subsystems/src/page_backed.rs:659`](../../../crates/tx-subsystems/src/page_backed.rs) — `step_range` (innermost loop)
- [`crates/tx-subsystems/src/page_backed/user_buffer.rs:108`](../../../crates/tx-subsystems/src/page_backed/user_buffer.rs) — `step_range_with_user_buffer`
- [`crates/tx-subsystems/src/page_backed.rs:414`](../../../crates/tx-subsystems/src/page_backed.rs) — `materialize_page` (dispatch hub)

Bench `page_backed` and `tmpfs` before/after PR-2 per doc 07 risk register.

### 9e. Revised wave plan for PR-1 step 2

Given §9b dependencies, reorder fan-out into two waves:

**Wave 1 (parallel, 6 workers):**
- W-vfs *(emits `AMBIGUOUS-IOCTL-N` markers; resolved in wave 2)*
- W-tty *(produces ioctl Advanced semantics decision note for vfs)*
- W-page-backed *(batched with W-fs; one of them edits the shared trait first)*
- W-fs *(batched with W-page-backed)*
- W-vm *(independent)*
- W-mount-pipe-futex *(independent)*

**Wave 2 (parallel, 5 workers, after wave 1 lands):**
- W-process-signal
- W-initramfs
- W-ext4
- W-shims-fs *(now has stable upstream)*
- W-shims-io
- W-shims-mod
- W-scripts
- W-kernel
- W-vfs follow-up commit *(resolve `AMBIGUOUS-IOCTL-N` against W-tty's decisions)*

Wave 1 gates the world; wave 2 is straightforward sweep.

### 9f. Verdict

Parallelization is **real and worth running**, with two coordination items baked into the brief: (1) wave 1 includes the W-fs/W-page-backed pairing, (2) wave 1's W-vfs leaves ioctl markers that wave 2 resolves. No surprise scope leaks. No partition is broken; two are coupled. Total elapsed time for PR-1 step 2 with two waves of fan-out: ~1.5 days wall time vs. the 5-day single-author estimate.

## 10. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Worker write-scope drift (one worker edits another's files) | Medium | Medium | Each worker's brief names its allowed prefix; main agent diffs against the brief on return |
| Mapping ambiguity at `Advanced(T) → Continue \| Done` boundary | High | Low | PR-template question: "did this site previously continue or terminate?" — workers flag uncertain sites for review rather than guessing |
| Inlining regression in `page_backed.rs` / `tmpfs.rs` hot paths | Low | Medium | Bench before/after; `#[inline]` small `step` impls |
| `vfs::walker::tests::step_walk_returns_eloop_after_41_hops` flake masking real regression | Medium | Low | Flake is documented in STATUS.md; investigate if any other test starts flaking in the same way |
| PR-1 conflicts with concurrent subsystem work elsewhere on the tree | High | Medium | Lock concurrent tx-subsystems / tx-shims / tx-fs / tx-scripts / tx-kernel edits for foundation week, per doc 07 §6 |
| Net-new test files diverge from final type shape | Medium | Low | Tests for net-new PRs reviewed against the doc shape before production code starts |

## 11. Next concrete action

PR-0 — write `crates/tx-substrate/tests/v3_algebra.rs`, the proptest, the lint extensions in `xtask/src/lint.rs`, the baseline doc, and the txdoc-tag harvest. Single author, one session, mergeable.

After PR-0 lands and the baseline is locked, fan out the PR-1 step 2 workers per §4.
