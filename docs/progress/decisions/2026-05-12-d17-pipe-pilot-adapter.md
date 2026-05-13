# D17 — Pipe pilot adapter (#[platform_adapter] migration)

Date: 2026-05-12
Status: landed (this branch)

## Why

D16 added `#[platform_adapter]` + `cargo xtask boundary-report` but no
subsystem had been migrated. The pilot validates the end-to-end pattern
on the smallest non-trivial multi-platform target — `tx-subsystems::pipe`
uses both `tx-substrate` (step_v3, zone, SpinMutex, wake) and `tx-reactor`
(wait::Channel/Mask), so it exercises stacked `#[platform_adapter]`
attributes on the same module.

## What changed

1. **Macro: per-platform manifest const namespace.** Updated
   `crates/tx-platform-adapter/src/lib.rs` to inject
   `__PLATFORM_ADAPTER_<PLATFORM>` instead of a single
   `__PLATFORM_ADAPTER`, so two `#[platform_adapter]` attributes can
   stack on one inline module (one per platform). The boundary-report
   scanner is unaffected — it greps the attribute literal, not the
   const name. Added expansion test
   `stacked_attributes_inject_per_platform_constants` covering the
   substrate + reactor stacked case (one of pipe's adapter modules
   uses exactly this shape).

2. **File restructure.** `crates/tx-subsystems/src/pipe.rs` →
   `crates/tx-subsystems/src/pipe/mod.rs` + new
   `crates/tx-subsystems/src/pipe/adapter.rs`. The split keeps the
   `#[cfg(test)]` block in `mod.rs` (where its substrate references
   remain `outside_adapter` for phase 7) and isolates the sanctioned
   adapter calls in `adapter.rs`.

3. **Two adapter modules** in `pipe/adapter.rs`:

   * `step_engine` — `#[platform_adapter(platform = "substrate",
     domain = "step_engine", apis = ["step_v3", "zone"], reason = "…")]`.
     Re-exports `step_v3` types (`StepOutcome`, `ByteProgress`,
     `NoProgress`, `ScriptCtx`, `StepOp`, `SubjectIdentity`),
     `zone::{Cap, Zone, ZoneAllocated, ZoneError}`, and `SpinMutex`,
     and exposes pipe-named verbs: `done_bytes`, `eagain`, `epipe`,
     `yield_until_readable`, `yield_until_writable`, `sign_zone_for`,
     `register_zone_for`.

   * `wait_routing` — TWO stacked attributes:
     `#[platform_adapter(platform = "substrate", domain =
     "wait_routing", …)]` and `#[platform_adapter(platform =
     "reactor", domain = "wait_routing", …)]`. Re-exports
     `WaitSource`, `Channel`, `Mask`. Exposes pipe-named verbs:
     `new_wait_source`, `fire_legacy_channel`, `notify_v3_source`.

4. **Production call sites migrated** in `pipe/mod.rs`: all 32
   substrate references and 1 reactor reference outside the
   `#[cfg(test)]` block now route through the adapter. Behavior is
   unchanged — wrappers are pure renames + small bundling
   (`zone::reserve_for` + `zone::sign_for` → `sign_zone_for`;
   `Channel.fire(Mask::from_bits(mask))` → `fire_legacy_channel`).

5. **Dependency added.** `tx-subsystems`'s `[dependencies]` gains
   `tx-platform-adapter = { path = "../tx-platform-adapter" }`.

## Boundary-report delta

```
                            before     after    delta
substrate outside (lines)    2547      2515      −32
substrate outside (files)     164       164        0   (pipe/mod.rs still has test refs)
substrate inside  (lines)       0        10      +10
substrate inside  (files)       0         1       +1
reactor   outside (lines)      72        71       −1
reactor   outside (files)      41        40       −1
reactor   inside  (lines)       0         3       +3
reactor   inside  (files)       0         1       +1
adapters declared               0         3       +3
```

The 53 substrate lines remaining in `pipe/mod.rs` are exclusively
inside the `#[cfg(test)]` block (32 production → 0; tests untouched
per the plan's phase 7 ordering). The 9 substrate lines in
`crates/tx-subsystems/tests/v3_pipe_waitsource.rs` are unchanged —
that's the integration test crate, also phase 7.

## Verification

- `cargo build -p tx-subsystems` — clean.
- `cargo test -p tx-subsystems --lib pipe::` — 34 / 34 pass.
- `cargo test -p tx-subsystems --test v3_pipe_waitsource` — 8 / 8 pass.
- `cargo test -p tx-subsystems --lib -- --test-threads=1` — 623 / 623
  pass single-threaded (the suite's documented mode; multi-threaded
  failures pre-date this PR).
- `cargo test -p tx-platform-adapter` — 11 unit + 4 expansion pass.
- `cargo xtask lint arch` — ok.
- `cargo xtask boundary-report` — produces the expected `step_engine`
  + `wait_routing` adapter declarations with the manifest strings
  intact.

## Pattern validated

The pipe pilot proves the following pattern, reusable for the rest
of the migration:

- A subsystem owning multiple platform deps can carve its adapter
  modules along **semantic domain**, not platform. `wait_routing`
  stacks two `#[platform_adapter]` attributes because the wakeup
  domain is genuinely cross-platform; splitting would fragment the
  unit.
- The role-typed wrappers (`done_bytes`, `eagain`, `yield_until_*`,
  `sign_zone_for`) read as pipe-side verbs rather than substrate
  primitives. Removing the substrate vocabulary from call sites is
  the visible payoff.
- Test substrate use stays in `mod.rs`'s `#[cfg(test)]` block where
  the boundary-report keeps flagging it. That preserves the signal
  for phase 7 (cross-cutting test migration).

## Next step

Replicate for the four mid-sized single-file subsystems in phase 1
of the refactor plan: `mount` (72), `futex` (61), `signal` (36),
`cred` (54). Each is single-file and can use the same `mod.rs` +
`adapter.rs` split. Estimated combined burn-down: ~220 substrate
lines outside → inside (substrate moves from 2515 → ~2295).
