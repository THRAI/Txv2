# D20 — Phase 3 adapter migration (tty family)

Date: 2026-05-12
Status: landed (this branch)

## Why

D19 closed phase 2 (process + vfs). Phase 3 is the largest single-
subsystem phase in the refactor plan: the tty family spans ~14
production files across `tty/execution/{register_hardware,
step_*}.rs`, `tty/structure/`, `tty/checks/`, and `tty/project.rs`.
Combined substrate fan-in was ~382 production lines.

## What changed

Same two-domain shape established in D17-D19, applied uniformly:

- `crates/tx-subsystems/src/tty/adapter.rs` declares `step_engine`
  (substrate: step_v3 types, zone role types, EBR Guard +
  guard(), bus primitives `RawPort` + `RawQueue`, AtomicSlot,
  SpinMutex, plus `sign_zone_for` and pass-through `reserve_for` /
  `sign_for`) and `wait_routing` (stacked substrate + reactor:
  WaitSource, Channel, Mask, plus the standard wakeup verbs).
- `tty/mod.rs` adds `pub mod adapter;`.
- All 14 production files migrated to consume the adapter.

## Workflow

This was the first phase to use **bulk `sed` substitution** for the
mechanical body-pattern replacements. The recurring patterns
(`tx_substrate::step_v3::StepOutcome` → `StepOutcome`,
`tx_substrate::step_v3::NoProgress` → `NoProgress`, etc.) are
identical across all 14 files, so a single sed pass over the
production glob handled the bulk of the work. Per-file `Edit` calls
then handled the import line shapes (which differ per file) and
fixed the few unique sites (`use ... as V3;` local aliases needed
their path restored, nested `mod step_op_wraps` test modules needed
`use super::super::{step_engine, ...}` to inherit the alias).

`sed -i …` only — GNU sed in PATH (homebrew). For macOS-default BSD
sed compatibility, the pattern would be `sed -i '' …`. The script
in `/tmp/tty_migrate.sh` is checked-in nowhere; it was a one-shot
tool. If the same migration shape repeats often enough, hoisting it
into `cargo xtask boundary migrate <subsystem>` becomes worth
doing.

## Boundary-report delta

```
                            baseline  after p2   after p3   cumulative Δ
substrate outside (lines)     2547      2257      1955       −592 (23%)
substrate outside (files)      164       160       159         −5
substrate inside  (lines)        0        40        48        +48
substrate inside  (files)        0         7         8         +8
reactor   outside (lines)       72        64        62        −10
reactor   outside (files)       41        36        34         −7
reactor   inside  (lines)        0         6         7         +7
reactor   inside  (files)        0         4         5         +5
adapters declared                0        15        18        +18
```

**38:1 outside-removed-to-inside-added ratio** on this phase — much
higher than phases 1 (5:1) and 2 (7:1). Reason: TTY's substrate
surface is overwhelmingly *types* (step engine outcome / progress /
context types repeated in 10 step_*.rs trait signatures and StepOp
impl headers), so the migration was almost pure re-export
substitution. Very few semantic verb wrappers needed beyond what
the earlier phases already established.

## Verification

- `cargo build -p tx-subsystems` — clean.
- `cargo test -p tx-subsystems --lib tty::` — 106 / 106 pass.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — full
  host suite 623 / 623 passing, 11 ignored. Matches D17-D19
  baseline; no regressions.
- `cargo xtask lint arch` — ok.
- `cargo xtask boundary-report` — shows 18 declared adapters; the
  three new ones are tty's step_engine (substrate) and the stacked
  wait_routing (substrate + reactor).

## Pattern observations

* **Bulk sed** is genuinely faster than per-file Edit when the
  pattern is uniform across many files. The risk is import-line
  fragility (each file's `use` shape differs slightly), so the
  sed pass needs to focus on body patterns only; imports get
  handled per-file. For TTY, that was 14 sed-uniform body files +
  14 per-file import edits.
* **`use Foo as Alias;` patterns are sed-hostile.** A naive
  `tx_substrate::step_v3::Foo` → `Foo` substitution corrupts
  `use tx_substrate::step_v3::Foo as Bar;` into `use Foo as Bar;`
  (Foo not in scope). The cleanup pass needed to detect those
  broken use-lines and rewrite them as
  `use crate::tty::adapter::step_engine::Foo as Bar;`.
* **Nested test modules** (`mod step_op_wraps` inside `mod tests`)
  need `use super::super::{step_engine, …};` to inherit the
  adapter alias from file scope. Caught by the test compilation
  pass after the lib compilation pass.

## Next step

Phase 4 from the refactor plan: memory subsystems `page_backed/`
and `vm/`. Combined ~350 substrate lines. The `vm/` migration also
needs to reconcile with the existing `lint arch` rule against raw
`Zone<T, Policy>` upper-layer (D10 §"Cross-cutting"): the boundary
report counted 161 substrate `zone` references but `lint arch` only
flagged the explicit `Zone<_, Policy>` form. Need a brief audit
during phase 4 to confirm the residual references are role-typed
not raw.

## Blocker

None.
