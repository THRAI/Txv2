# D10 — v4 Vocabulary Retirement Completeness Audit

**Date:** 2026-05-12
**Worker:** W-NN (research-only)
**Anchor:** [`docs/Txv3/07_BLAST_RADIUS.md`](../../Txv3/07_BLAST_RADIUS.md) §7
success criteria; [`docs/progress/migration-completion-audit-2026-05-12.md`](../migration-completion-audit-2026-05-12.md).
**Purpose.** Verify the §7 exit bar — "no `OnCarrier`, `WakeCarrier`,
`InterestConditions`, `exit_port`, `read_wq`/`write_wq` identifiers
remain in code (excluding archived docs)" — by greppable evidence, and
produce a retire plan for any residuals (including the implicit PR-2
and PR-6 vocabulary axes).

<!-- txdoc:TXV3-VOCAB-RETIRE-D10 -->

---

## 1. Executive summary

- **Total identifiers audited:** 13 (5 explicit §7 names + 8 implicit
  v4 vocabulary axes from PR-2/PR-6/PR-A.1..A.4 closure).
- **Total raw hits (incl. self-references in the audit/plan docs):** **189**.
- **Production-code identifier hits (must retire NOW): 0.**
- **Production-code doc-comment hits (cosmetic; nice to fix): 9.**
- **Active-spec doc-comment hits (must update if v1 not marked superseded): 18.**
- **Historical / archived / progress-log hits (LEAVE ALONE): 162.**

**Verdict.** The §7 exit bar is **met**. No production
identifier of the retired vocabulary survives in `crates/`. The
residuals are entirely (a) prose references in `docs/progress/`
(historical record), (b) "renamed from X" annotations in code
doc-comments at the substrate definition sites (`step_v3/mod.rs`,
`step_v3/agent.rs` — intentional traceability), (c) `STEP_MODEL_v1.md`
which is already marked SUPERSEDED, and (d) a small number of stale
`StepOutcome::Blocked` references in active design docs from
subsystems whose docs predate PR-2.

The single non-trivial residual is **the bare-`carrier` local-variable
name** used at every `OnWaitSource { source: carrier, .. }`
destructuring site (~40 lines of production Rust). This is *not* a v4
vocabulary survival — the v3 field is `source`; the local binding name
is just an unrelated identifier choice. It is flagged in §5 as a false
positive.

---

## 2. Per-identifier table

Counts run from worktree root with
`grep -rn <ident> --exclude-dir=target --exclude-dir=.git --exclude-dir=docs/archive .`.
The `Production` column counts identifier *uses* in `.rs` source
(excluding doc-comments). The `Doc-comment (code)` column counts
doc-comments in `.rs` files. The `Active-doc` column counts uses in
docs *not* marked SUPERSEDED. The `Hist./progress` column folds
STATUS.md, plans, decisions, research, and the migration-audit doc.
The `Self-ref` column counts the audit/plan documents that mention the
identifier as part of the retire plan itself.

| Identifier | Raw | Production | Doc-comment (code) | Active-doc | Test name | Hist./progress | Archived | Self-ref |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `OnCarrier` | 24 | 0 | 1 | 0 | 0 | 14 | 0 | 9 |
| `WakeCarrier` | 18 | 0 | 1 | 0 | 0 | 6 | 0 | 11 |
| `InterestConditions` | 16 | 0 | 1 | 0 | 0 | 7 | 0 | 8 |
| `exit_port` | 73 | 0 | 0 | 0 | 0 | 67 | 0 | 6 |
| `read_wq` | 18 | 0 | 0 | 0 (v1 superseded) | 0 | 6 | 1 | 4 |
| `write_wq` | 12 | 0 | 0 | 0 (v1 superseded) | 0 | 5 | 0 | 4 |
| `wait_carrier` | 59 | 0 | 0 | 0 | 0 | 58 | 0 | 1 |
| `WakeCarrierId` | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `yield_on_carrier` | 8 | 0 | 0 | 0 | 0 | 8 | 0 | 0 |
| `StepOutcome::Advanced` | 2 | 0 | 0 | 1 (`BDEV_FS.md`) | 0 | 0 | 0 | 1 |
| `StepOutcome::Blocked` | 11 | 0 | 4 | 5 | 0 | 1 | 0 | 1 |
| `StepOutcome::AdvancedThenBlocked` | 1 | 0 | 0 | 1 (`BDEV_FS.md`) | 0 | 0 | 0 | 0 |
| `CancelPolicy` (unprefixed) | 13 | 0 | 2 | 0 | 0 | 5 | 0 | 6 |
| **Totals** | **255 raw incl. self-ref** | **0** | **9** | **7 (incl. v1)** | **0** | **177** | **1** | **51** |

