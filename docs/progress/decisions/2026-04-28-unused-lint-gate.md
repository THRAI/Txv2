# Unused Lint Gate

Date: 2026-04-28

## Decision

`cargo xtask lint unused` is now a first-class lint gate. It runs Cargo checks
with `RUSTFLAGS=-Dunused` for the host workspace and for installed board target
checks, currently RV64 QEMU and RV64 M1 Dock mock on this machine. Missing
targets are reported as skips rather than hidden.

The architecture lint also rejects normal-code `#[allow(dead_code)]` and
`#[allow(unused...)]` escape hatches. Staged boot helpers must be live on their
target path or gated behind `cfg(test)` / target cfg.

## Context

The RV64 high sentinel is still needed as the last runtime proof before the
temporary identity bridge is cleared: it verifies high `pc`, `sp`, and `gp`
after the high jump and before `drop_lower()`. The unused lint caught host-side
false positives for that target-only path, so the sentinel code is now compiled
only for RV64 or tests.

## Verification

- `cargo fmt --check`
- `cargo test -p xtask lint::tests`
- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo xtask lint arch`
- `cargo xtask lint docs`
- `cargo xtask lint unused`
- `cargo xtask ci`
- `cargo check -p tx-hal-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 10000`

## Next

Keep the sentinel until the bootstrap identity bridge no longer exists or the
handoff has a stronger proof that high `pc`, `sp`, and `gp` cannot regress.
