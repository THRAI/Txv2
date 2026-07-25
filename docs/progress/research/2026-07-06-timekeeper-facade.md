# Timekeeper Facade Slice

Date: 2026-07-06

## Summary

Completed the Package B facade slice for the time/wake architecture. The active
design document now treats `TimekeeperIf` as the cross-module semantic time
surface, and the current code paths for clock syscalls, vDSO/VVAR publication,
timerfd realtime conversion, futex timeout conversion, interval timers, and
nanosleep deadline conversion call through `timekeeper()` instead of importing
raw `wall_clock` globals or reading the platform counter directly.

This does not implement RTC seed/writeback. That remains Package F scope.

## What Changed

- Added the explicit `TimekeeperIf` contract section to
  [`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md).
- Updated the design document's implementation status and current-code
  alignment so Package B is recorded as partially landed: facade and core call
  sites are in place; persistent-clock integration remains open.
- Migrated vDSO/VVAR publication to `timekeeper().publish_vvar()` and
  `timekeeper().set_clock_params()`.
- Migrated timerfd realtime-deadline conversion and realtime generation reads
  to `timekeeper()`.
- Migrated time syscall and futex timeout conversion reads that were in the
  Package B scope to `timekeeper().monotonic_now_ns()` or
  `timekeeper().monotonic_deadline_from_realtime_ns()`.
- Kept hardware deadline programming on `DeadlineTimerIf`; the timekeeper does
  not arm hardware timers.

## Verification

- `cargo fmt --check` passed.
- `cargo check -p tx-subsystems -q` passed.
- `cargo check -p tx-shims -q` passed.
- `cargo check -p tx-kernel -q` passed.
- `cargo test -p tx-subsystems wall_clock -- --nocapture` passed.
- `cargo test -p tx-shims time_syscalls -- --nocapture` passed 20/20 filtered
  tests.
- `cargo test -p tx-shims timerfd_dispatch -- --nocapture` passed 4/4 filtered
  tests.
- `cargo test -p tx-shims futex_dispatch -- --nocapture` passed 14/14 filtered
  tests.
- The old-interface active Rust audit had no matches:
  `rg -n "\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue" crates boards --glob '*.rs'`.
- The raw wall-clock API audit had no matches outside compatibility wrappers:
  `rg -n "wall_clock::(monotonic_now_ns|realtime_now_ns|set_realtime_ns|generation|realtime_offset_ns|monotonic_deadline_from_realtime_ns|snapshot_for_vvar|publish_vvar|set_clock_params)" crates --glob '*.rs'`.
- Scoped whitespace check passed:
  `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md crates/tx-subsystems/src/wall_clock.rs crates/tx-shims/src/linux_syscall/time.rs crates/tx-shims/src/linux_syscall/timerfd.rs crates/tx-shims/src/linux_syscall/vm.rs crates/tx-subsystems/src/timerfd/mod.rs crates/tx-kernel/src/vdso/mod.rs crates/tx-subsystems/src/vdso/mod.rs`.
- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/progress/STATUS.md docs/progress/research/2026-07-06-timekeeper-facade.md`
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class.
- `cargo -q xtask unit` passed (`tx-shims` 555/555, `tx-kernel` 91/91,
  `tx-ext4` 9/9, `tx-scripts` 56/56).

## Next Step

Package F should add `PersistentClockIf`, `RtcDeviceOps`, devfs RTC routing,
and boot seed/writeback hooks. Separately, older timeout helpers that still read
the monotonic counter directly should be narrowed as they are moved onto
registrar-owned waits.

## Blockers

No blocker for the facade slice. Full time/wake target architecture remains
incomplete until RTC device integration and broader wake-class convergence land.