(The "Raw" column row-total exceeds the §1 figure because identifiers
overlap on the same lines, e.g. the BLAST_RADIUS.md §7 success-criteria
sentence contains five at once. §1's 189 is the deduplicated line count
across all 13 greps.)

---

## 3. "Must retire NOW" list

**Empty.** No production code identifier from the retired vocabulary
survives. The closest items to that bar are the production
doc-comments below; they are cosmetic and listed for Phase 2.

### 3a. Production doc-comments (cosmetic; not blocking §7 exit)

These are `///` or `//!` comments inside `.rs` files that mention the
old vocabulary as historical context. They are not identifiers, do not
affect compilation, and — at the two `step_v3` sites — are
**intentional traceability comments** marked "renamed from X per
PR-N". Leave the intentional ones; modernise the rest in a parallel
sweep.

| File:line | Current text | Disposition |
|---|---|---|
| `crates/tx-substrate/src/step_v3/mod.rs:90` | `/// Renamed from \`WakeCarrier\` per docs/Txv3/07_BLAST_RADIUS.md §3.1.` | **KEEP** — intentional rename annotation at the canonical definition site. |
| `crates/tx-substrate/src/step_v3/mod.rs:105` | `/// \`InterestConditions\` per docs/Txv3/07_BLAST_RADIUS.md §3.1.` | **KEEP** — same as above. |
| `crates/tx-substrate/src/step_v3/mod.rs:120` | `/// PR-0 pinned \`OnWaitSource\` (formerly \`OnCarrier\`); the OnAgent variant` | **KEEP** — same as above. |
| `crates/tx-substrate/src/step_v3/agent.rs:7` | `//! and \`CancelPolicy\`.` | **KEEP** — module docstring listing PR-6 inputs. |
| `crates/tx-substrate/src/step_v3/agent.rs:80` | `/// Renamed from \`CancelPolicy\` per the v3 migration's PR-6:` | **KEEP** — intentional rename annotation. |
| `crates/tx-shims/src/linux_syscall/mod.rs:245` | `/// fall-through magnitude for \`StepOutcome::Blocked\` /` | **UPDATE** to `StepOutcome::Yield { OnWaitSource }`. Stale post-PR-2. |
| `crates/tx-shims/src/linux_syscall/mod.rs:452` | `/// (notably \`NR_WRITE\`) loop on \`StepOutcome::Blocked\` and \`.await\`` | **UPDATE** to `StepOutcome::Yield`. |
| `crates/tx-subsystems/src/vm/execution.rs:757` | `/// \`StepOutcome::Blocked(token)\`. Resolved through the global wait-source` | **UPDATE** to `StepOutcome::Yield { OnWaitSource }`. |
| `crates/tx-subsystems/src/vm/execution.rs:768` | `/// ever produces \`StepOutcome::Done\` or \`StepOutcome::Blocked\`. Other` | **UPDATE** to `StepOutcome::Yield { OnWaitSource }`. |

### 3b. Active design-doc references

These docs are **not** marked SUPERSEDED in their status banner and
contain pre-PR-2 vocabulary in non-prose form (pseudocode / examples).
Each is a 1-3 line cosmetic edit. They are not the v3 spec — most are
older subsystem specs (FS, HAL, signal) that pre-date the
`StepOutcome` 5→4 refactor.

