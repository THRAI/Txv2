# The Bus

<!-- txdoc:01-SUBSTRATE-BUS-V1 -->

**Status.** v1 (2026-04-19).

**Purpose.** Specify the bus layer: the three publication primitives (RawQueue, RawPort, RawTrace), their static/temporal decomposition, wire declaration and subscription semantics, and the APIs subsystems and the wait primitive use to fire and subscribe. The bus is the publication plane's mechanism; this document describes what it provides and what it does not.

**Audience.** Subsystem authors declaring signal attachments, wait-primitive implementers, lint-infrastructure authors enforcing SIG-* rules.

**Companion documents.**

- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — SIG-* rules this document implements; BIF-5 (single-carrier attachment) and SCRIPT-* isolation relate.
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — bus static/temporal layering at the vocabulary level.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — the publish phase of the in-step commit discipline invokes bus primitives.
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) — per-subsystem publication catalog.

### Zone-derived type policy
<!-- txdoc:BUS-ZONE-DERIVED-TYPE-POLICY-1 -->

The bus is substrate publication machinery, not a semantic entity owner:

| Bus declaration | Public handle | Reclamation role |
|---|---|---|
| wire attached to an entity | field on the owning identity or payload type | inherits the owner's retention domain |
| subscription edge | bus-managed subscription token | temporal/static bus state, not addressability |
| wake carrier | reactor-facing wake primitive | wake delivery, not truth |
| tracepoint | static declaration plus temporal fire | diagnostic publication, no entity |

Bus code may be embedded in zone-backed entities, but it does not create
`Cap<Wire>` or choose zone reclamation policies. The owning subsystem's
zone-derived type manifest decides where each wire lives.

---

## 1. The three primitives
<!-- txdoc:BUS-THE-THREE-PRIMITIVES-1 -->

The bus layer provides three primitives, distinguished by their notification semantics. The set is closed (ARCH-3); proposals for a fourth primitive require revisiting the invariants that determined this membership.

| Primitive | Semantics | Subscribers | Consumption | Typical use |
|---|---|---|---|---|
| RawQueue | Level-triggered readiness | Multi-subscriber | Non-consuming | Pipe readable, socket writable, timerfd ready |
| RawPort | Edge-triggered events | Multi or single | Possibly consuming | Process exit, ptrace stops, mount teardown |
| RawTrace | Passive recording | Consumers optional | Fire-and-forget | Tracepoints, observability hooks |

Each primitive has a distinct role in the publication plane. Mechanisms that do not fit any of the three are either subsystems (e.g., futex's hash-indexed waitqueue, rt_mutex's priority-ordered owner-tracking waitqueue) or carve-outs (EXC-1, EXC-2, EXC-3).

### 1.1 When to use which
<!-- txdoc:BUS-THE-THREE-PRIMITIVES-WHEN-TO-USE-WHICH-1 -->

**RawQueue — when readiness is continuous and multi-observer.** The question subscribers ask is "is condition X currently true?" Subscribers are independent; one subscriber consuming the signal does not affect others. Stale wakes are fine because truth lives in the predicate, not the wire (SIG-1, SIG-2).

**RawPort — when the event is edge-triggered and may have specific consumers.** The question is "did event Y happen?" Events may accumulate, may be consumed (e.g., `waitpid` consumes a child's exit status), and may have specific observer contracts (ptrace's tracer is privileged over ordinary observers).

**RawTrace — when the publication is purely diagnostic.** Tracepoint fires record that an event occurred but do not affect control flow. Subscribers are optional; the tracepoint may be nop-patched out entirely when no subscribers exist.

### 1.2 Not-a-primitive examples
<!-- txdoc:BUS-THE-THREE-PRIMITIVES-NOT-A-PRIMITIVE-EXAMPLES-1 -->

Mechanisms that look bus-shaped but belong elsewhere:

