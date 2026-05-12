# Decision: PR-3 — wake substrate shape

**Date:** 2026-05-11
**Status:** decided

## Decision

PR-3 chooses **task-owned wake delivery** and **object-owned wait
publication**. Specifically:

| Concept | Owner |
|---|---|
| `WaitSource` | semantic object whose state transition may make blocked operations worth retrying |
| `TaskMailbox` | reactor task (per `ReactorTask`, not per process) |
| `WaitGeneration` | `TaskMailbox` (per task, identifies the currently active wait) |
| `ActiveWait` | driver-local; not stored as long-lived `ThreadPayload` wait frame |

`Channel.fire(Mask)` is not semantic truth. It is migrated **behind**
`WaitSource.notify(Mask)`, which posts `WakeHint::SourceFired` to each
subscriber's `TaskMailbox` using the subscriber's stored
`WaitGeneration`.

## Why `WaitGeneration` is per-task, not per-source

A source can have many subscribers:

```
Pipe.read_source
  -> task A generation 17
  -> task B generation 91
  -> task C generation 4
```

If generation lived on the source, one source-side counter would have
to mean different things for different waiters. Wrong ownership. The
generation identifies *this task's* current active wait, not a
semantic object's state.

Right shape:

```rust
struct TaskMailbox {
    generation: AtomicU64,
    queue: BoundedMpsc<WakeHint>,
    overflow: AtomicBool,
}
```

On every active wait:

```rust
let generation = mailbox.next_generation();
```

Each wait registration stores:

```rust
mailbox: Weak<TaskMailbox>
generation: WaitGeneration
interests: InterestMask
```

`WaitSource.notify(mask)` posts wake hints with the generation
captured at registration time:

```rust
WakeHint::SourceFired {
    generation: subscriber.generation,
    source: self.id,
    interests: mask & subscriber.interests,
}
```

The task ignores stale hints:

```rust
if hint.generation != active_wait.generation {
    ignore;
}
```

Cleanly handles late wakeups after unregister, timeout, signal
interruption, or a new wait replacing an old wait.

## Interaction with `Channel.fire(Mask)`

Current `Channel.fire(Mask)` is mask-broadcast. Still compatible.

```
old:
  Channel.fire(mask)
    -> wake registered wakers

new:
  WaitSource.notify(mask)
    -> iterate subscribers
    -> post WakeHint::SourceFired { generation, source, interests }
    -> wake task mailbox
```

Mask remains a hint, not truth.

Subscribers are no longer raw wakers — they are registration records:

```rust
struct Subscriber {
    mailbox: Weak<TaskMailbox>,
    generation: WaitGeneration,
    interests: InterestMask,
}
```

`fire(Mask)` does not need to understand semantic predicates. It only
posts hints. The step re-runs and rechecks the object predicate.

## Why `TaskMailbox` is per `ReactorTask`

Not per process — a process has many threads/tasks, each potentially
blocked on a different wait.

Not per thread abstractly — kernel actors, SQPOLL workers, AIO
workers, and borrowed-scope tasks also need mailbox delivery. The
stable owner is the reactor task.

```rust
pub struct ReactorTask {
    id: TaskId,
    future: KernelFuture,
    mailbox: Cap<TaskMailbox>,
    state: AtomicTaskState,
}
```

For native user threads:

```rust
ThreadPayload {
    task: TaskHandle,
    mailbox: Cap<TaskMailbox>,
    ...
}
```

For kernel actors:

- kthread / reactor task owns its own `TaskMailbox`
- may enter `OnBehalfOf` scope
- the borrowed process does **not** own the mailbox

```
SubjectContext  -> who the script runs as
TaskMailbox     -> where wake hints for the running task are delivered
```

Do **not** tie mailbox ownership to `ProcessIdentity`.

## Migration story: wrap first, replace later

Do **not** replace `Channel` wholesale in PR-3. Phases:

### Phase PR-3A — introduce mailbox + generation

Add:
- `TaskMailbox`
- `WaitGeneration`
- `WakeHint`
- `ActiveWait`

Keep existing channels mostly intact. Let the driver allocate
generations and filter hints.

### Phase PR-3B — introduce `WaitSource` as a wrapper

```rust
pub struct WaitSource {
    id: WaitSourceId,
    readiness: AtomicReadiness,
    subscribers: SubscriberIndex,
}
```

Internally, reuse existing channel/waker plumbing initially. Public
API becomes `source.notify(mask)` / `source.register_prepared(...)`,
but implementation still calls into existing `Channel` until all
sites are moved.

### Phase PR-3C — migrate wait sites

Convert direct waker registration to:

- `PreparedWaitRegistration`
- `WaitRegistrationGuard`

This is the **critical lost-wake fix.** The old unsafe sequence:

```
observe blocked
return yield
register later
```

becomes:

```
observe blocked
prepare registration with predicate evidence
install registration conditionally
if condition changed: retry step
else: park
```

### Phase PR-3D — retire direct `Waker` sites

Once all 92 direct `Waker` sites go through `WaitSource` /
`TaskMailbox`, remove the old direct wake path (or demote it to an
internal raw primitive).

## Final relationship

```
Raw channel / raw queue   low-level delivery primitive
WaitSource                object-owned semantic wait publication point
TaskMailbox               task-owned hint delivery queue
ActiveWait                driver-local active suspension state
```

## Scope boundary

PR-1.6 is **not** a filesystem-dispatch redesign.

PR-3 **is** a runtime-substrate redesign. Reserve architectural
energy for PR-3, where ownership decisions affect the reactor's
correctness model.
