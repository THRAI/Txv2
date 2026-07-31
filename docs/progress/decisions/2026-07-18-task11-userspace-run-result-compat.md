# Task 11 userspace-run result compatibility

**Date:** 2026-07-18
**Status:** Accepted for Task 11; temporary compatibility retires in Task 12.

## Context

Task 11 makes cancellation a terminal userspace-run outcome rather than a
timer-shaped trap. That requires changing `UserspaceRunWait` from the parent
trap-only future output to `UserspaceRunResult::{Trap, Cancelled}`. Keeping the
old output would make cancellation either unrepresentable or silently map it
to `UserspaceTrapInfo::TimerPreempt`, violating preemption transparency.

The strict rendezvous also needs one error that names both the required and
observed phase. `UserspaceRunError::InvalidPhase { request, expected, actual }`
is therefore added to the parent error enum. This is a source break for callers
that exhaustively match `UserspaceRunError`, even though existing named variants
remain importable.

## Decision

Task 11 has exactly two intentional public source breaks:

1. `UserspaceRunWait::Output` changes from `UserspaceTrapInfo` to
   `UserspaceRunResult`. Direct awaiters must match
   `UserspaceRunResult::Trap(info)` and `UserspaceRunResult::Cancelled`.
2. `UserspaceRunError::InvalidPhase` is added. Exhaustive matches on the strict
   error enum must add that variant; non-exhaustive callers and callers that
   match named parent variants are unaffected.

Request, status, phase, trap, error, and slot names remain exported at
`tx_reactor::userspace::*`; this does not make exhaustive strict-error matches
source compatible.

Temporary source vocabulary is retained where it does not weaken the new state
machine: deprecated `UserspaceTrapInfo::TimerPreempt`,
`UserspaceRunPhase::Preempted`, and
`UserspaceRunError::{AlreadyResolved, NotRunning}` remain importable. Production
transitions never produce those variants.

`TimerPreempt` is compatibility vocabulary only. The centralized strict trap
classifier rejects attempts to complete it as an interesting trap with
`InvalidPhase { expected: Resolved, actual: Running }`. The request remains
`Running`, with no context publication, terminal result, or wake; there are
zero production transitions that can produce a terminal timer trap.

Trap-only callers can explicitly convert with
`UserspaceRunWait::into_legacy()` or `LegacyUserspaceRunWait::from(wait)`. The
adapter future returns
`Result<UserspaceTrapInfo, LegacyUserspaceRunWaitError>`; a trap is `Ok(info)`
and cancellation is `Err(LegacyUserspaceRunWaitError::Cancelled)`.
Cancellation is never translated into `TimerPreempt`.

`LegacyUserspaceRunError` contains exactly the six parent variants (`Busy`,
`NoActiveRequest`, `StaleRequest`, `AlreadyResolved`, `NotRunning`, and
`RequestIdExhausted`) so parent-shaped exhaustive matches can migrate to the
legacy facade. `LegacyUserspaceRunSlot` maps a strict `InvalidPhase` with an
already-resolved actual phase to `AlreadyResolved`, and maps operations that
require `Running` from any other phase to `NotRunning`. When operation context
cannot express the strict failure without loss, including duplicate dispatch
and retired timer injection, `LegacyUserspaceRunCompatError::Strict` retains
the original `UserspaceRunError`; all exact parent errors use its `Legacy`
branch. Wait cancellation remains separate from slot-operation errors.

All temporary global `Reactor` userspace facade definitions and the legacy
wait adapter live in `userspace/compat.rs`. The structural witness tokenizes
the repository sources, ignores comments, literals, and attributes, and
requires exactly one owner for each facade method and rendezvous state
declaration.

## Blast Radius

- Direct `UserspaceRunWait` awaiters must handle cancellation explicitly.
- Exhaustive `UserspaceRunError` matches must handle `InvalidPhase` explicitly.
- Existing trap-only code can opt into the deprecated adapter during migration.
- Parent-shaped slot callers can use `LegacyUserspaceRunSlot`; unmappable
  strict failures remain inspectable rather than being silently collapsed.
- `UserspaceRunSlot::cancel` keeps its parent
  `Result<(), UserspaceRunError>` signature.
- Kernel and ThreadRuntime production paths use `UserspaceRunResult` directly;
  no production path depends on the legacy adapter or retired variants.

## Retirement

Task 12 migrates the remaining compatibility callers, removes the global
`Reactor` userspace facade and `ReactorShared.userspace`, then removes
`LegacyUserspaceRunWait`, `LegacyUserspaceRunError`, and the retired vocabulary.
It also removes `LegacyUserspaceRunSlot`, `LegacyUserspaceRunCompatError`, and
`LegacyUserspaceRunWaitError` after their callers migrate. The mandated
`UserspaceRunResult` output and strict `InvalidPhase` error remain canonical.