- **Futex waitqueue.** Address-keyed (not entity-attached). The waitqueue is subsystem-owned state with compare-before-sleep semantics; the wake mechanism is called from userspace via syscall, not from kernel commit. Specified in the futex subsystem.
- **rt_mutex waiter list.** Priority-ordered, owner-tracked, with inheritance propagation. A structured waitqueue requiring semantic state (who holds the lock, who's waiting with what priority, which chains need re-propagation). Subsystem, not primitive.
- **TLB shootdown ack.** Synchronous coordination where the issuer blocks on acknowledgment. Per EXC-2, not a publication plane mechanism; specified in the reactor's synchronous coordination API.
- **Fault AST.** Thread-local signal injection at trap-return. Per EXC-1, not a bus publication.

---

## 2. Static and temporal layering
<!-- txdoc:BUS-STATIC-AND-TEMPORAL-LAYERING-1 -->

The bus decomposes into two layers with distinct responsibilities.

### 2.1 Static bus
<!-- txdoc:BUS-STATIC-AND-TEMPORAL-LAYERING-STATIC-BUS-1 -->

**What wires exist and who subscribes.** Compile-time-rooted declarations; type-checked wire identities; subscription graph manipulation.

The static bus is about structure: a capability declares what it publishes, subscribers register their interest, the subscription graph records the current registrations. Nothing fires at this layer; nothing wakes.

Static bus APIs are cold-path: they execute at capability construction/destruction, at `EPOLL_CTL_ADD`/`EPOLL_CTL_DEL`, and at subscription lifecycle boundaries. Hot-path code (step execution, signal firing, wake delivery) does not execute static-layer logic.

### 2.2 Temporal bus
<!-- txdoc:BUS-STATIC-AND-TEMPORAL-LAYERING-TEMPORAL-BUS-1 -->

**Runtime fire-and-wake.** The hot path; minimal.

The temporal bus fires transitions when steps publish, delivers wakes to registered subscribers, maintains per-carrier ordering (SIG-5). It consults the subscription graph maintained by the static layer but does not modify it.

Temporal bus APIs are hot-path: every mutating step's publish phase invokes them. Minimality is a design goal — each fire operation should reduce to a small, predictable instruction sequence.

### 2.3 Why the split matters
<!-- txdoc:BUS-STATIC-AND-TEMPORAL-LAYERING-WHY-THE-SPLIT-MATTERS-1 -->

Three structural benefits:

**(a) Epoll and poll/select are static-layer queries.** At `EPOLL_CTL_ADD`, the epoll subsystem records an interest mask against a capability's declared wires. This is a pure structural operation: type-check the interest against the declaration, insert into the epoll's internal subscription graph, return. No runtime bus state is read or modified.

**(b) The temporal layer stays small and disciplined.** Confined to fire-and-wake, it is easier to reason about for latency (bounded per fire), for correctness (per-carrier ordering is enforced at the primitive), and for cross-CPU coherence (wake propagation is a well-defined event).

**(c) Invariants split cleanly between layers.** SIG-1 (signals are not truth), SIG-2 (wires are not state), SIG-4 (publish after write), SIG-5 (per-carrier ordering) apply to the temporal layer. Static-layer properties — wire declarations are fixed at capability-type definition, subscription does not imply temporal liveness — are encoded in the static bus's type system rather than stated as behavioral invariants.

---

## 3. Wire declarations (static layer)
<!-- txdoc:BUS-WIRE-DECLARATIONS-STATIC-LAYER-1 -->

Wire declarations are compile-time-rooted. A capability type declares which wires it publishes by associating mask types, event types, or tracepoint types with the capability.

### 3.1 Readiness declarations
<!-- txdoc:BUS-WIRE-DECLARATIONS-STATIC-LAYER-READINESS-DECLARATIONS-1 -->

A capability that publishes level-triggered readiness declares its readiness bit set:

```
readiness! {
    PipeReadEnd {
        has_data  => POLLIN,
        broken    => POLLHUP | POLLERR,
    }
}
```

This declaration establishes:

- `PipeReadEnd` exposes a RawQueue wire.
- The wire has two named transitions: `has_data` and `broken`.
- Each transition maps to a POSIX poll mask bit or combination.
- The wire's type is `ReadinessMask<PipeReadEnd>`, a type-level encoding of its bit set.

The declaration is fixed at the capability-type definition site. Runtime code cannot add or remove wires from `PipeReadEnd` (the static-monotone property: wire sets may grow across API revisions, never shrink within one).

### 3.2 Lifecycle event declarations
<!-- txdoc:BUS-WIRE-DECLARATIONS-STATIC-LAYER-LIFECYCLE-EVENT-DECLARATIONS-1 -->

A capability that publishes edge-triggered lifecycle events declares its event type:

```
lifecycle! {
    ProcessLifecycle {
        exited   => WEXITED,
        stopped  => WSTOPPED,
        continued => WCONTINUED,
    }
}
```

The pattern mirrors readiness but with different semantics: events fire once per occurrence, may have consumption semantics, and typically carry payload (exit status, stop signal).

### 3.3 Tracepoint declarations
<!-- txdoc:BUS-WIRE-DECLARATIONS-STATIC-LAYER-TRACEPOINT-DECLARATIONS-1 -->

A capability or subsystem that publishes tracepoints declares them separately:

```
tracepoint! {
    vfs {
        open(path: &Path, ino: u64, flags: u32),
        unlink(parent: u64, name: &Name),
        rename(from_parent: u64, from_name: &Name, to_parent: u64, to_name: &Name),
    }
}
```

Tracepoints carry structured payload; the declaration specifies parameter types. Tracepoint publication is nop-patched when no subscribers exist, incurring near-zero cost at un-subscribed sites.

### 3.4 Declaration-firing type safety
<!-- txdoc:BUS-WIRE-DECLARATIONS-STATIC-LAYER-DECLARATION-FIRING-TYPE-SAFETY-1 -->

The declaration types enable compile-time safety for signal firing. Firing a mask bit not in the declared set is a type error:

```
// at publish phase in step_write_commit:
pipe.read_source.fire(ReadinessMask::<PipeReadEnd>::has_data());  // OK
pipe.read_source.fire(ReadinessMask::<PipeReadEnd>::writable());  // compile error
//                                             ^^^^^^^^
//                      error: not a variant of PipeReadEnd's readiness
```

This removes a class of bugs at compile time and provides the static type information needed for epoll's interest-set construction.

### 3.5 Wire instances vs declarations
<!-- txdoc:BUS-WIRE-DECLARATIONS-STATIC-LAYER-WIRE-INSTANCES-VS-DECLARATIONS-1 -->

A declaration establishes the wire's *type*; each capability instance carries its own wire *instance* (storage for current state plus subscriber list). Distinct pipes have distinct `RawQueue` instances but share the `ReadinessMask<PipeReadEnd>` declaration type.

The instance lives inside the capability's structure; its lifetime follows the capability's. Reclamation of the capability (per object_model §6) finalizes the wire instance after its subscribers have released.

### 3.6 Current implementation checkpoint
<!-- txdoc:BUS-WIRE-DECLARATIONS-STATIC-LAYER-CURRENT-IMPLEMENTATION-CHECKPOINT-1 -->

The first Rust static-layer slice exposes typed declarations as wrappers over
the raw temporal carriers:

- `WireEventSet` names the declared event/readiness bit set for one wire type.
- `bus_event_set!`, `bus_readiness!`, and `bus_lifecycle!` generate first-slice
  `WireEventSet` bit newtypes with a declared-bit union, `from_bits`, `bits`,
  `contains`, and bitwise composition. Subsystem examples can use these helpers
  instead of hand-writing boilerplate bit-set wrappers.
- `WireDeclaration<E>` records the wire name, carrier kind, and declared bit
  set.
- `DeclaredQueue<E>` and `DeclaredPort<E>` wrap `RawQueue` and `RawPort`,
  validate fired bits and subscription interests against the declaration, and
  preserve the raw terminal/unsubscribed behavior.
- `WireRetirement` records the epoch guard, CPU, terminal bits, and wake count
  for a queue/port destruction handshake.
- `StaticRawQueue` and `StaticRawPort` are const-constructible backing storage
  for device/block tables; `raw()` and `RawQueue::from_static` /
  `RawPort::from_static` produce cloneable handles to that static storage.
- `DeclaredQueue<E>::from_static` and `DeclaredPort<E>::from_static` combine
  static backing storage with typed declaration validation.
- `SubscriptionGraph<N>` is a bounded long-lived subscription owner for raw
  queue/port carriers. It stores queue/port subscription tokens, returns
  generation-checked keys, and supports update, remove, kind/state inspection,
  readiness consumption, bounded ready/terminal scans through
  `SubscriptionGraphReady`, and explicit graph clear teardown for epoll-style
  consumers.
- `DeclaredSubscriptionGraphKey<E>` and the declared graph helpers subscribe,
  update, inspect, remove, and consume readiness through `DeclaredQueue<E>` /
  `DeclaredPort<E>`, validating interests against the declared event set
  before delegating to the raw graph.
- `TracePayload`, `TraceDeclaration<P>`, `RawTrace<P>`, and
  `bus_tracepoint!` provide the first typed tracepoint payload surface.
  `emit(payload)` is currently a no-op when no trace runtime is attached, but
  call sites keep the payload type and trace declaration name.
- `WireOwnerRetireFence` bridges wire-level retire records to owner-level EBR.
  It proves all embedded wires for one owner were terminal-drained under the
  same guard before the caller queues the containing storage for
  epoch-delayed reclaim.
- `WireOwnerManifest` and `retire_wire_owner<T>()` provide the typed owner hook
  over that fence: the owner type supplies the complete embedded-wire retire
  sequence and typed reclaim callback, avoiding erased reclaim callbacks at
  semantic call sites. `bus_wire_owner_manifest!` generates the repetitive
  manifest implementation for owners whose embedded wires can be described as
  simple field retire calls.

The raw `u64` APIs remain available for substrate-internal compatibility and
raw reactor internals. New subsystem-facing code should prefer the declared
wrappers. The current declaration macros cover typed queue/port bit-set
wrappers, typed tracepoint payload structs, and simple owner-manifest
boilerplate; full trace subscriber registration, nop-patching, target-fd
reverse-index teardown, global epoll table integration, spill/fanout policy,
and concrete zone/device owner implementations remain later implementation
slices.

---

## 4. Subscription (static layer)
<!-- txdoc:BUS-SUBSCRIPTION-STATIC-LAYER-1 -->

Subscribers register interest on a wire instance with a mask (readiness) or filter (events) specifying which transitions they care about.

### 4.1 Subscription APIs
<!-- txdoc:BUS-SUBSCRIPTION-STATIC-LAYER-SUBSCRIPTION-APIS-1 -->

```
trait Subscribable {
    type Interest;
    fn subscribe(&self, interest: Self::Interest, waker: Waker) -> Subscription;
    fn unsubscribe(&self, sub: Subscription);
}
```

`subscribe` records a (waker, interest) pair against the wire and returns an opaque `Subscription` handle. `unsubscribe` removes the registration.

Interest values are type-checked against the wire's declaration. For RawQueue, `Interest` is a bit mask drawn from the declared bit set. For RawPort, `Interest` is an event filter drawn from the declared event variants.

### 4.2 The subscription graph
<!-- txdoc:BUS-SUBSCRIPTION-STATIC-LAYER-THE-SUBSCRIPTION-GRAPH-1 -->

The aggregate of all active subscriptions across all wires is the subscription graph. Each wire holds its local subscriber list (typically as inline storage for small counts plus spill-to-heap for larger); each subscription references its wire.

The graph is modified only through `subscribe`/`unsubscribe` calls. Hot-path code (step publish, wake delivery) reads the graph but does not modify it.

Current Rust code exposes a bounded `SubscriptionGraph<N>` checkpoint for
long-lived queue/port subscriptions. It owns the returned subscription tokens
so consumers such as epoll can keep registrations alive across waits, uses
generation-checked handles to reject stale removals/updates after slot reuse,
delegates actual wake delivery to the existing raw carrier storage, and exposes
declared helper methods over `DeclaredQueue<E>` / `DeclaredPort<E>` so
subsystem-facing graph users do not erase typed interests to raw masks. It also
has a bounded `collect_ready(&mut [SubscriptionGraphReady])` scan for
ready/terminal entries and `clear()` for explicit epoll-fd close teardown. This
is not the final global epoll table yet: target-fd reverse indexes, spill
storage, fanout policy, and fd/owner integration remain separate slices.

### 4.3 Subscription lifecycle
<!-- txdoc:BUS-SUBSCRIPTION-STATIC-LAYER-SUBSCRIPTION-LIFECYCLE-1 -->

Subscriptions have well-defined lifetimes tied to their consumers:

- **Wait primitive subscriptions** are created when the wait primitive parks on a wire, destroyed on wake or cancel. Typically sub-microsecond scope.
- **Epoll subscriptions** are created at `EPOLL_CTL_ADD`, destroyed at `EPOLL_CTL_DEL`, at epoll-fd close, or at target-fd close. Long-lived.
- **Tracepoint subscriptions** are created at tracing-session setup, destroyed at session teardown.

Subscriber-side cleanup on wire destruction: if the capability holding a wire is reclaimed while subscribers remain, the wire fires a synthetic "gone" notification (typically POLLHUP on RawQueue) and invalidates the subscriptions. Subscribers holding subscriptions must tolerate the wire becoming unsubscribable (see §6.2).

### 4.4 Subscription does not imply delivery
<!-- txdoc:BUS-SUBSCRIPTION-STATIC-LAYER-SUBSCRIPTION-DOES-NOT-IMPLY-DELIVERY-1 -->

Being registered on a wire does not guarantee notification delivery when the wire fires. Several mechanisms may drop or coalesce notifications:

- **Level-triggered wires coalesce.** If the wire's state transitions true→true (already true), no new wake fires. If it transitions true→false→true rapidly, only one wake may be delivered.
- **Subscribers may miss wakes while already woken.** A subscriber whose waker was already fired but has not yet re-observed may not receive a subsequent wake until it cycles through the waiter contract.
- **Epoch-deferred wake.** Wakes may be deferred to avoid thundering-herd; a wake fired now may be delivered slightly later.

The waiter contract handles all of these by construction: wake → re-observe → act (SIG-1). A subscriber that relies on "I am registered, therefore I will be woken on every transition" is buggy; such code would fail even with perfectly reliable delivery when the wire coalesces.

This subscription-does-not-imply-liveness property is what justifies the permissiveness of the wake-delivery system. The bus is free to drop, coalesce, or defer notifications for any reason; correctness is preserved because the waiter must re-observe anyway.

---

## 5. Firing (temporal layer)
<!-- txdoc:BUS-FIRING-TEMPORAL-LAYER-1 -->

When a mutating step completes a linearizing write, its publish phase invokes the bus primitives to fire transitions on the relevant wires.

### 5.1 RawQueue fire API
<!-- txdoc:BUS-FIRING-TEMPORAL-LAYER-RAWQUEUE-FIRE-API-1 -->

```
impl RawQueue<M: ReadinessMask> {
    /// Set one or more bits; if any bit goes 0→1, wake subscribers
    /// whose interest includes that bit.
    /// Called from a step's publish phase, after the state write.
    fn fire(&self, bits: M);

    /// Clear one or more bits. Called when the state that was
    /// published as "ready" ceases to hold.
    fn clear(&self, bits: M);

    /// Current bit state; diagnostic only (SIG-2: not state).
    fn peek(&self) -> M;
}
```

The `fire` operation is the primary hot-path call. Its implementation:

1. Atomic `fetch_or` to set the bits.
2. Detect which bits are new (were 0, are now 1).
3. For each newly-set bit, wake subscribers whose interest mask includes that bit.

The fetch-or is the linearization point of the publication: prior writes from the calling step are visible to any observer that sees the new bit, thanks to release-acquire ordering on the atomic (SIG-4).

`clear` is symmetric but does not wake. A transition from ready to not-ready is not a wake event; only the reverse is.

`peek` exists for diagnostic purposes (debugger views, `/proc` readers). Per SIG-2, no correctness reasoning may depend on the value returned by `peek`; truth lives in the predicate, not the wire.

### 5.2 RawPort fire API
<!-- txdoc:BUS-FIRING-TEMPORAL-LAYER-RAWPORT-FIRE-API-1 -->

```
impl RawPort<E: EventType> {
    /// Fire an event. Delivers to subscribers whose filter admits it.
    fn fire(&self, event: E);

    /// No event-state to peek; edge-triggered.
}
```

Each `fire` is an independent event delivery. Events are not accumulated as bits; each fire produces a distinct wake. Subscribers with consuming semantics (waitpid) consume events; subscribers with observing semantics (ptrace peek, fanotify) see events without consuming.

The event type `E` is declared by the `lifecycle!` macro and typechecked at fire time per §3.4.

### 5.3 RawTrace fire API
<!-- txdoc:BUS-FIRING-TEMPORAL-LAYER-RAWTRACE-FIRE-API-1 -->

```
impl RawTrace<P: TracePayload> {
    /// Record an event with payload. No waiters.
    fn emit(&self, payload: P);
}
```

When no tracepoint subscribers exist (the common case), `emit` is nop-patched at the call site: the instruction is replaced with a `nop` on RISC-V / LoongArch. When a subscriber attaches, the nop is patched to a jump to the trace handler.

No wake delivery; no state. Tracepoints are purely passive.

Current Rust code exposes the typed tracepoint payload shape as
`TracePayload`, `TraceDeclaration<P>`, `RawTrace<P>`, and `bus_tracepoint!`.
`RawTrace<P>::emit(payload)` is intentionally no-op until the trace runtime and
nop-patching layer exists; it exists now so subsystem code can declare and call
typed tracepoints without inventing a parallel raw API.

### 5.4 Ordering guarantees (SIG-4, SIG-5)
<!-- txdoc:BUS-FIRING-TEMPORAL-LAYER-ORDERING-GUARANTEES-SIG-4-SIG-5-1 -->

Per SIG-4, the fire call happens-after the state write it publishes. Per SIG-5, per-carrier ordering holds: subscribers on one wire observe wakes in the order the fires occurred.

Cross-carrier ordering is not guaranteed. Fire-A on wire-A happens-before fire-B on wire-B in wall-clock time does not imply subscribers will observe them in that order if they are watching both wires. This is consistent with per-carrier ordering: each wire is independently consistent, but the bus makes no global serialization claim.

### 5.5 Fire call sites
<!-- txdoc:BUS-FIRING-TEMPORAL-LAYER-FIRE-CALL-SITES-1 -->

Per SIG-6, signal firing is not a substrate responsibility. The substrate primitives (`zone::sign`, `index::commit`, `credit::commit`) perform their mutations; the step's code then invokes bus primitives to publish. These are separate calls, separately linted:

```rust
// inside a step's publish phase (illustrative):
substrate::index::commit(container, key, evidence);    // the write
pipe.read_source.fire(ReadinessMask::has_data());          // the publish
```

A lint flags bus-primitive calls outside step commit code (SIG-6): checks/, structure/, projection/ may not call fire. Only execution code inside step functions' publish phase may.

---

## 6. Wake delivery (temporal layer)
<!-- txdoc:BUS-WAKE-DELIVERY-TEMPORAL-LAYER-1 -->

When a wire fires, the temporal bus delivers wakes to matching subscribers.

### 6.1 Wake mechanics
<!-- txdoc:BUS-WAKE-DELIVERY-TEMPORAL-LAYER-WAKE-MECHANICS-1 -->

A wake delivery consists of:

1. Identify matching subscribers (those whose interest includes the fired transition).
2. For each, invoke the subscriber's waker.
3. The waker marks the waiting task as ready to poll.
4. The reactor, on its next scheduling pass, polls the task.

The subscription graph is walked under an epoch guard or equivalent protection so that concurrent unsubscribe operations do not race with wake delivery. A subscription that is being unsubscribed concurrently may or may not be woken; either outcome is acceptable because the waiter contract ensures the subscriber re-observes on wake.

Wakers themselves are the reactor-provided primitive. The bus does not know what a task is; it knows what a waker is (a function pointer plus context). Task scheduling is the reactor's concern.

### 6.2 Wire destruction
<!-- txdoc:BUS-WAKE-DELIVERY-TEMPORAL-LAYER-WIRE-DESTRUCTION-1 -->

When a capability is reclaimed while subscribers remain registered:

1. The bus fires a synthetic terminal notification. For RawQueue, this is typically `POLLHUP | POLLERR` (whichever the wire declared). For RawPort, it is a declared `Gone` variant if the declaration includes one, otherwise the subscription list is drained silently.
2. The subscription graph is walked; each subscriber is woken with the terminal notification.
3. The wire's subscription list is emptied.

On the subscriber side, a wake that arrives with a terminal notification is observable per the wire's declaration: a RawQueue subscriber sees POLLHUP and adjusts its behavior (a waiting reader returns EOF or broken-pipe); a RawPort subscriber sees the Gone event or observes its subscription become invalid.

This handshake closes the reclamation race: the wire is not reclaimed until its subscription list is drained, which happens after all subscribers have been woken with the terminal signal.

The current Rust checkpoint exposes this as `retire(..., &epoch::Guard)` and
`retire_silently(&epoch::Guard)` on `RawQueue`, `RawPort`, `DeclaredQueue<E>`,
and `DeclaredPort<E>`. These calls require the caller to hold an epoch guard,
terminate the wire, drain subscribers, and return a `WireRetirement` record
that captures the guard epoch and CPU used for the handshake. The current
backing storage is still `Arc`-owned raw wire state; embedding wires directly
in zone/device owner storage additionally uses `WireOwnerRetireFence`: the
owner retires every embedded wire under one guard, combines the returned
`WireRetirement` records, then calls the fence's unsafe owner-storage retire
hook to enqueue the containing storage through EBR. The unsafe boundary belongs
to the owner because only the owner knows which wire set is complete and which
typed reclaim callback returns its storage. The current typed hook is
`WireOwnerManifest` plus `retire_wire_owner<T>()`: callers pass a typed owner
pointer and guard, the manifest retires all embedded wires, and the helper
enqueues storage with the owner type's reclaim callback. Concrete VFS/device
owner manifests remain later subsystem work.

### 6.3 Wake delivery is best-effort
<!-- txdoc:BUS-WAKE-DELIVERY-TEMPORAL-LAYER-WAKE-DELIVERY-IS-BEST-EFFORT-1 -->

Consistent with §4.4: the bus may coalesce, defer, or drop wake deliveries under certain conditions. Concretely:

- **Already-ready subscriber.** If a subscriber's waker has been fired and not yet acknowledged (task is already scheduled to poll), subsequent wakes for that subscriber coalesce into the existing wake. Only one wake is delivered per "ready cycle."
- **Epoch-deferred wake.** Some wake deliveries may be batched to the next epoch advance, to avoid thundering-herd contention. The wake arrives, just possibly a few microseconds later than the fire.
- **Concurrent unsubscribe.** If the subscriber is in the process of unsubscribing when a fire occurs, the wake may be delivered to the now-departing subscriber or may be dropped. Either is correct.

These are all acceptable per SIG-1 and SIG-2. The waiter contract handles them.

---

## 7. Integration with the wait primitive
<!-- txdoc:BUS-INTEGRATION-WITH-THE-WAIT-PRIMITIVE-1 -->

The wait primitive (see CONCEPTS §9) composes bus subscription with condition-predicate re-evaluation. The integration pattern:

```
pseudocode (inside wait::wait_event):

loop {
    g = epoch::guard()
    if condition(&g) { return Ready }

    sub = wire.subscribe(interest, waker_for_current_task())
    if condition(&g) {
        wire.unsubscribe(sub)
        return Ready
    }
    drop(g)                        // release guard before sleep

    park_task(protocol)            // async suspend

    wire.unsubscribe(sub)
    // loop: acquire fresh guard, re-check condition
}
```

The wait primitive:

1. Checks the condition (predicate) under an epoch guard. If true, done.
2. Subscribes to the wire with the interest mask.
3. Re-checks the condition after registration, before parking.
4. Drops the guard and parks the task only if the predicate is still false.
5. On wake, unsubscribes and re-checks the condition.
6. Loops until the condition holds or the protocol's interrupt/timeout fires.

The subscribe happens *after* the first condition check, but it is not enough
by itself. `ASYNC-4` requires register-or-recheck safety: if the condition
changed between the first check and the subscription, the second check observes
it before the task parks. The re-check on wake then closes stale and spurious
wake cases.

This is the wait-side realization of "signals are not truth" (SIG-1): the subscribe enables sleep; the condition-check determines truth.

### 7.1 Subscription as sleep enabler
<!-- txdoc:BUS-INTEGRATION-WITH-THE-WAIT-PRIMITIVE-SUBSCRIPTION-AS-SLEEP-ENABLER-1 -->

The subscription's purpose is enabling sleep: without it, the task would have nothing to sleep *on*, and would have to poll. With it, the task can park efficiently, and the bus is responsible for waking it when something potentially worth re-checking happens.

This framing matches CONCEPTS's one-liner: "Signals make waiting efficient." The subscription and subsequent wake reduce polling to event-driven response, but the event itself does not authorize action.

The wait primitive realizes the subscriber-side obligations SIG-9 (wake does not grant truth; re-observe before acting) and SIG-10 (bus state is not truth; only predicate evaluation under a fresh guard is). Its structure — park on wake, re-invoke condition on resumption — is exactly what these invariants require. Subscribers that bypass the wait primitive (consuming wake notifications directly) must implement equivalent re-observation discipline; SIG-9 and SIG-10 are not optional.

Current Rust reactor code exposes this discipline through the raw
`tx_reactor::wait::Channel` / `Mask` compatibility path and through
`DeclaredChannel<E>` for typed declared-port waits over `DeclaredPort<E>` plus
`DeclaredReadinessChannel<E>` for typed readiness waits over `DeclaredQueue<E>`.
Subsystem-facing waits should prefer declared channels so the interest type
remains tied to the bus declaration. Concrete subsystem migrations and fd/epoll
graph policy remain later slices.

### 7.2 The step's Blocked outcome as wait-primitive input
<!-- txdoc:BUS-INTEGRATION-WITH-THE-WAIT-PRIMITIVE-THE-STEPS-BLOCKED-OUTCOME-AS-WAIT-PRIMITIVE-INPUT-1 -->

When a step returns `Blocked(carrier, interests)` or `AdvancedThenBlocked(progress, carrier, interests)`, the driver invokes the wait primitive with:

- `wq` = carrier
- `condition` = a closure that re-observes the step's readiness (typically by calling a subsystem-provided predicate)
- `protocol` = driver's configured wait protocol (Interruptible, Killable, Timed)

The wait primitive then executes §7's pseudocode. On return (`Ready` or interrupt/timeout), the driver loops and re-invokes step.

The subsystem does not need to provide a separate condition closure for the wait: in the common case, the next step invocation's initial predicate check serves this role. Some implementations may optimize by providing a dedicated lightweight condition function that matches what the step would check first; this is an optimization, not a structural requirement.

---

## 8. Module layout
<!-- txdoc:BUS-MODULE-LAYOUT-1 -->

Under the static/temporal split, the bus module structure is:

```
bus/
    static_layer/
        declarations.rs      // readiness!, lifecycle!, tracepoint! macros
        mask.rs              // mask types, type-level bit set machinery
        event.rs             // event types, filter machinery
        subscription.rs      // Subscription handle, subscription-graph ops
        pub use ...

    temporal_layer/
        queue.rs             // RawQueue implementation
        port.rs              // RawPort implementation
        trace.rs             // RawTrace implementation (nop-patch support)
        wake.rs              // waker delivery, epoch coordination
        pub use ...

    mod.rs                   // re-exports; client code imports from here
```

Clients of the bus (subsystems, wait primitive, epoll) import from `bus::` top-level paths. The internal static/temporal distinction is implementation structure, not API.

Current Rust code is split as a smaller implementation checkpoint under
`crates/tx-substrate/src/bus/`:

- `mod.rs` is the public facade.
- `common.rs` owns shared declaration types, errors, raw storage selection,
  spin locking, and `WireRetirement`.
- `queue.rs` owns `RawQueue`, `StaticRawQueue`, `DeclaredQueue<E>`, and their
  subscriptions.
- `port.rs` owns `RawPort`, `StaticRawPort`, `DeclaredPort<E>`, and their
  subscriptions.
- `graph.rs` owns the bounded `SubscriptionGraph<N>` long-lived subscription
  owner, generation-checked keys, ready/terminal scan records, and explicit
  graph teardown.
- `owner.rs` owns the `WireOwnerRetireFence` bridge from wire terminal/drain
  records to owner-storage EBR retirement plus the `WireOwnerManifest` typed
  owner hook.
- `trace.rs` owns `TracePayload`, `TraceDeclaration<P>`, and `RawTrace<P>`.
- `macros.rs` owns the first declaration macros that generate typed
  `WireEventSet` bit newtypes for declared queues/ports and typed tracepoint
  payload structs, plus simple `WireOwnerManifest` implementations.

This split is behavior-preserving and keeps authored Rust files below the
repo's source-size lint while the final static/temporal subdirectory shape is
still growing.

---

## 9. Lints
<!-- txdoc:BUS-LINTS-1 -->

Lints that enforce bus-related invariants:

- **SIG-6:** Calls to `fire`, `clear`, `emit` are permitted only from functions within `execution/*/step_*.rs` publish phases. Flagged call sites in `checks/`, `structure/`, `project.rs`, or outside execution altogether.
- **SIG-4:** Within a step, a fire call on a wire follows a substrate primitive call on the corresponding state. Detected by AST analysis: a fire on wire `W` attached to entity `E` must follow a substrate commit on some field of `E` within the same basic block (or demonstrate it via a comment escape hatch for unusual patterns).
- **Static declaration integrity:** Mask or event types appearing in a `fire` call must match a declaration on the capability's type. This is a type-system check (declaration produces a type; fire is generic over declared types) rather than a lint, enforcing the property at compile time.
- **BIF-5, SIG-7:** A capability type's declarations must correspond to a single retention domain. A split entity (Identity/Payload) may declare wires on both, but each declaration attaches to one. Flagged if a single declaration bundles wires from both domains.
- **ARCH-3:** New primitives (beyond RawQueue, RawPort, RawTrace) require architectural review. Flagged if new types implementing a bus-primitive-like API appear outside `bus/`.

---

## 10. What the bus does not do
<!-- txdoc:BUS-WHAT-THE-BUS-DOES-NOT-DO-1 -->

For clarity, the bus does not provide:

- **Sleeping primitives.** `park_task` and waker infrastructure are reactor-provided; the bus invokes wakers.
- **Timer services.** Timeouts and scheduled delays are reactor concerns; the bus does not schedule wake events.
- **Condition evaluation.** Predicates are subsystem code; the bus does not know what a condition means.
- **Wait-protocol policy.** Interruptibility, killability, cancellation — all wait-layer concerns.
- **Cross-node or cross-machine distribution.** The bus is a single-kernel-instance mechanism. Distributed coordination is out of scope.
- **Message passing.** The bus signals transitions; it does not carry semantic messages. Subscribers observe that a transition fired; the meaning is determined by re-observing the subsystem's state.

Attempts to add any of these to the bus would violate either its minimality (goal: small temporal-layer hot path) or its disciplined scope (goal: publication plane only, not execution or wait).

---

## 11. Interaction with carve-outs
<!-- txdoc:BUS-INTERACTION-WITH-CARVE-OUTS-1 -->

Per EXC-1, EXC-2, EXC-3 in `02_INVARIANTS_v5.md`, three mechanisms are normatively excluded from bus publication:

**EXC-1 (fault injection).** Thread-local. The signal subsystem's two-site model handles fault signals via the AST mechanism at trap-return path. The bus sees none of this.

**EXC-2 (cross-core barriers).** Synchronous reactor coordination. TLB shootdown issues IPIs; the issuing thread blocks until all targets have acked via the reactor's coordination API. Completion is a synchronous return, not a wake. The bus is not involved.

**EXC-3 (single-waiter handoff).** Mechanisms like rt_mutex's owner-tracked priority-ordered waitqueue, or similar subsystem-specific parking protocols. These have their own dedicated waitqueue structures with semantic state; they are not bus primitives.

A proposal that routes any of these through bus primitives is an EXC-* violation. The bus layer rejects it; the carve-out's dedicated mechanism handles it.

---

## 12. Relationship to prior patterns
<!-- txdoc:BUS-RELATIONSHIP-TO-PRIOR-PATTERNS-1 -->

For readers familiar with earlier iterations:

- **v12's `Pollable` trait** is retired. Its role (introspection of what a capability publishes) is now the static bus's wire-declaration query. A capability's static declaration is accessible via its type; no runtime trait dispatch is needed.
- **v12's WakerGuard-threading** through subsystem step functions is retired. The WakerGuard is internal to the wait primitive; subsystems return `Blocked` outcomes naming the carrier abstractly; the wait primitive handles subscribe/unsubscribe.
- **The retired bus-as-one-layer model** is gone. The static/temporal split is new in v3; it disentangles compile-time declaration from runtime fire-and-wake, making invariants easier to state and enforce.

---

## 13. Open questions
<!-- txdoc:BUS-OPEN-QUESTIONS-1 -->

**13.1. Nop-patching implementation.** RawTrace's nop-patching requires `fence.i` on RISC-V after patching to ensure instruction-cache coherence. The exact HAL interface for patching is deferred to HAL spec.

**13.2. Epoch coordination in wake delivery.** Wake delivery walks the subscription list; concurrent unsubscribe must be handled safely. The current design uses the same EBR guard used for structure traversal; whether this suffices under all access patterns requires detailed implementation review.

**13.3. Adaptive subscriber storage.** The current sketch uses "inline for small counts, spill to heap for larger." The inline/spill threshold and the spill data structure (linked list, array, hash) is an implementation choice. Performance measurements will inform.

**13.4. Cross-subsystem wire coordination.** When one subsystem's commit triggers firing on another subsystem's wire (e.g., VFS unlink firing fsnotify on a watched directory), the ownership and declaration pattern requires careful naming. Current approach: wires are always owned by the entity they fire on; cross-subsystem fires access the wire through the target capability. Details deferred.

---

## References
<!-- txdoc:BUS-REFERENCES-1 -->

- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) §6 (SIG-*), §10 (EXC-*), §2 (BIF-5).
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — bus static/temporal layering and wait primitive integration.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) §3.4 (publish phase), §3.5 (return).
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) §7 (import rules: bus/ restrictions).
- [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md) — per-subsystem catalog of wire usage.
- [`REACTOR_v0.md`](../02_execution/REACTOR_v0.md) — reactor-provided waker infrastructure.
