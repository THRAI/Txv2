# Completion — v1

<!-- txdoc:02-EXECUTION-COMPLETION-V1 -->

**Status.** Draft v1 companion to the reactor, bus, and script middleware model.

**Purpose.** Specify the txKernel completion object: a Linux-inspired, reactor-facing wait protocol object for counted internal rendezvous. A completion is not semantic truth, not a bus primitive, and not a subsystem entity. It is a specialized middleware object that packages a simple condition with a private wake carrier.

**Audience.** Authors of reactor wait code, syscall scripts, process/thread coordination code, and reviewers deciding whether a new "wait until X finishes" mechanism should be a completion, a bus signal, or a subsystem-specific handoff.

**Companion documents.**

- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — middleware as protocol combinator; specialized protocol objects.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — `ARCH-*`, `SIG-*`, `SCRIPT-*`, `EXC-*`, and `COMP-*`.
- [`REACTOR_v0.md`](REACTOR_v0.md) — wait/wake boundary and classified wait outcomes.
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — wake carriers, subscription, and "wake is not truth."
- [`PROCESS_v1.md`](../04_process-signals/PROCESS_v1.md) — group-exit collapse currently uses a hand-written countdown completion.

### Zone-derived type policy
<!-- txdoc:COMP-ZONE-DERIVED-TYPE-POLICY -->

Completion objects are middleware, not semantic entities:

| Completion declaration | Public handle | Reclamation role |
|---|---|---|
| one-shot/countdown state | owner-embedded value or owner-retained allocation | no independent identity |
| private wake channel | reactor/bus wait primitive | wake carrier, not truth |
| semantic condition | caller-owned field or predicate | owner supplies retention and publication |

Do not introduce `Cap<Completion>` or `Zone<Completion, Policy>` unless a
future design promotes completions to user-visible named objects with their own
identity and reclamation.

---

## 1. Classification
<!-- txdoc:COMP-1-CLASSIFICATION -->

A completion is a **specialized protocol object**:

```text
Completion = wait_event(
    channel   = completion.private_channel,
    condition = completion.done_count > 0,
    protocol  = caller-selected WaitProtocol
)
```

It is middleware because it supplies waiting protocol while the caller supplies use-site meaning. It becomes a named object only because the `(condition, channel)` pair recurs often enough that a type is safer than repeated ad hoc wait loops.

| Question | Answer |
|---|---|
| Architectural home | Reactor / wait middleware |
| Plane | Publication/execution boundary |
| Script-phase class | Wait-adapt |
| Middleware? | Yes: specialized protocol object |
| Semantic entity? | No |
| Bus primitive? | No |
| Authoritative truth? | No; only the owner-defined condition is truth |

---

## 2. Semantics
<!-- txdoc:COMP-2-SEMANTICS -->

Two shapes are admitted in v1.

### 2.1 Counted Completion
<!-- txdoc:COMP-2-1-COUNTED-COMPLETION -->

A counted completion starts with zero available completion credits.

```rust
struct Completion {
    done_count: AtomicU32,
    channel: Channel,
}
```

Rules:

- `complete()` adds one completion credit and wakes at least one waiter.
- `wait(protocol)` returns when it can consume one completion credit, or returns `Interrupted`, `Killed`, or `TimedOut` per `WaitProtocol`.
- repeated `complete()` calls add repeated credits.
- default completion is not broadcast. Broadcast or latch semantics require a distinct type name such as `BroadcastCompletion` or `LatchCompletion`.

### 2.2 Countdown Completion
<!-- txdoc:COMP-2-2-COUNTDOWN-COMPLETION -->

A countdown completion represents "N participants must finish."

```rust
struct CountdownCompletion {
    remaining: AtomicU32,
    channel: Channel,
}
```

Rules:

- `arrive()` decrements `remaining`.
- the transition `remaining: 1 -> 0` fires the private channel.
- `wait(protocol)` returns when `remaining == 0`.
- underflow is a bug.
- adding participants after exposure requires a separate reviewed type; v1 countdowns are closed after construction.

`ProcessPayload.group_exit` is the motivating instance: non-initiator threads decrement `remaining_threads`; the final decrement wakes the initiator.

---

## 3. Wake Is Not Truth
<!-- txdoc:COMP-3-WAKE-IS-NOT-TRUTH -->

The private channel only makes waiting efficient. It does not authorize progress.

Waiters must re-read the completion condition after wake:

```rust
loop {
    if completion.try_consume(Ordering::Acquire) {
        return WaitOutcome::Ready;
    }

    reactor::wait(
        completion.channel(),
        CompletionMask::DONE,
        protocol,
    ).await?;
}
```

An implementation may fuse subscribe/recheck/sleep into a lower-level wait primitive, but the semantic shape must remain:

```text
check condition -> arm wait -> sleep -> wake -> recheck condition
```

This is the same discipline as `BUS_v1`: wake is a hint; the condition is the truth.

---

## 4. Ordering
<!-- txdoc:COMP-4-ORDERING -->

