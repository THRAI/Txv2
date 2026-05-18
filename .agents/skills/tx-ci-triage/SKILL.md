---
name: tx-ci-triage
description: Use when a txKernel CI run fails or a `cargo xtask ci` / `ci-slow` gate breaks — locating the right gate from its txdoc tag, reproducing locally, deciding regression vs. ratchet, and avoiding the common wrong-fixes (silencing lints, raising ceilings, hiding warnings). Trigger whenever the user mentions a failing check, a red CI job, a ratchet regression, a clippy gate, an invariants lint, a missing target, the QEMU smoke sentinel, the busybox boot sentinel, a docs-lint broken link, a stale txdoc tag, or any specific `txdoc:CI-GATE-*` reference, even if they don't explicitly say "CI". Also use when planning a change that touches lint behavior or could move a ratchet.
---

# tx-ci-triage

Use this skill to take a failing txKernel CI signal and turn it into the smallest correct fix. CI here is intentionally noisy on purpose — every gate maps to an architectural invariant — so the trap to avoid is making the red go away (silencing lints, bumping a ceiling, adding `#[allow(...)]`) instead of understanding what the gate caught.

## Read First

- `docs/design/00_meta-framework/CI_REPORTING_v1.md` — the gate catalog and reporting contract.
- `.github/workflows/check.yml` — what GitHub actually runs.
- `xtask/src/ci.rs` — local source of truth for the `ci` and `ci-slow` step lists.
- `xtask/src/lint.rs` — `arch`, `docs`, `unused`, `boundary` lints and their text rules.
- `xtask/src/lint_invariants_*.rs` — one file per invariants sub-rule, each with a ratchet constant.
- `clippy.toml` — the retired-vocabulary regression list.

## The Two Gates

CI is exactly two `cargo xtask` invocations, both reported under `CI_REPORTING_v1` with one ✓/↷/✗ line per step plus a final summary:

- **`cargo xtask ci`** (fast, GitHub job `check`) — fmt, clippy, retired vocab, host check, host tests, four `xtask lint` subcommands (`arch`, `docs`, `unused`, `boundary`), `xtask lint invariants all`, `progress validate`, observe demo+validate, RV64 + M1 Dock mock target checks, optional LA64 target check.
- **`cargo xtask ci-slow`** (slow, GitHub job `qemu-smoke`, runs after `check`) — RV64 build, RV64 QEMU smoke with sentinel `txkernel:qemu-riscv64-virt:boot:ok`, optional busybox boot sentinel when `tools/images/.../busybox` is vendored.

Locally, prefer `cargo -q xtask unit` for the fastest sanity check; reach for full CI only when reproducing a real failure or finishing a change.

## Triage Loop

1. **Read the report, not the cargo spam.** CI prints `✓ <name> — passed (CI_REPORTING_v1.md txdoc:CI-GATE-…)`, `↷ … skipped: <reason>`, or `✗ <name> — failed` followed by `command:`, `status:`, and the tail of stdout+stderr (last 80 lines). The `txdoc:CI-GATE-*` tag is the stable anchor — grep on that string to find the gate definition and the matching step in `xtask/src/ci.rs`.
2. **Reproduce the exact step locally.** Copy the `command:` line verbatim. Don't approximate — many gates exclude specific packages or pass `--test-threads=1` for a reason (e.g. the unit-tests gate serializes on `test_support::EPOCH_TEST_LOCK`; running tests with default parallelism races on zone registration and will fail differently than CI).
3. **Classify before fixing.** Each gate failure is one of:
   - **Real regression** — the code violates an invariant; fix the code, not the gate.
   - **Ratchet movement** — an `xtask lint invariants` or `lint boundary` ceiling needs to *lower* (preferred when the code now satisfies the rule better) or *raise* (requires an explicit decision note; see "Ratchets").
   - **Environment skip** — missing rustup target, missing busybox, missing LA64 toolchain. Required gates fail; optional gates print `↷ skipped`. Install the target / vendor the artifact rather than weakening the check.
   - **Doc drift** — a markdown link rotted, a `txdoc:` tag duplicated, a retired term resurfaced, a `stale-vocabulary` warning increased. Fix the doc.
4. **Verify with the same gate, not the whole suite.** Re-run only the failing `cargo xtask` invocation to confirm. Run the full `cargo xtask ci` only once you believe the fix is complete.

## Gate-by-Gate Quick Reference

