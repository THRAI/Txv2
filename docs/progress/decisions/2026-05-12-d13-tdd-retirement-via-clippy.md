# Decision D13: Test-driven retirement framework via clippy disallowed-names

**Date:** 2026-05-12
**Status:** decided
**Worker:** W-QQ (design + landing)
**Anchors:**
- [D10 vocabulary-retire audit](2026-05-12-d10-vocabulary-retire-audit.md) — the target identifier list.
- [D12 dead-code + TODO audit](2026-05-12-d12-dead-code-todo-audit.md) — explains why a blanket `-D warnings` gate is currently inappropriate.
- [`docs/Txv3/07_BLAST_RADIUS.md`](../../Txv3/07_BLAST_RADIUS.md) §7 — the migration-exit success criteria the gate protects.

<!-- txdoc:TXV3-RETIRED-VOCAB-GATE-D13 -->

---

## 1. Problem statement

W-NN's D10 audit verified that the v4 vocabulary
(`OnCarrier`, `WakeCarrier`, `WakeCarrierId`, `InterestConditions`,
`exit_port`, `read_wq`, `write_wq`, `wait_carrier`, `yield_on_carrier`)
has been **fully retired** from `crates/` — zero production-code
identifier survivals. But the migration is currently protected only by
(a) the grep-sweep in D10, and (b) code review vigilance. Nothing in CI
fails fast if a future worker re-introduces a retired name.

The gap: a regression — accidental `pub struct OnCarrier;` in a fresh
PR, or a `let exit_port = ...` rebinding — would land green, and the
v4-era vocabulary would silently start re-accreting. D10's success is
not durable without a test.

## 2. Decision

Land a **clippy-based regression gate**:

1. A workspace-root `clippy.toml` enumerates the retired identifiers
   under `disallowed-names`.
2. A new `xtask ci` step (`txdoc:CI-GATE-RETIRED-VOCAB`) runs clippy
   with **only the disallowed-* lints promoted to `-D`** (everything
   else demoted via `-A clippy::all`), so unrelated cosmetic warnings
   (per D12: 27 PR-2 `dead_code` scaffolding warnings, hex-literal
   style, etc.) do not gate the regression check.
3. The existing `txdoc:CI-GATE-CLIPPY` step is **untouched**. It
   continues to run `-D warnings` for the day when D12 Phase B
   decorates the scaffolding wraps with `#[allow(dead_code)]`.

The gate lives in `xtask`, which `.github/workflows/check.yml` already
invokes via `cargo xtask ci` — no workflow edit required.

## 3. Why clippy, and why these three lints

Clippy 1.55+ ships three configurable lints suitable for vocabulary
bans:

| Lint | Fires on | Use for retired vocab |
|---|---|---|
| `disallowed_names` | local-variable bindings, function params, destructuring pattern bindings (config: `disallowed-names`) | **Primary catch.** Fields like `exit_port` destructure into bindings (`let Process { exit_port, .. }`); locals like `read_wq` are caught at their `let`. |
| `disallowed_types` | usage of a type by its fully qualified path (config: `disallowed-types`) | Empty today — the retired types no longer exist in any reachable crate, so no path can be banned. Left as a hook. |
| `disallowed_methods` | calls to a fully qualified method path (config: `disallowed-methods`) | Empty today — retired variants like `StepOutcome::Blocked` were deleted with the parent enum in PR-2. The path doesn't exist; structural deletion is the test. |

The `disallowed_names` lint has one important non-coverage area
explored empirically below in §4: it does **not** fire on struct, enum,
field, or function *definition* sites. It fires on *binding* sites.
The asymmetry is annoying but the realistic regression mode — someone
re-introduces a retired name as a local or a destructured field — is
covered.

## 4. Empirical verification matrix

Run during D13 landing (against the worktree state at HEAD):

| Probe | Expected | Observed |
|---|---|---|
| `cargo clippy ... -D clippy::disallowed_names` on clean worktree | clean | **clean** (zero fires) |
| `let exit_port = 0u32;` in a scratch `.rs` | fires | **fires** (`error: use of a disallowed/placeholder name 'exit_port'`) |
| `let read_wq = 0u32;` in a scratch `.rs` | fires | **fires** |
| `let OnCarrier = 1u32;` in a scratch `.rs` | fires (modulo `non_snake_case` noise) | **fires** |
| `pub struct OnCarrier { x: u32 }` definition only | does not fire | **does not fire** (limitation documented) |
| `pub fn exit_port() -> u32 { 0 }` definition only | does not fire | **does not fire** (limitation documented) |
| `let Source { source: carrier, .. } = s;` (D10 §5.1 false positive — 40 sites) | does not fire | **does not fire** (intentional — `carrier` is **not** listed in `disallowed-names`) |
| `xtask ci` step `retired vocabulary gate` runs cleanly | passes | **passes** |

