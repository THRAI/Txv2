# Decision D2: RawPort migration — WaitSource coexists in parallel

**Date:** 2026-05-11
**Status:** decided
**Companion:** [D1](2026-05-11-d1-scriptctx-trait-bound-identity.md), [D3](2026-05-11-d3-walker-async-carveout.md)

## Decision

Choose **A**: introduce `WaitSource` beside `RawPort`. New v3 consumers
use `WaitSource::register_prepared` and `TaskMailbox`. Old
`RawPort::subscribe(waker)` remains temporarily, marked deprecated.
**Do not rewrite `RawPort` internals yet. Do not delete all old sites
in one PR.**

## Rule

Do not turn PR-3D into a bus rewrite.

## Why not C (replace)

A 31-site forced replacement is exactly how PR-3D becomes a
long-running branch with regression risk concentrated in the wait
substrate.

## Why not B (rewrite internals)

Rewriting `RawPort` internals to synthesize a `TaskMailbox` per old
`Waker` adds allocation and lifetime behavior to the legacy API
precisely when migration should be boring. More importantly, **old
`Waker` users are not semantically equivalent to task-owned
`TaskMailbox` users**:

```
old Waker:
  wake this future somehow

new TaskMailbox:
  post replayable hint with generation
  wake reactor task
  driver filters stale hints
  step rechecks truth
```

Bridging old wakers into new mailboxes would hide exactly the
distinction PR-3 is supposed to introduce.

## Why A (parallel surfaces)

A keeps the two surfaces honest. Each call site is converted with
intent. No fake semantic equivalence between old and new wake
delivery.

## Recommended coexistence model

```rust
pub struct RawPort {
    // old Vec<Waker> path remains for old users
}

pub struct WaitSource {
    id: WaitSourceId,
    readiness: AtomicReadiness,
    subscribers: SubscriberIndex,
    // optional during transition
    legacy_port: Option<RawPort>,
}

impl WaitSource {
    pub fn register_prepared(
        &self,
        mailbox: Weak<TaskMailbox>,
        generation: WaitGeneration,
        interests: InterestMask,
        predicate: PreparedPredicate,
    ) -> Result<WaitRegistrationGuard, ConditionChanged>;

    pub fn notify(&self, mask: InterestMask) {
        self.notify_wait_source_subscribers(mask);

        // transitional only, if this object still has old users
        if let Some(port) = &self.legacy_port {
            port.fire(mask);
        }
    }
}
```

New v3 consumers:

```
WaitSource.register_prepared(...)
WaitSource.notify(mask)
TaskMailbox.post(WakeHint::SourceFired { ... })
```

Old consumers keep using:

```
RawPort.subscribe(waker)
RawPort.fire(mask)
```

Site-by-site migration.

## Deprecation rule

```rust
#[deprecated(note = "use WaitSource::register_prepared")]
pub fn subscribe(&self, waker: Waker) { ... }
```

Do not delete until the 31 sites are gone.

## PR-3D landing sequence

```
PR-3D.1:
  Add WaitSource beside RawPort.
  Add TaskMailbox / WaitGeneration / WakeHint.   ← LANDED (PR-3A/B/C)
  No mass migration yet.

PR-3D.2:
  Convert pipe readiness.
  Validates register_prepared + lost-wake behavior.

PR-3D.3:
  Convert timerfd / pidfd / socket readiness.

PR-3D.4:
  Convert remaining bus users.

PR-3D.5:
  Remove or demote RawPort.subscribe(waker).
```

This keeps the new surface honest without requiring a flag day.

## Status

PR-3D.1 is **already landed** (PR-3A/B/C in this session introduced
`TaskMailbox`, `WaitSource`, `WaitGeneration`, `PreparedWaitRegistration`,
`WaitRegistrationGuard`, `WakeHint`/`MailboxEvent`). The remaining
phases PR-3D.2 through PR-3D.5 are per-subsystem migrations that
follow this ADR's coexistence model.
