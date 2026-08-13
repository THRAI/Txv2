# Timer Registrar Handle Slice

Date: 2026-07-06

## Summary

Completed the next timer-interface convergence slice from
[`TIME_WAKE_v1.md`](../../design/02_execution/TIME_WAKE_v1.md): producer-side
contexts now carry a `TimerRegistrarHandle` instead of a raw `TimerWheel`.
The reactor/substrate side still owns the concrete `TimerWheel` as the registry
and driver object, but syscall/script yield resolution no longer depends on the
wheel type directly.

## What Changed

- Added `TimerRegistrarHandle` in `tx_substrate::wake::timer`.
- Implemented `TimerRegistrar` for the handle and exposed
  `TimerWheel::registrar_handle()` for reactor-owned registries.
- Changed `ScriptCtx` from `timer_wheel` / `with_timer_wheel` /
  `timer_wheel()` to `timer_registrar` / `with_timer_registrar` /
  `timer_registrar()`.
- Changed `SyscallCtx` from `timer_wheel` / `with_timer_wheel` to
  `timer_registrar` / `with_timer_registrar`.
- Updated `tx_scripts::drive()` so `OnTimer`, wait-source protocol deadlines,
  and `OnAgent` delegate deadlines install through `TimerRegistrarHandle`.
- Narrowed `DelegateRegistry::install_request` so paired delegate timeouts
  receive `TimerRegistrarHandle` rather than `&TimerWheel`.
- Removed raw `TimerWheel` re-exports from the producer-facing
  `tx-scripts` and `tx-shims` adapters.
- Updated `TIME_WAKE_v1.md` current-code alignment and Package C notes.

## Verification

- `cargo fmt --check` passed.
- `cargo check -p tx-substrate -q` passed.
- `cargo check -p tx-scripts -q` passed.
- `cargo check -p tx-shims -q` passed.
- `cargo check -p tx-kernel -q` passed.
- `cargo test -p tx-substrate --test v3_agent_token_guard_timer -- --nocapture`
  passed 10/10.
- `cargo test -p tx-reactor --test v3_pr7b_timer_routing -- --nocapture`
  passed 9/9.
- `cargo test -p tx-scripts drive_yield_on_wait_source_with_deadline_returns_etimedout -- --nocapture`
  passed.
- `cargo test -p tx-shims futex_dispatch -- --nocapture` passed 14/14 filtered
  tests.
- The old-interface active Rust audit had no matches:
  `rg -n "\bTimeIf\b|TimerQueue|DeadlineFuture|timer_sleep|install_timer_queue|sleep_until_ns|DirectMailboxTimerWakeRouter|\.fire_due\(|timer_queue" crates boards --glob '*.rs'`.
- The producer-context raw wheel audit has no `SyscallCtx` / `ScriptCtx`
  timer-wheel fields or accessors:
  `rg -n 'ctx\.timer_wheel|pub timer_wheel|with_timer_wheel|timer_wheel\(\)|timer_wheel_arc|ScriptCtx[^\n]*timer_wheel|SyscallCtx[^\n]*timer_wheel' crates/tx-shims crates/tx-scripts crates/tx-substrate crates/tx-kernel --glob '*.rs'`.
- `git diff --check -- docs/design/02_execution/TIME_WAKE_v1.md docs/progress/STATUS.md docs/progress/research/2026-07-06-timer-registrar-handle.md crates/tx-substrate/src/wake/timer.rs crates/tx-substrate/src/step/mod.rs crates/tx-substrate/src/step/agent.rs crates/tx-scripts/src/drive.rs crates/tx-scripts/src/adapter.rs crates/tx-shims/src/adapter.rs crates/tx-shims/src/linux_syscall/ctx.rs crates/tx-shims/src/linux_syscall/wait.rs crates/tx-kernel/src/thread_future.rs`
  passed.
- `cargo xtask progress validate` passed with `progress records: ok (29
  file(s))`.
- `cargo xtask lint docs` passed with the expected retired-term warning class.
- `cargo -q xtask unit` passed (`tx-shims` 555/555, `tx-kernel` 91/91,
  `tx-ext4` 9/9, `tx-scripts` 56/56).

## Next Step

Continue semantic convergence above the timer facade: timerfd/POSIX timer
objects should use registrar handles where they own future registrations, and
wait-source/delegate wake delivery still needs broader owner-aware routing.
Package F remains open for `PersistentClockIf`, `RtcDeviceOps`, devfs RTC
routing, and boot seed/writeback.

## Blockers

No blocker for this slice. Full `TIME_WAKE_v1` remains incomplete until RTC
integration and broader wake-class convergence land.