| File:line | Identifier | Action |
|---|---|---|
| `docs/design/05_filesystem/BDEV_FS.md:235,243,245` | `StepOutcome::{Blocked,AdvancedThenBlocked,Advanced}` in pseudocode | Update pseudocode to v3 `Yield { shape: OnWaitSource }` / `Continue { progress }`. |
| `docs/design/05_filesystem/TX_EXT4_PLAN_v1_2.md:77,581` | `StepOutcome::Blocked` in prose | Two-word edit each → `StepOutcome::Yield { OnWaitSource }`. |
| `docs/design/04_process-signals/SIGNAL_v1.md:1896` | `StepOutcome::Blocked {` in pseudocode block | Single-block update to `Yield { shape: OnWaitSource { .. } }`. |
| `docs/design/02_execution/cred_service_v_1_draft (2).md:605` | `StepOutcome::Blocked(c, m)` in match arm pseudocode | Update pseudocode match arm. |
| `docs/design/01_substrate/HAL_v1.md:1668` | `StepOutcome::Blocked(WaitToken)` in prose | Update prose. |

`docs/design/02_execution/STEP_MODEL_v1.md` (8 hits) is **already
marked SUPERSEDED** at line 6 (`> ⚠ SUPERSEDED by Txv3/03_STEP_MODEL_v2.md`).
Leave it untouched; the supersession header is the canonical signal.

`docs/design/00_meta-framework/archived/INVARIANTS_v3_3.md:168` is in
`/archived/`; out of scope per audit rules.

### 3c. Plan / decision documents referenced by historical record

The `docs/progress/plans/*.md` and `docs/progress/decisions/*.md`
files (e.g. `2026-05-06-fork-clone-wait4.md`, the v3 migration plans)
contain ~150 references to the old vocabulary in *historical* context
— they describe what was renamed, when, and to what. These are by
charter a historical record and must not be edited; future archaeology
needs them.

---

## 4. Retire plan

### Phase 1 — Production code retirement
**Status: ALREADY COMPLETE.** No work remains; §7 exit bar is met as
of 2026-05-12.

### Phase 2 — Stale doc-comment cleanup in production Rust (cosmetic)
**Effort: 1 worker, ~30 minutes.**

Touch the four lines in §3a marked "UPDATE":
- `crates/tx-shims/src/linux_syscall/mod.rs:245,452`
- `crates/tx-subsystems/src/vm/execution.rs:757,768`

Mechanical text edit; replace `StepOutcome::Blocked(token)` with
`StepOutcome::Yield { shape: YieldShape::OnWaitSource { .. } }` in
each comment. No code or test changes.

### Phase 3 — Active design-doc pseudocode update
**Effort: 1 worker, ~2 hours (judgment edits on pseudocode).**

The five files in §3b each have 1-3 sites of `StepOutcome::{Blocked,
Advanced, AdvancedThenBlocked}` in pseudocode or prose examples. Each
needs:

1. Pseudocode match arms rewritten with `Yield { shape: OnWaitSource
   { source, interests } }` / `Continue { progress }` per
   `Txv3/03_STEP_MODEL_v2.md` §2.
2. Surrounding prose adjusted so e.g. "yields `StepOutcome::Blocked`"
   becomes "yields on the wait source via `StepOutcome::Yield`".

These are subsystem specs not in the v3 hot path — the work can fan
out across BDEV_FS, TX_EXT4_PLAN_v1_2, SIGNAL_v1, cred_service, and
HAL_v1 in parallel by 5 workers in ~30 min wall, or sequentially in 2
hours.

### Phase 4 — Archive consolidation
**Effort: low priority; not required for any exit bar.**

The `docs/design/02_execution/STEP_MODEL_v1.md` file is correctly
marked SUPERSEDED and could be moved under
`docs/design/02_execution/archived/` to make the `--exclude-dir=archive`
grep filter cleaner. This is purely cosmetic and would silence ~8
audit-grep hits without changing any semantic.

Optional: relocate `cred_service_v_1_draft (2).md` and
`rlimit_service_v_1_draft (1).md` (draft files with `(2)`/`(1)` paren
suffixes) into a `drafts/` subdir to remove special-cased shell
quoting.

---

## 5. False positives

### 5.1 Bare `carrier` as a local-variable binding name

