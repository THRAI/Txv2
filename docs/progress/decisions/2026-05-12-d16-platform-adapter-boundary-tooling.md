# D16 — Platform-adapter boundary tooling (xtask + proc-macro)

Date: 2026-05-12
Status: landed (this branch)

## Why

Substrate is used directly from 164 files / 2,547 lines across the
workspace (76% of that is `tx_substrate::step_v3::*`); reactor leaks
to 41 files / 72 lines. Every future substrate refactor today has to
chase those call sites across `tx-fs`, `tx-subsystems`, `tx-shims`,
and `tx-ext4`. The convergence plan is to route each upper module's
substrate calls through a single declared adapter module per
(subsystem × platform), so substrate refactors only have to follow
those declared paths.

Compiler can't seal an extern crate — once `tx-substrate` is a Cargo
dependency, any file can `use tx_substrate::…`. The seal has to come
from a measurable boundary plus a CI gate.

## What changed

1. **`cargo xtask boundary-report`** ([xtask/src/boundary_report.rs](../../../xtask/src/boundary_report.rs))
   walks `crates/**/*.rs` and produces an Architecture Boundary Report
   counting raw substrate/reactor lines, files, sub-API fan-in, and
   top per-file offenders. Recognises `#[platform_adapter(platform =
   "...", domain = "...", reason = "...")]` modules and routes their
   substrate/reactor lines into the `inside_adapter` bucket. Flags:
   `--top N` (default 10), `--json` (full per-file table).
2. **`tx-platform-adapter` proc-macro crate**
   ([crates/tx-platform-adapter/src/lib.rs](../../../crates/tx-platform-adapter/src/lib.rs))
   provides the attribute. Validates `platform ∈ {substrate, reactor}`,
   snake_case `domain`, `reason ≥ 12 chars`, optional snake_case
   `apis = [...]` whitelist. Injects a single `pub const
   __PLATFORM_ADAPTER: &str = "platform=…;domain=…;reason=…"` at the
   top of each annotated inline module so the manifest is real,
   queryable code (`cargo doc`, runtime introspection).
3. **Meta-crate exclusion** — `boundary-report` skips
   `crates/tx-platform-adapter/` so the macro's own test fixtures (which
   contain literal `#[platform_adapter(…)]` for end-to-end expansion
   checks) don't pollute workspace adapter counts.

## Verification

- `cargo test -p tx-platform-adapter` — 11 unit tests + 3 expansion
  integration tests pass.
- `cargo test -p xtask --lib boundary_report::` — 8 scanner / accumulator
  tests pass.
- `cargo build --workspace` (host members) — clean.
- `cargo xtask lint arch` — ok (no regressions).
- `cargo xtask boundary-report` — produces the expected baseline:

  ```
  Raw substrate calls outside adapters:  2547 lines / 164 files
  Raw substrate calls inside  adapters:     0 lines /   0 files
  Raw reactor   calls outside adapters:    72 lines /  41 files
  Platform adapters declared:               0
  ```

  Top substrate sub-APIs: `step_v3` 1948, `epoch` 560, `zone` 161,
  `testing` 87, `page_allocator` 75, `wake` 66.

## Next step

Begin substrate-adapter migration starting with the largest single
sub-API (`step_v3`, ~76% of substrate fan-in). The natural per-
subsystem decomposition:

- `tx-subsystems::vfs::step_adapter`        (vfs/execution.rs, walker.rs)
- `tx-subsystems::tty::step_adapter`        (tty/execution/step_*.rs)
- `tx-subsystems::process::step_adapter`    (process/execution.rs, structure.rs)
- `tx-subsystems::page_backed::step_adapter` (page_backed/*.rs)
- `tx-subsystems::pipe::step_adapter`       (pipe.rs)
- `tx-subsystems::mount::step_adapter`      (mount.rs)
- `tx-subsystems::futex::step_adapter`      (futex.rs)
- `tx-subsystems::signal::step_adapter`     (signal.rs, signalfd.rs)
- `tx-subsystems::cred::step_adapter`       (cred.rs)
- `tx-fs::tmpfs::step_adapter`              (tmpfs.rs)
- `tx-fs::devfs::step_adapter`              (devfs.rs)

The boundary-report's `inside_adapter` number is the burn-up; the
`outside_adapter` number is the burn-down ratchet.

## Blocker

None. The macro and report are independent infrastructure; no
production code paths changed. The actual migration is the follow-on
work that the report now measures.