The completing side must publish all data guarded by the completion before making the completion visible.

Minimum ordering:

- producer writes result state;
- producer performs `done.store(true, Release)` or `remaining.fetch_sub(1, Release)` whose `1 -> 0` transition completes;
- producer wakes the private channel after the release transition;
- waiter observes `done == true` or `remaining == 0` with Acquire before reading result state.

The wake may be coalesced, delayed, or spurious. The acquire read of the condition is what pairs with the producer's release.

---

## 5. API Sketch
<!-- txdoc:COMP-5-API-SKETCH -->

Exact Rust spelling is implementation-layer, but these roles are load-bearing:

```rust
    pub struct Completion { /* opaque */ }

impl Completion {
    pub fn new() -> Self;
    pub fn complete(&self);
    pub fn try_consume(&self) -> bool;
    pub async fn wait(&self, protocol: WaitProtocol) -> WaitOutcome;
}

pub struct CountdownCompletion { /* opaque */ }

impl CountdownCompletion {
    pub fn new(count: NonZeroU32) -> Self;
    pub fn arrive(&self);
    pub fn is_complete(&self) -> bool;
    pub async fn wait(&self, protocol: WaitProtocol) -> WaitOutcome;
}
```

`wait()` is sugar over the reactor wait primitive. It must not inspect subsystem structure directly. If a caller needs semantic revalidation beyond the completion's own condition, it waits for completion and then calls the owning subsystem's `checks/` or `execution/` API under a fresh guard.

---

## 6. Proper Uses
<!-- txdoc:COMP-6-PROPER-USES -->

Use a completion for:

- thread-group collapse completion;
- helper-task completion where the result is stored elsewhere;
- one-shot initialization barriers after boot phases that have entered reactor time;
- synchronous-looking script waits over private internal work;
- countdown rendezvous with a closed participant set.

Prefer a raw `wait_event` when the condition is subsystem semantic readiness, such as pipe readable, socket writable, process exited, mount detached, or page available.

Prefer `RawQueue` / `RawPort` declarations when external observers subscribe to state transitions.

Prefer a subsystem-specific waitqueue or handoff primitive when ownership, priority inheritance, lock transfer, or one-waiter semantics are part of the protocol.

---

## 7. Non-Goals
<!-- txdoc:COMP-7-NON-GOALS -->

A completion is not:

- an eventfd/signalfd replacement;
- a POSIX signal delivery mechanism;
- a lock;
- a condition variable with arbitrary predicates;
- an rt-mutex handoff;
- a bus primitive;
- a semantic state store;
- a way to avoid re-observation after wake.

---

## 8. Relationship to EXC-3
<!-- txdoc:COMP-8-RELATIONSHIP-TO-EXC-3 -->

`EXC-3` excludes single-waiter handoff from the publication catalog. Completion does not violate this exclusion because completion does not transfer ownership to a selected waiter.

The distinction:

| Mechanism | Meaning |
|---|---|
| Completion | "A condition became true; waiters may continue after recheck." |
| Single-waiter handoff | "This waiter receives ownership/control/priority-sensitive state." |

If a proposed completion needs to choose a particular waiter, donate priority, transfer a lock, or maintain owner identity, it is not a completion. It is subsystem synchronization and needs its own spec.

---

## 9. Invariants
<!-- txdoc:COMP-9-INVARIANTS -->

**COMP-1.** A completion is middleware: it packages a condition and private wake carrier, but does not define subsystem truth.

LINT: completion modules may import reactor wait and bus carrier types; they must not import subsystem `structure/`.

**COMP-2.** Completion wake does not grant truth. Waiters re-read the completion condition after every wake.

LINT: completion wait APIs must contain or call a condition recheck loop.

**COMP-3.** Counted completion consumes one credit per successful waiter unless the type is explicitly named as broadcast/latch.

LINT: default completion waits must use a consuming check; broadcast-style APIs require a distinct type/name.

**COMP-4.** Producer data is release-published before completion is made observable; waiters acquire-observe completion before reading producer data.

LINT: completion state transitions require release/acquire ordering or a stronger reviewed synchronization primitive.

**COMP-5.** Countdown completions have a closed participant count after construction.

LINT: no public `add_participant` on `CountdownCompletion` in v1.

**COMP-6.** Completion is not handoff. Any mechanism selecting a single waiter for ownership transfer is outside completion and falls under `EXC-3`.

LINT: completion APIs cannot expose selected-waiter identity, priority donation, or lock ownership transfer.

---

## 10. Review Questions
<!-- txdoc:COMP-10-REVIEW-QUESTIONS -->

1. Is the waited-for condition simple, fixed, and local to the completion object?
2. Are all producer writes visible before completion is observed?
3. Does every waiter recheck the condition after wake?
4. Is the participant set closed if this is a countdown?
5. Does the mechanism avoid transferring ownership to a selected waiter?
6. Would a raw `wait_event` or a subsystem-specific waitqueue be more honest?

If any answer is uncertain, do not introduce a completion yet; classify the protocol first.
