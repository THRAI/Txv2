# TimerQueue/DeadlineFuture Retirement

Date: 2026-07-06

## Summary

Completed the Package E retirement slice from
[`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md): active Rust no
longer contains the old reactor-local timeout queue/future path or the
`tx_subsystems::timer_sleep` global install facade.

## What Changed

- Replaced reactor wait timeout state with private `ProtocolTimer` guards that
  install `TimerGuardRole::DeadlineAbort` entries into the unified
  `TimerWheel` registry.
- Removed `ReactorShared.timers`, `Reactor::sleep_until`, and
  `Reactor::timer_queue`; `advance_time_to` and `next_deadline_ns` now drive
  only `TimerRegistry`.
- Reduced `tx_reactor::timer` to a re-export of the unified substrate timer
  facade.
- Added a syscall-side `deadline_timer(ctx, deadline_ns)` future over
  `SyscallCtx.timer_wheel`, then migrated pselect, epoll, sigtimedwait polling,
  socket itimer waits, and timerfd blocking waits to it.
- Deleted `tx_subsystems::timer_sleep` and removed the boot-time sleep seam
  installation from `tx-kernel`.
- Cleaned active Rust comments and test names that still referred to the old
  queue/future path.

## Verification

- `rg -n "\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue" crates boards --glob '*.rs'`
  returned no matches.
- `cargo fmt --check` passed.
- `cargo check -p tx-substrate -q`, `cargo check -p tx-reactor -q`,
  `cargo check -p tx-subsystems -q`, `cargo check -p tx-shims -q`,
  `cargo check -p tx-kernel -q`, and `cargo check -p tx-scripts -q` passed.
  Existing unrelated warnings remain in `tx-subsystems`, `tx-fs`, and
  `tx-kernel`.
- `cargo test -p tx-reactor --test v3_timer_surface -- --nocapture` passed
  14/14.
- `cargo test -p tx-reactor --test timer_idle -- --nocapture` passed 4/4.
- `cargo test -p tx-reactor --test wait_bus -- --nocapture` passed 14/14.
- `cargo test -p tx-reactor --test wait_interrupt -- --nocapture` passed 6/6.
- `cargo test -p tx-reactor --test completion -- --nocapture` passed 6/6.
- `cargo test -p tx-reactor --test reactor_smoke -- --test-threads=1 --nocapture`
  passed 40/40.
- `cargo test -p tx-shims --lib -- --test-threads=1` passed 555/555.
- `cargo test -p tx-scripts drive_yield_on_wait_source_with_deadline_returns_etimedout -- --nocapture`
  passed.
- `cargo -q xtask unit` passed: build plus `tx-shims` 555/555,
  `tx-kernel` 91/91, `tx-ext4` 9/9, and `tx-scripts` 56/56.

## Next Step

Continue semantic convergence above the retired interfaces: route more
wait-source and delegate wake classes through the owner-aware wake boundary, and
turn raw timer-wheel dependencies in higher-level syscall/subsystem code into
narrow registrar/timekeeper handles where appropriate.

## Blockers

No blocker for the named old-interface retirement. Remaining warnings observed
during verification pre-existed this slice and are not from the time/wake
changes.
