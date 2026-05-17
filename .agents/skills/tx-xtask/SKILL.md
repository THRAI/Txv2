---
name: tx-xtask
description: Use when running, building, imaging, decoding traps, linting, or otherwise driving the workspace through `cargo xtask`. Authoritative reference for every subcommand surfaced by `xtask --help` plus the prepare/run/diagnose chains they compose into.
---

# tx-xtask

`cargo xtask` is the single entry point for every workspace workflow that
isn't a plain `cargo` build. The dispatch table lives in
[`xtask/src/lib.rs`](../../../xtask/src/lib.rs) — when this skill and the
binary disagree, the binary wins.

## Quick pick

| You want to… | Run |
|---|---|
| Build + run host unit tests (fast feedback) | `cargo -q xtask unit` |
| Build kernel ELF + initramfs/disk for a target | `cargo xtask full-build [--target TARGET]` |
| Boot the kernel in QEMU | `cargo xtask qemu --target rv64-qemu --profile smoke` |
| Boot, run a profile, assert sentinel | `cargo xtask test smoke` |
| Boot BusyBox userland | `cargo xtask test busybox-boot` |
| Decode a trap from serial log or scause/sepc/stval | `cargo xtask fault-decode --target rv64-qemu --serial PATH` |
| Decode a syscall trace from serial log | `cargo xtask trap-trace --serial PATH --syscalls` |
| Run guest shell scenarios | `cargo xtask shell-test --target rv64-qemu --script PATH` |
| Validate progress JSON records | `cargo xtask progress validate` |
| Run an architecture / docs / boundary lint | `cargo xtask lint arch\|docs\|unused\|boundary\|invariants` |

## Command chains

The three most common chains layer on each other:

```
doctor              env check (toolchain, qemu, targets)
   ↓
full-build          doctor + kernel build + image
   ↓
qemu                full-build + boot
   ↓
test                full-build + qemu + sentinel assertion
```

`cargo xtask test [smoke|busybox-boot]` chains all three; reach for it
unless you specifically want the intermediate artifact.

## Fast-feedback (host)

### `cargo -q xtask unit`

Build + host unit tests for `tx-shims`, `tx-kernel`, `tx-ext4`,
`tx-scripts` (see [`xtask/src/unit.rs`](../../../xtask/src/unit.rs)).
One line per step on pass; failures print only the failing test name,
panic message, and failure list. **Default first move after any code
edit.**

### `cargo xtask check`

`cargo fmt --check` + `cargo clippy --workspace --all-targets`. Slower
than `unit`; run before pushing.

### `cargo xtask ci` / `cargo xtask ci-slow`

Local mirror of the CI gates. `ci-slow` adds the long-running gates.
Run before declaring a phase complete.

### `cargo xtask lint <kind> [rule|all]`

Architecture/discipline linters. Kinds:

- `arch` — module-graph constraints.
- `docs` — link checker + stale-vocabulary scan over `docs/`.
- `unused` — unused-symbol detector.
- `boundary` — substrate/reactor-call ratchet (post-adapter enforcement;
  fails if outside-adapter call count regresses).
- `invariants [rule|all]` — STEP-4 / WIT / SIG / SCRIPT / SUBJ / CHECKS
  discipline lints. Rule names: `step`, `step-discipline`,
  `step-v4-vocabulary`, `step-no-await`, `step-sync-signature`,
  `subject-context`, `witness-scope`, `signal-publish`,
  `script-boundary`, `checks-purity`, `step-guard`, `no-adhoc-drive`,
  `syscall-adhoc-loop`, `syscall-no-await`, `syscall-ctx-bridge`, or
  `all`. See [`xtask/src/lint.rs`](../../../xtask/src/lint.rs) for the
  current set.

### `cargo xtask boundary-report [--top N] [--json]`

Counts raw `tx_substrate::*` / `tx_reactor::*` references *outside*
`#[platform_adapter]` modules. Pure counter — never fails. Use it to
diagnose a `lint boundary` regression and see which call sites need to
move into an adapter.

## Build & image

### `cargo xtask doctor`

Verifies toolchain, qemu, target triples. Cheap. Run when a new clone
or new machine misbehaves.

### `cargo xtask full-build [--target TARGET] [--skip-doctor] [--no-image]`

Doctor + kernel build + initramfs/disk image. **Prepare step before
`cargo xtask qemu`.** Targets: `rv64-qemu`, `la64-qemu`,
`rv64-m1dock-mock`, `all` (default: `rv64-qemu`). `--no-image` skips
the image build when you only want the kernel ELF.

### `cargo xtask build --target TARGET`

Kernel build only — no doctor, no image. Use when iterating on the
kernel ELF and the image is already correct.

### `cargo xtask image <kind> --profile busybox [--target TARGET] [--size N]`

Build a disk image only. Kinds:

- `cpio` — initramfs cpio newc archive.
- `ext4 [--size 64M]` — ext4 disk image.
- `m1dock-sd [--size 64M] --target rv64-m1dock-mock` — SD image.

Reach for `image` only when you specifically want to rebuild the
filesystem; `full-build` already calls it.

## Run

### `cargo xtask qemu --target TARGET --profile PROFILE [flags]`

Launches QEMU against the already-built artifacts. Profiles: `smoke`,
`busybox`. Flags:

- `--dry-run` — print the qemu command without running it.
- `--expect-sentinel` — fail unless the kernel emits the boot-sentinel
  line; pair with a `--timeout-ms`.
- `--timeout-ms N` — kill QEMU after N ms.
- `--no-block` — return immediately; useful with `--interactive`.
- `--interactive` — attach the user's terminal to QEMU stdio.

### `cargo xtask test [smoke|busybox-boot] [--target TARGET] [flags]`

Convenience wrapper: `full-build` + `qemu` + sentinel assertion. Flags:

- `--timeout-ms N` — bound the whole test.
- `--dry-run` — print but don't run.
- `--trap-trace` — capture and decode any traps observed during the
  run.

Default target is `rv64-qemu`. `smoke` is the minimal kernel boot;
`busybox-boot` is the full userland start.

### `cargo xtask shell-test --target TARGET --script PATH [flags]`

Runs guest shell scenarios from a script file. Flags:

- `--group NAME[,NAME...]` — run only the named test group(s).
- `--list-groups` — print available group names from the script.
- `--keep-going` — don't stop on first failure.

## Diagnose

### `cargo xtask fault-decode --target rv64-qemu [mode]`

Authoritative RV64 trap decoder. **Prefer this over hand-decoding
`scause`/`sepc`/`stval`.** Handles low-linked and high-VMA ELF
layouts, direct-map classification, demangling, conservative
code-pointer trace, full RV64C compressed decode, stack dump with
heuristic scanning, and kernel panics (the panic handler emits a
synthetic `scause=3 sepc=<ra> stval=0` so panics parse identically to
hardware traps).

Modes:

- `--serial PATH [--all]` — parse a full QEMU log; `--all` decodes
  every trap (default: first only).
- `--scause HEX --sepc HEX --stval HEX` — decode a single trap.
- `--addr HEX` — annotate one address against the kernel ELF.

Flags:

- `--elf PATH` — explicit kernel ELF (default: from target).
- `--user-elf PATH` — annotate user-space addresses too.
- `--brief` — one line per trap.
- `--json` — structured output.
- `--summary` — aligned table + per-cause histogram.
- `--color` / `--no-color`.

### `cargo xtask trap-trace --serial PATH [flags]`

Parse a serial log into a trap/syscall trace. One of:

- `--syscalls` — syscall trace view.
- `--raw` — raw trap-frame view.

Pair with `cargo xtask test --trap-trace` to capture the log
automatically.

## Progress

See `tx-progress-memory` for the writing discipline.

- `cargo xtask progress validate` — schema-check every JSON record under
  `docs/progress/`. Run after editing any plan/handoff/worktree JSON.
- `cargo xtask progress list plans|handoffs|worktrees|all [--json]` —
  enumerate records.
- `cargo xtask progress new plan|handoff|worktree --id ID --title TITLE [...]` —
  scaffold a new record from a template.
- `cargo xtask progress claim plan|worktree --id ID --owner NAME --scope PATH [--scope PATH]` —
  mark a record owned with a write scope.
- `cargo xtask progress close plan|handoff|worktree --id ID --status STATUS` —
  finalize a record.

## Observation

### `cargo xtask observe <subcmd>`

Trace pipeline tooling (not surfaced in `xtask --help` — see
[`xtask/src/observe.rs`](../../../xtask/src/observe.rs)). Subcommands:

- `replay` — decode a `.txtrace` file → NDJSON (default) or Perfetto.
- `pftrace` — alias for `replay --out pftrace --output <path>`.
- `validate` — parse the trace header + walk slots, print a one-line
  summary.
- `demo` — generate a small synthetic `.txtrace` for testing.

### `cargo xtask observe-discipline`

OBS-7 lint: fails if any `StepOp::step` body calls `tx_observe::*`
directly. Also not in `--help`; safe to run.

## OS-comp / submission

### `cargo xtask oscomp <subcmd>`

OS-competition harness. Subcommands: `doctor`, `prepare`, `submit`,
`run`, `qemu`. Use only when working the oscomp workflow.

### `cargo xtask submit k210 [--out target/submit/k210]`

Build a K210 submission artifact.

## Rules

- For any code edit, the first verification is `cargo -q xtask unit`.
  One-line-per-step output; only failing tests print detail.
- Never hand-decode RV64 traps — reach for `fault-decode` first. It
  handles every layout and synthetic-panic case the manual decoder
  trips on.
- `full-build` is the prepare step for `qemu`. `test [profile]` chains
  all three when you want the full loop.
- `progress validate` is required after editing any JSON record under
  `docs/progress/`. CI runs it too.
- `lint boundary` and `boundary-report` are counter/ratchet pair — the
  report diagnoses, the lint enforces. Do not raise the ceiling
  constants in [`xtask/src/lint.rs`](../../../xtask/src/lint.rs)
  without a decision note.

## Done means

- The right xtask command was named in the response (not improvised
  `cargo` invocations that bypass the harness).
- Trap/fault analysis cites `fault-decode` output, not hand decoding.
- Any progress-file edit was followed by `progress validate`.
- New or moved gates updated this skill and the `print_usage` block in
  [`xtask/src/lib.rs`](../../../xtask/src/lib.rs) together.