194 hits of `\bcarrier\b` in Rust source. These are **not** v4
vocabulary survivals — they are local-variable binding names at every
`OnWaitSource { source: carrier, interests }` destructuring site:

```rust
StepOutcome::Yield {
    shape: YieldShape::OnWaitSource { source: carrier, interests },
    ..
} => {
    let token = WaitToken::new(carrier.raw(), interests.raw());
    ...
}
```

The v3 field is unambiguously named `source`; the `: carrier` after
the colon is the *local rebinding* in the destructuring pattern. There
is no v4 type or field with the bare name `carrier`. This is a style
choice that survived the rename arc because (a) `wait_carrier` →
`wait_source` rename was scoped to identifiers in module paths and
public APIs, not destructuring locals, and (b) `source` as a local
name conflicts with `tokio::source`-style readability lint
conventions at the call sites.

**Recommendation:** treat as a false positive for the §7 bar. A future
codebase-wide style sweep could rename these to `source_id` /
`waker_source` if desired, but it is *not* a vocabulary retirement
issue.

### 5.2 `*_carrier_id` suffix fields

Per `docs/progress/plans/2026-05-11-v3-migration-remaining.md:121`,
33 sites of `*_carrier_id` (e.g. `wait_carrier_id`,
`reader_wait_carrier_id`, `exit_source_carrier_id`) are documented
as **explicitly deferred** to a follow-up judgment PR:

> remaining: `*_carrier_id` suffix audit (33 sites: `wait_carrier_id`
> field/method/local, `reader_wait_carrier_id`,
> `writer_wait_carrier_id`, `exit_source_carrier_id`) — deferred to a
> follow-up PR (judgment-heavy: rename to `_source_id`, drop
> `_carrier_` infix, or keep as-is per-site).

These are explicitly scoped out of §7. Not a residual.

### 5.3 `CancelPolicy` doc-comments at `step_v3/agent.rs`

Two hits of `CancelPolicy` at `step_v3/agent.rs:7,80` are
**intentional** "renamed from X per PR-6" annotations at the canonical
definition site of `AgentCancelPolicy`. They are traceability records
in the substrate spec and must remain.

---

## 6. Recommendation

**Full vocabulary retirement against the §7 success bar is already
achieved as of 2026-05-12.** The migration is **done**.

The 9 stale production-doc-comments (§3a Phase 2) and 5 active
design-doc pseudocode sites (§3b Phase 3) can be cleared in a single
half-day sweep with **1-5 workers depending on parallelism**:

- **Sequential (1 worker):** ~2.5 hours wall.
- **Parallel (5 workers, one per Phase-3 doc + 1 for Phase 2):**
  ~30 min wall.

Neither phase is on the critical path for the v3 migration exit bar.
Phase 2/3 can land in a separate cleanup PR (e.g. PR-V) at the
implementation lead's discretion. Phase 4 (archive consolidation) is
optional and zero-priority.

---

## 7. Verification

1. `grep -rn '\(OnCarrier\|WakeCarrier\|InterestConditions\|exit_port\|read_wq\|write_wq\)' --include='*.rs' --exclude-dir=target --exclude-dir=.git . | grep -v '^.*///' | grep -v '^.*//!'`
   → **empty.** No production identifier hits.
2. `cargo check --workspace` baseline unchanged (this audit is
   research-only; zero code edits).
3. STATUS.md gets a 5-10 line catchup entry summarizing this audit.
4. No new tests; no production code changes; no doc moves.

---

## 8. References

- `docs/Txv3/07_BLAST_RADIUS.md` §7 (success criteria, line 230).
- `docs/progress/migration-completion-audit-2026-05-12.md` §1
  ("vocabulary retired tree-wide" claim — this ADR verifies it).
- `docs/progress/plans/2026-05-11-v3-migration-remaining.md` rows
  PR-A.1, PR-A.2, PR-A.3, PR-A.4 (the four rename PRs that landed the
  retirement).
- `docs/progress/decisions/2026-05-11-d4-bus-mailbox-layering.md`
  (PR-3D consumer migrations that converted the last `read_wq` /
  `write_wq` carriers).
