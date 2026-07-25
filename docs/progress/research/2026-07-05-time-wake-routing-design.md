# Time, Timer, And Wake-Routing Design Note

Date: 2026-07-05

## Summary

Added [`docs/design/02_execution/TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md)
as the active design reference for Tx time and timer convergence. The document
defines the target split between HAL time capabilities, `wall_clock` as
timekeeper, wake-substrate timer registration, reactor timer driving, and
scheduler-aware wake routing under work stealing.

## What Changed

- Captured the target HAL split:
  `MonotonicCounterIf`, `DeadlineTimerIf`, compatibility `TimeIf`, and
  `PersistentClockIf`.
- Defined `TimekeeperIf` over the existing wall-clock offset/generation model.
- Defined timer facets:
  `TimerRegistrar` for producers and `TimerRegistry` for the reactor driver.
- Recorded the rule that futures may hold `TimerGuard`s but must not own
  private timer queues.
- Recorded the SMP rule that timer expiry posts wake events, while CPU
  placement goes through a scheduler-aware `WakeRouter`.
- Added the design doc to `docs/design/INDEX.md`.

## Verification

- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/design/INDEX.md docs/progress/research/2026-07-05-time-wake-routing-design.md docs/progress/STATUS.md`
  passed.
- `cargo xtask lint docs` passed. It reported the pre-existing
  stale-vocabulary warning class and completed with `docs lint: ok`.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.

## Next Step

Use `TIME_WAKE_v1.md` to plan the mechanical migration:

1. split HAL time traits with compatibility `TimeIf`;
2. add `TimekeeperIf`;
3. add `TimerRegistrar` / `TimerRegistry` facades;
4. introduce `WakeRouter`;
5. retire legacy reactor `TimerQueue` users into role-tagged timer
   registrations.

## Blockers

No documentation blocker. Implementation still needs the scheduler-routed
mailbox post path before timer expiry is SMP-safe after work stealing.