| Tag | Step | Common failure shape | First instinct |
|---|---|---|---|
| `CI-GATE-FMT` | `cargo fmt --check` | Diff hunks in tail | `cargo fmt` and recommit. |
| `CI-GATE-CLIPPY` | `cargo clippy … -D warnings` | A specific `clippy::…` lint name | Fix the lint at the call site. Adding `#[allow(...)]` is almost never the right fix — see "What not to do". |
| `CI-GATE-RETIRED-VOCAB` | clippy with only `disallowed_{names,types,methods}` denied | A name from `clippy.toml`'s list (`OnCarrier`, `WakeCarrier`, `exit_port`, …) reintroduced | Use the v4 replacement vocabulary; consult the D10/D13 decision notes referenced in `clippy.toml`. |
| `CI-GATE-HOST-CHECK` | `cargo check --workspace` | A normal rustc error | Fix the build error. |
| `CI-GATE-UNIT-TESTS` | `cargo test --workspace … -- --test-threads=1` | A failing test, or a hang/race if run in parallel locally | Reproduce *with* `--test-threads=1`. Tests touching zone state share `EPOCH_TEST_LOCK`. |
| `CI-GATE-ARCH-LINT` | `cargo xtask lint arch` | `unused/dead-code allowance`, `resurrected runtime HAL vocabulary`, `raw Zone<T, Policy> outside substrate`, `BootHandoff` / `_start` violations, TTY-CTL-1 / TTY-CTL-1a, cross-board imports, file >1600 lines | Each finding includes a path:line and a one-line explanation. The fix is structural, not a suppression. |
| `CI-GATE-DOC-LINT` | `cargo xtask lint docs` | `broken link`, `missing file-level txdoc tag`, `missing fine-grained txdoc section anchors`, `invalid txdoc tag`, `duplicate txdoc tag`, or a stale-vocabulary count increase | Fix the link or tag. Every active design doc needs ≥2 `txdoc:` tags (one file-level, one section). Tags are uppercase + digits + `-_.:`. |
| `CI-GATE-UNUSED-LINT` | `cargo xtask lint unused` | `unused_imports`/`dead_code` surfaced on a target build | Delete or correctly gate the item. Don't add `#[allow(...)]`. |
| `CI-GATE-BOUNDARY-RATCHET` | `cargo xtask lint boundary` | `substrate outside adapters: N > 0` or `reactor outside adapters: N > 0` | Route the new call site through a `#[platform_adapter]` module (existing or new) — see `tx-hal-axhal`. Do **not** raise the ceiling constants in `xtask/src/lint.rs` without a decision note. Use `cargo xtask boundary-report` to see the offending lines. |
| `CI-GATE-INVARIANTS-LINT` | `cargo xtask lint invariants all` | Sub-rule name + `ratchet regression — N > ceiling M` | Run just the failing sub-rule (e.g. `cargo xtask lint invariants step-discipline`) and consult the matching `xtask/src/lint_invariants_*.rs` header — every rule cites the design doc it enforces. |
| `CI-GATE-PROGRESS-JSON` | `cargo xtask progress validate` | A plan/handoff/worktree JSON failed schema | Validate locally; consult `tx-progress-memory` for record shapes. |
| `CI-GATE-OBSERVE-SMOKE` | `xtask observe demo` then `observe validate` | Trace header or record-count mismatch | Inspect the produced `/tmp/txkernel-ci-observe-demo.txtrace`; the validator is in `xtask/src/observe.rs`. |
| `CI-GATE-RV64` | `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf` | Target check error | Required gate. Reproduce with the same `--target`. Missing target → `rustup target add riscv64gc-unknown-none-elf`. |
| `CI-GATE-M1DOCK-MOCK` | `cargo check -p tx-kernel-riscv64-m1dock-mock --target …` | Same shape as RV64 | Required gate. |
| `CI-GATE-LA64` | `cargo check -p tx-kernel-loongarch64-qemu-virt --target …` | Often `↷ skipped` when the LA64 target isn't installed | If skipped locally, install `rustup target add loongarch64-unknown-none` (the CI host installs it explicitly in `.github/workflows/check.yml`). |
| `CI-GATE-QEMU-SMOKE` | `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 30000` | Boot didn't reach `txkernel:qemu-riscv64-virt:boot:ok` within timeout, or hit a trap | Use `cargo xtask fault-decode --target rv64-qemu --serial <log> --summary` (or `--all`) before hand-decoding `scause`/`sepc`/`stval`. Run `cargo xtask full-build` then `cargo xtask qemu` to iterate. |
| `CI-GATE-QEMU-BUSYBOX-BOOT` | `cargo xtask test busybox-boot --target rv64-qemu` | Boot stalls or a syscall regression breaks init | Same decoder workflow; `cargo xtask test busybox-boot` already chains `full-build`. If reported as `↷ skipped: vendored busybox missing`, run `tools/images/fetch-busybox.sh`. |

## Ratchets

`lint boundary` and every `lint invariants` sub-rule carry a `MAX_*` constant — a one-way ratchet. The intent is that the only direction these move under normal work is *down*.

- The diagnostic is `<rule> ratchet regression — N > ceiling M`. The number is a count (offending lines, files, or violations), not severity.
- If the new code is legitimate but the ratchet is too tight, lowering counts elsewhere is usually possible. Read the rule's source file (`xtask/src/lint_invariants_*.rs`) — the header comment cites the design doc, the constant has a `// 0→N: rationale` trailing comment showing the last bump.
- **Raising a ratchet requires an explicit decision note** in `docs/progress/decisions/`, dated, with rationale and the burn-down plan. Then update the inline comment alongside the constant to reference the note. The boundary-ratchet decision (`2026-05-13-d49-phase8-boundary-ratchet.md`) is the canonical example. Consult `tx-progress-memory` for the record shape.
- `cargo xtask boundary-report` enumerates the substrate/reactor outside-adapter call sites; use it to choose between routing through an adapter and (rarely) raising the ceiling.

