# D19 — Phase 2 adapter migration (process, vfs)

Date: 2026-05-12
Status: landed (this branch)

## Why

D18 closed phase 1 (single-file subsystems). Phase 2 from the refactor
plan handles the two multi-file core subsystems: `process/` and
`vfs/`. These exercise the pattern of a single `adapter.rs` consumed
by multiple sibling files in the same module.

## What changed

### Process

`crates/tx-subsystems/src/process/` already had `mod.rs`, so no `git
mv` needed — just added `adapter.rs` and wired `pub mod adapter;` at
the top of `mod.rs`.

Two adapter modules:

* `step_engine` — substrate. Re-exports `step_v3` types used by the
  seven `*Op` impls in `execution.rs` (ForkOp, WaitpidOp, ChdirOp,
  GetcwdOp, ExitGroupOp, SetpgidOp, SetsidOp). Re-exports zone role
  types (Cap, PayloadCap, Weak, IdentRef, Entity, Dead,
  ZoneAllocated, ZoneError, Zone, OperationalCapExt) used by
  ProcessIdentity / ProcessPayload / ProcessGroup / Session. Plus
  EBR `Guard` / `guard()`, the `RestrictionStackHandle`, the
  `AtomicSlot` and `SpinMutex` primitives, and a `sign_zone_for`
  verb.

* `wait_routing` — stacked substrate + reactor. The per-process
  exit-source path: `new_wait_source`, `fire_legacy_channel`,
  `notify_v3_source`. Same verbs as pipe / futex.

Sibling files migrated: `structure.rs`, `execution.rs`,
`exec_prep.rs`. Production substrate refs collapsed from 17 + 84 +
1 = 102 → all sanctioned through the adapter.

### VFS

Same shape — `crates/tx-subsystems/src/vfs/adapter.rs` with
`step_engine` (substrate) and `wait_routing` (stacked).

Sibling files migrated: `structure.rs`, `execution.rs`, `walker.rs`,
`checks.rs`. The vfs `FsOps` trait surface in `execution.rs` had
many trait signatures of the shape `-> StepOutcome<T,
NoProgress>` (one per backend method), which all collapsed cleanly
to the re-exported names.

### `fire_legacy_channel` signature upgrade

VFS structure has `fire_read_wait` / `fire_write_wait` methods that
return the wake count (passed through from `Channel::fire`). The
shared `fire_legacy_channel` adapter verb originally discarded the
count (pipe/futex/process didn't need it). Updated the verb to
return `usize` and pipe/futex/process versions to match — a tiny
signature improvement that future-proofs the shared verb.

## Boundary-report delta

```
                            baseline  after p1   after p2   cumulative Δ
substrate outside (lines)     2547      2420      2257       −290
substrate outside (files)      164       164       160         −4
substrate inside  (lines)        0        27        40        +40
substrate inside  (files)        0         5         7         +7
reactor   outside (lines)       72        70        64         −8
reactor   outside (files)       41        39        36         −5
reactor   inside  (lines)        0         4         6         +6
reactor   inside  (files)        0         2         4         +4
adapters declared                0         9        15        +15
```

7:1 outside-removed-to-inside-added ratio on this phase — better
than phase 1's 5:1. Vfs's `FsOps` trait signatures (one return type
per backend method) collapsed especially well.

## Verification

- `cargo build -p tx-subsystems` — clean.
- `cargo test -p tx-subsystems --lib process::` — 87 / 87 pass.
- `cargo test -p tx-subsystems --lib vfs::` — 36 / 36 pass (11
  ignored — pre-existing main-side zone-slot cascade flakes).
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — full
  host suite 623 / 623 passing, 11 ignored. Matches D17/D18 baseline.
- `cargo xtask lint arch` — ok.
- `cargo xtask boundary-report` — shows 15 adapter manifests.

## Pattern notes

* Multi-file subsystems use one `adapter.rs` consumed via
  `use crate::<subsystem>::adapter::step_engine::*` from sibling
  files. No need for `super::adapter::*` paths — the absolute path
  is more readable in cross-file imports.
* `crate::execution::Errno` (the subsystem-side wrapper) vs
  `step_v3::Errno` (substrate's) — when both appear, import the
  substrate one as `step_engine::Errno` and use it module-qualified,
  not via `pub use`. Walker.rs exercises this pattern.
* `Channel::fire` returns the wake count. The adapter verb should
  preserve that — not all callers ignore it. Caught when migrating
  vfs.

## Next step

Phase 3 from the refactor plan: TTY family (`tty/structure/`,
`tty/execution/step_*.rs` — 10 step_* files plus subdirs). ~440
substrate lines, all of similar shape. Largest single-subsystem
phase remaining. With pipe + process + vfs as exemplars, this is
mostly mechanical replication.

## Blocker

None.
