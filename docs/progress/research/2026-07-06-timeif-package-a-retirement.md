# TimeIf Package A Retirement

Date: 2026-07-06

## Summary

Implemented Package A from
[`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md): the active Rust
HAL time interface is now split into `MonotonicCounterIf` and
`DeadlineTimerIf`, and the retired `TimeIf` name is removed from active Rust
code.

## What Changed

- Replaced `tx_hal::TimeIf` with two hardware capability traits:
  `MonotonicCounterIf` for `read_ns` / `frequency_hz`, and
  `DeadlineTimerIf` for `set_deadline_ns` / `cancel_deadline` /
  `enable_timer_wakeups`.
- Updated `TxPlatform` to require the two target traits directly.
- Split all current board implementations:
  RV64 qemu-virt, RV64 m1dock mock, and LA64 qemu-virt.
- Migrated active callers and fake test platforms across `tx-kernel`,
  `tx-shims`, `tx-observe`, `tx-subsystems`, `tx-substrate` tests, and board
  tests.
- Narrowed read-only consumers where obvious:
  `tx-observe` and `tx_subsystems::wall_clock` now depend only on
  `MonotonicCounterIf`.

## Verification

- `rg -n "\bTimeIf\b|tx_hal::TimeIf|TIMEIF_SPLIT_TODO" crates boards --glob '*.rs'`
  returned no matches.
- `cargo fmt --check` passed.
- `cargo check -p tx-hal -q` passed.
- `cargo check -p tx-observe -q` passed.
- `cargo check -p tx-subsystems -q` passed with the pre-existing
  `step_connect.rs` unused-variable warning.
- `cargo check -p tx-kernel --tests -q` passed with pre-existing warnings in
  `tx-subsystems`, `tx-fs`, and `tx-kernel` test-only helpers.
- `cargo check -p tx-shims --tests -q` passed with pre-existing warnings in
  `tx-subsystems` and `tx-fs`.
- `cargo check -p tx-substrate --tests -q` passed.
- `cargo check -p tx-hal-riscv64-qemu-virt --tests -q` passed.
- `cargo check -p tx-hal-loongarch64-qemu-virt --tests -q` passed.
- `cargo check -p tx-hal-riscv64-m1dock-mock --tests -q` passed.

## Remaining Work

Package A is complete for active Rust code, but the full time/wake refactor is
not complete. The next retire target is the legacy software timer path:
`TimerQueue`, `DeadlineFuture`, `timer_sleep::install_timer_queue`, and
`timer_sleep::sleep_until_ns` still appear in active runtime code. Package B/C/D
also still need the `TimekeeperIf`, `TimerRegistrar` / `TimerRegistry`, and
`WakeRouter` facades before Package E can fully remove the legacy path.