## What Not To Do

These all "fix" CI in the short term and break the invariant the gate guards:

- **Don't add `#[allow(dead_code)]`, `#[allow(unused)]`, `#[allow(unused_imports)]`, or `#[allow(unused_variables)]` to make `lint arch` happy.** The arch lint rejects exactly these allowances because they hide stale boot/API surfaces. Delete the item or properly gate it behind `cfg(test)`. The one documented exemption is the `txdoc:pr2-step-op-scaffold` marker.
- **Don't raise a `MAX_*` ratchet constant** to silence `lint boundary` or `lint invariants` without a decision note.
- **Don't introduce `HalManager`, `Box<dyn Hal>`, `dyn Hal`, `__ostd_main`, raw `Zone<T, Policy>` outside substrate, or any name in `clippy.toml`'s `disallowed-names`.** The retired-vocabulary gate is structural: see `tx-meta-alignment` and the D10/D13 decision notes.
- **Don't cfg on `target_arch` inside `crates/tx-kernel/`** or take raw firmware boot values; the kernel consumes `BootHandoff` only. Boards (`boards/tx-kernel-*`) export `rust_entry`; platform crates own `_start`.
- **Don't import a concrete board (`tx_hal_riscv64_qemu_virt`, `tx_hal_riscv64_m1dock_mock`, `tx_hal_loongarch64_qemu_virt`) outside its board boundary.** See `tx-hal-axhal`.
- **Don't comment out a failing unit test** to ship; reproduce with `--test-threads=1` first — many failures are parallelism races on `EPOCH_TEST_LOCK`, not real bugs.
- **Don't delete a `txdoc:` tag** to make `lint docs` pass after a duplicate report. The duplicate has a previous location in the message — one of the two needs to be renamed or removed for the right reason.
- **Don't bypass git hooks (`--no-verify`)** to land a commit while CI is red.

## Decoding RV64 QEMU Traps

When `CI-GATE-QEMU-SMOKE` or `CI-GATE-QEMU-BUSYBOX-BOOT` fails with a serial log containing a trap or panic, run `cargo xtask fault-decode --target rv64-qemu --serial <log>` *before* hand-decoding `scause` / `sepc` / `stval`. The tool handles:

- low-linked and high-VMA ELF layouts (direct-map classification);
- demangling and conservative code-pointer tracing through data;
- full RV64C compressed instruction decode;
- stack-dump heuristic code-pointer scanning;
- kernel panics — the panic handler emits a synthetic `scause=3 sepc=<ra> stval=0` line so panics parse identically to hardware traps.

Useful flags: `--all` (every trap in the log), `--brief` (one line per trap), `--summary` (aligned table + histogram), `--json`, `--user-elf <path>` (annotate user-space addresses).

## Local-First Workflow

- `cargo -q xtask unit` — fastest path to build + host unit tests. One line per step on pass; failures show only the failing test name and panic message.
- `cargo xtask full-build [--target TARGET] [--skip-doctor] [--no-image]` — prepares everything for a QEMU run (environment doctor, kernel ELF, initramfs/disk image). Chained by `cargo xtask test [busybox-boot|smoke]`.
- `cargo xtask ci` — the full fast gate. Run before declaring a non-trivial change complete.
- `cargo xtask lint invariants <sub-rule>` — re-run a single invariants sub-rule when iterating on a ratchet.
- `cargo xtask boundary-report` — enumerate substrate/reactor outside-adapter call sites for `CI-GATE-BOUNDARY-RATCHET`.

## Cross-Skill Pointers

- Boundary or HAL gate failure → `tx-hal-axhal`.
- Invariants sub-rule about steps, scripts, drives, or syscalls → `tx-subsystem-manifest`, then the rule's source file.
- Retired-vocabulary or doc-lint stale-term failure → `tx-meta-alignment`.
- Markdown link or `txdoc:` tag failure under `docs/design/` → `tx-design-reference` and `tx-docs-cleanup`.
- Recording a ratchet movement, decision note, or follow-up plan → `tx-progress-memory`.

## Done Means

- The failing step is identified by its `txdoc:CI-GATE-*` tag and reproduced locally with the same command.
- The fix addresses the invariant the gate guards — code change, doc fix, target install, or (rarely, with a decision note) a ratchet update.
- The exact failing gate passes locally on a fresh run; `cargo xtask ci` (or `ci-slow` for boot gates) is green end-to-end before marking the task complete.
- The progress catch-up (`tx-progress-memory`) records what changed if the fix touched architecture, lint behavior, or a ratchet.