The struct/fn definition gap is acceptable because:

1. A regression that adds `struct OnCarrier;` is almost always
   accompanied by use sites (`let oncarrier = OnCarrier { .. }` or
   destructuring), and those use sites fire the gate.
2. Code review and the D10 grep sweep remain the primary regression
   filter. The clippy gate is a *fast-fail second line of defense*, not
   the only defense.

## 5. Why not the existing `-D warnings` step

`xtask ci`'s long-standing clippy step runs
`cargo clippy ... -- -D warnings` against the workspace minus the
three board crates. Per D12, the workspace currently emits:

- 27 `dead_code` warnings on intentional PR-2 `StepOp` scaffolding wraps in
  `tx-subsystems/{page_backed,tty/execution}/` awaiting caller migration.
- 1 `unused_imports` (`WaitSourceId` in `tx-subsystems/src/process/structure.rs:34`).
- A handful of style lints (`unusual_byte_groupings`,
  `doc_lazy_continuation`).

Promoting `disallowed_names` under that umbrella means the gate
inherits all of the above as blockers. The D12 plan ("Option A: add
`#[allow(dead_code)]` to PR-2 scaffolding wraps") is the natural way
to make the umbrella gate pass, but per D12 §Recommendation it is
deliberately deferred ("decorating the PR-2 wraps now risks confusing
the next caller-migration worker").

So D13 lands the **narrow** gate first, per the brief's Option B
recommendation. D12 Phase B remains the natural follow-up that, when
applied, would let the broad `-D warnings` step gate retired
vocabulary as a side effect — at which point D13's separate step
becomes redundant and can be folded back. That fold is **not** part of
D13's scope.

## 6. clippy.toml content (canonical)

```toml
# clippy.toml — retired-vocabulary regression gate
disallowed-names = [
    "OnCarrier", "WakeCarrier", "WakeCarrierId", "InterestConditions",
    "exit_port", "read_wq", "write_wq", "wait_carrier",
    "yield_on_carrier",
]
disallowed-types = []   # left as a hook; nothing path-bannable today
disallowed-methods = [] # PR-2 retired StepOutcome variants — paths gone
```

Notable **omissions** (explained in clippy.toml's prose header):

- **No bare `"carrier"`**. D10 §5.1 documents ~40 sites of
  `OnWaitSource { source: carrier, .. }` destructuring that are
  intentional local-binding style — banning the bare name would create
  ~40 false positives.
- **No `"CancelPolicy"`**. The PR-6 retired identifier was prefixed
  to `AgentCancelPolicy`. The bare name still appears in two
  intentional doc-comments at `step_v3/agent.rs` (D10 §5.3), and
  `disallowed-names` doesn't scan doc-comments anyway.
- **No bare `"step_outcome"` or variant strings**. Variants of removed
  enums are structurally inaccessible; the deletion of the parent
  enum is the test.

## 7. xtask `ci` step (canonical YAML-free recipe)

The gate is wired through `xtask` rather than through workflow YAML so
that local `cargo xtask ci` runs the same command as CI. The new step
inserted after the existing `clippy` step:

```rust
ci_run(
    root,
    "retired vocabulary gate",
    "cargo",
    &[
        "clippy", "--no-deps", "--workspace", "--lib", "--bins",
        "--",
        "-A", "clippy::all",
        "-D", "clippy::disallowed_names",
        "-D", "clippy::disallowed_types",
        "-D", "clippy::disallowed_methods",
    ],
    "txdoc:CI-GATE-RETIRED-VOCAB",
),
```

`--lib --bins` (rather than `--all-targets`) avoids the integration-
test compilation surfaces where `panic_impl` conflicts on `no_std` board
crates when built as tests. The retired-vocabulary regression risk is
in the production / library sources, so this scope is precise.

Existing `.github/workflows/check.yml` invokes `cargo xtask ci` and
therefore picks up the new step automatically — **no workflow edit
required, no destructive step modified**.

## 8. Sanity-test recipe

Run on demand to confirm the gate fires on a re-introduction:

```bash
# 1. Baseline: confirms the gate currently passes
cargo clippy --workspace --lib --bins -- \
  -A clippy::all \
  -D clippy::disallowed_names \
  -D clippy::disallowed_types \
  -D clippy::disallowed_methods

# 2. Create a temporary regression
cat > crates/tx-substrate/src/scratch_regression.rs <<'RUST'
pub fn provoke() {
    let exit_port = 0u32;
    let _ = exit_port;
}
RUST
echo 'pub mod scratch_regression;' >> crates/tx-substrate/src/lib.rs

# 3. Confirm the gate fires
cargo clippy --workspace --lib --bins -- \
  -A clippy::all -D clippy::disallowed_names
# Expected:
# error: use of a disallowed/placeholder name `exit_port`
#  --> crates/tx-substrate/src/scratch_regression.rs:3:9
#   |
# 3 |     let exit_port = 0u32;
#   |         ^^^^^^^^^

# 4. Revert
rm crates/tx-substrate/src/scratch_regression.rs
sed -i '' '/pub mod scratch_regression;/d' crates/tx-substrate/src/lib.rs
```

This recipe was executed during D13 landing; outcomes are tabulated in
§4 above.

## 9. Known limitations

1. **Definition-site blind spot.** `disallowed-names` fires on bindings,
   not on `struct X;` / `enum X;` / `fn x()` definitions. A motivated
   regression that defines and never destructures a banned name slips
   through. Mitigation: code review + a future structural lint (e.g.
   a custom xtask grep step) if regressions appear.
2. **Doc-comment blindness.** Identifier appearances in `///` or `//!`
   are not scanned. Per D10 the surviving doc-comments at `step_v3/`
   are intentional traceability anchors; this is a feature, not a bug.
3. **External-crate re-exports.** If a future external dependency
   exports a type literally named `OnCarrier`, `use foo::OnCarrier`
   compiles fine because `disallowed-names` does not match imports.
   `disallowed-types` (with a fully-qualified path) is the right tool
   for that scenario; we'd add the path then.
4. **CancelPolicy not enforced.** The unprefixed `CancelPolicy` name
   appears in two intentional doc-comments and is structurally
   inaccessible elsewhere (the type is `AgentCancelPolicy`). Adding
   `"CancelPolicy"` to disallowed-names would not help (doc-comments
   are unscanned) and could create binding false positives in unrelated
   contexts; skip.
5. **`*_carrier_id` suffix audit (33 sites)** is explicitly deferred per
   D10 §5.2 to a follow-up judgment PR. Not in scope of D13's gate.

## 10. Follow-ups

- **D12 Phase B applied → fold this step into the broad `-D warnings`
  step.** When the 27 PR-2 wraps gain `#[allow(dead_code)]` and the
  workspace becomes warning-clean, `txdoc:CI-GATE-CLIPPY` will catch
  retired vocabulary as a side effect of `-D warnings`. At that point
  the dedicated `txdoc:CI-GATE-RETIRED-VOCAB` step is redundant and
  can be retired in favour of clippy.toml living on as the
  declarative ban list.
- **Optional D14:** add a custom xtask grep step that catches
  *definition-site* re-introductions (the §9.1 blind spot). One-line
  `rg -n 'pub (struct|enum|fn|type) (OnCarrier|...)' crates/` style.
  Cheap insurance; not required for D13 to ship.
- **`*_carrier_id` follow-up PR** per D10 §5.2 — out of scope here.

## 11. Verification log (executed at landing)

1. `cargo clippy --workspace --lib --bins -- -A clippy::all -D clippy::disallowed_names -D clippy::disallowed_types -D clippy::disallowed_methods` → **clean** against HEAD.
2. Scratch regression recipe (§8) executed against `crates/tx-substrate/src/`. `let exit_port`, `let read_wq`, `let OnCarrier` each produced the expected `error: use of a disallowed/placeholder name`.
3. Scratch destructure (`let Source { source: carrier, .. } = s;`) → **does not fire**, confirming D10 §5.1's 40 sites stay green.
4. `cargo build -p xtask` after adding the new step → **clean**.

## 12. References

- D10 — vocabulary retirement audit (target list, false positives).
- D12 — dead-code + TODO audit (why broad `-D warnings` is currently incompatible).
- D11 — D2-coexistence retire plan (the migration this gate protects).
- D4 — bus / mailbox layering (precedent for ADR style with refresh plan).
- `docs/Txv3/07_BLAST_RADIUS.md` §7 — the success bar the gate makes durable.
- Clippy reference: `disallowed_names`, `disallowed_types`, `disallowed_methods` lints, available since Rust 1.55.
