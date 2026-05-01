# Reactor Serial API Prework

**Date:** 2026-04-30

**Status:** accepted for the first reactor worker fanout.

## Context

The reactor parallel-shard plan needed a small serial decision pass before
workers can own isolated files. The blocking ambiguities were the public wait
adapter shape, lost-wake registration rule, completion credit semantics, task
drain/cancel naming, and yield naming.

## Decisions

- The script-facing wait adapter is `wait_event(channel, mask, protocol,
  condition) -> WaitOutcome`. A lower raw channel wait may remain as an
  implementation/test helper, but subsystem scripts should not bypass
  `wait_event`.
- `WaitOutcome::Ready` is the retry signal returned by wait-adapt. It is not a
  claim that the wake itself was truth; the driver still re-invokes the step
  under a fresh guard.
- Lost-wake safety requires `check -> register -> recheck -> park`, or a
  substrate primitive proving the same linearization. A wake is only a sleep
  enabler.
- Default `Completion` is credit-consuming: each successful wait consumes one
  completion credit. Broadcast or latch behavior must use a distinct type name,
  such as `BroadcastCompletion` or `LatchCompletion`. `CountdownCompletion`
  remains a closed participant-count latch.
- Task lifecycle workers should use `cancel_task` for targeted cancellation,
  `drain_completed` for completed task cleanup, and `drain_cancelled` for
  cancellation-driven cleanup. These names keep explicit cause in API/tests.
- Scheduler/runtime workers should use `yield_now` for cooperative task yield.

## Consequences

- Wait/bus worker tests should exercise the register/recheck/park race rather
  than treating subscription as sufficient.
- Completion work must not accidentally implement a broadcast latch under the
  default `Completion` name.
- The serial module split can preserve the existing smoke behavior while
  giving workers stable file ownership.

## Verification

- Pending with the serial module split: `cargo test -p tx-reactor`,
  `cargo xtask lint docs`, `cargo xtask progress validate`, and
  `git diff --check`.

## Next Step

Split `crates/tx-reactor/src/lib.rs` into worker-owned modules without behavior
changes, then mark the two serial plan steps complete.
