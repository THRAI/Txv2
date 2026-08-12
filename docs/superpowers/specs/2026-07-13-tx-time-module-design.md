# Tx Time Module Design

**Status:** approved design, implementation pending (2026-07-13)

## Purpose

Create a physical `tx-time` crate as the readable home for kernel time
capabilities, timekeeper state, timer-queue algorithms, VVAR contents, and
typed RTC adapters. The change preserves reactor ownership of the runtime timer
domain: a reactor constructs, holds, drives, and arms its concrete timer queue.
It does not move scheduler placement or task wake ownership into `tx-time`.

The local browser sketches used to discuss this design are not repository
artifacts and must not be added to a commit.

## Ownership

| Concern | Owner | Not owned here |
|---|---|---|
| Clock/timekeeper state | `tx-time` | scheduler, task state, VFS timestamps |
| Timer-queue algorithm | `tx-time` | mailbox, task owner, signal/device semantics |
| Concrete queue instance and due drive | `tx-reactor::ReactorTimerDomain` | queue algorithm internals |
| Task expiry routing | reactor/scheduler | deadline ordering algorithm |
| Linux timer semantics | timerfd, shims, subsystems | queue ordering and hardware arm |
| Counter/deadline/RTC hardware | `tx-hal` and board crate | timekeeper policy |
| VVAR layout and publication contents | `tx-time::vvar` | user mapping and auxv |
| VVAR/vDSO mapping into an address space | VM/exec | time calibration or publication |

`TimerWheel` is renamed conceptually to `TimerQueue`: the current implementation
is a shared `Vec<Entry>` scan, not a wheel. The first replacement is an indexed
min-heap following the .NET `TimerQueue` shape. A Linux-style hierarchical wheel
is a later implementation option only if measurements show a need for it.

## Crate Topology

```text
crates/tx-time/
  src/
    lib.rs
    api/
      clock.rs              # ClockRead / RealtimeControl
      deadline.rs           # DeadlineRegistrar / TimerGuard / TimerKey
      rtc.rs                # RtcDeviceOps
    keeper/
      state.rs              # monotonic + offset + generation
      convert.rs            # realtime <-> monotonic conversion
      set.rs                # clock-set/rebase publication
    timer/
      engine.rs             # key lifecycle and due batches
      queue.rs              # private TimerQueue algorithm contract
      min_heap.rs           # indexed min-heap implementation
      key.rs
      guard.rs
    vvar/
      layout.rs             # VVAR ABI snapshot
      publish.rs            # serialized publication
      calibrate.rs          # cycle-to-ns parameters
    rtc/
      device.rs             # HalRtcDevice
      alarm.rs
    hal.rs                  # HAL time adapters

crates/tx-reactor/src/
  time_domain.rs            # owns tx_time::timer::TimerEngine instance
  time_route.rs             # TimerKey -> task/signal/wait-source/device delivery
  hart_loop.rs              # invokes domain drive and hardware arm
```

`tx-time` must not import `tx-reactor`, scheduler types, or semantic subsystem
types. `tx-reactor` depends on `tx-time` and implements the runtime integration.
Upper consumers depend only on `tx_time` capability interfaces, never on a
concrete queue, reactor registry, hardware timer, or board RTC register.

## HAL Integration

`tx-time` is the layer immediately above the axHal-style static platform
family. It consumes only narrow capabilities from a compile-time selected
`P: TxPlatform`; it does not introduce a runtime HAL manager, boxed platform
object, board registry, or board-specific conditional logic.

| `tx-time` component | HAL capability | Direction and rule |
|---|---|---|
| `keeper` and VVAR calibration | `MonotonicCounterIf` | Read `read_ns()` and `frequency_hz()` to form monotonic time and VVAR contents. This is the hot read path. |
| `ReactorTimerDomain` arm adapter | `DeadlineTimerIf` | `tx-time::hal::HalDeadlineTimer<P>` implements the current-hart arm/cancel adapter. Only reactor driver code invokes it. |
| boot seed, writeback, devfs RTC | `PersistentClockIf` | `tx-time::rtc::HalRtcDevice<P>` implements typed `RtcDeviceOps`; it translates typed errors but does not add policy. |
| RTC IRQ registration | `IrqIf::RTC_IRQ` | Kernel trap/IRQ binding identifies the line. The typed RTC adapter acknowledges hardware before device-event publication. |

The downward data flow is:

```text
board crate implements P
        -> tx-hal exposes MonotonicCounterIf / DeadlineTimerIf /
           PersistentClockIf / IrqIf
        -> tx-time adapts the capabilities into timekeeper, VVAR, and typed RTC APIs
        -> reactor owns a TimerDomain instance and uses the current-hart arm adapter
```

The timer interrupt path is deliberately split. HAL/trap code classifies the
interrupt and performs minimal current-hart interrupt handoff. In normal reactor
context, `ReactorTimerDomain` reads monotonic `now`, drains due registrations,
routes them, and asks the HAL adapter to arm or cancel the next deadline. HAL
never sees a timer key, task, mailbox, VVAR page, or software queue.

## Time-Layer State Budget

`tx-time` is not a new global object graph. It owns only time-specific state:

- one timekeeper state: realtime offset, generation, calibration parameters,
  and VVAR publication state;
- queue algorithm types and per-instance state: the indexed min-heap, slots,
  and cancellation/generation metadata;
- typed RTC adapter values, which are normally zero-sized wrappers over `P`.

The concrete `TimerEngine` instance is not a process-wide `tx-time` singleton.
It is a field of `ReactorTimerDomain`, which also owns the route table, single
driver claim, deadline-change signal, and current-hart arm policy. `tx-time`
does not own process timer tables, timerfd state, VFS timestamp state, task
mailboxes, scheduler queues, signal state, device event queues, or board state.

## Interfaces

The public consumer contract remains capability-shaped:

```rust
pub trait DeadlineRegistrar {
    fn register_deadline(
        &self,
        deadline_ns: DeadlineNs,
        target: TimerTarget,
    ) -> Result<TimerGuard, TimeError>;

    fn rearm_deadline(
        &self,
        guard: &mut TimerGuard,
        deadline_ns: DeadlineNs,
        target: TimerTarget,
    ) -> Result<(), TimeError>;
}
```

The queue algorithm is private to `tx-time` and receives only opaque keys:

```rust
trait TimerQueue {
    fn insert(&mut self, key: TimerKey, deadline_ns: u64);
    fn remove(&mut self, key: TimerKey) -> bool;
    fn rearm(&mut self, key: TimerKey, deadline_ns: u64);
    fn drain_due(&mut self, now_ns: u64, out: &mut Vec<TimerKey>);
    fn next_deadline_ns(&self) -> Option<u64>;
}
```

`ReactorTimerDomain` owns an engine plus a private route table. The engine
contains deadline order and cancellation state; the route table maps a key to
the current task, signal, wait-source, or device delivery. The queue algorithm
therefore never stores `TaskMailbox`, raw queue, signal state, a hart, or a
scheduler queue.

`TimerGuard` carries only a stable cancellation capability and key. Cancelling
is idempotent. Rearming replaces the logical registration through a generation
transition, so a former expiry is stale rather than a second valid delivery.

## Delivery Routing

The queue algorithm routes nothing. After `TimerEngine` removes due keys under
its lock, `ReactorTimerDomain` looks up each key in its private `TimerRouteTable`
and invokes the existing reactor-owned delivery path outside the algorithm lock.
The current `TimerTarget` variants map as follows:

| Public registration intent | Private reactor route | Expiry result | Semantic owner after hint |
|---|---|---|---|
| task timeout | task mailbox route | `TimerFired` mailbox hint, owner recheck, local enqueue or remote IPI | script/future wait state |
| POSIX or interval signal timer | signal mailbox route | `SignalTimerFired` hint | POSIX/ITIMER table performs due scan and signal/overrun update |
| timerfd/readiness timeout | wait-source route | source readable notification | timerfd object updates expiration count and periodic rearm |
| delegate timeout | delegate route | delegate registry timeout transition | delegate state machine |
| RTC alarm emulation or device timeout | device route | typed callback or queue/source publication | device/devfs event state |

`TimerRouteTable` is the only place that may carry task-mailbox, wait-source,
or device delivery details for a timer key. The route table is reactor-private;
it is not part of `TimerQueue`, `TimerEngine`, or the public registrar API.
Task routes use the existing owner-aware router: upgrade the mailbox, post the
hint, resolve the current owner, lock and recheck the target queue, enqueue if
needed, and send a reschedule IPI only for a remote target. Timer registration
never caches the registration hart as routing truth.

## Runtime Flow

1. A semantic producer registers through `DeadlineRegistrar`.
2. `ReactorTimerDomain` assigns a key, records delivery privately, and inserts
   the key into its `TimerEngine`.
3. If registration changes the earliest deadline, the domain publishes a
   deadline-change signal so its driver hart reruns the timer step before idle.
4. The hardware timer interrupt only performs the platform interrupt handoff.
   The reactor hart loop runs `drive_due(now)` in normal context.
5. The engine removes due keys under its lock and returns a batch. The private
   route table then supplies task, signal, wait-source, delegate, or device
   delivery. Callback, publication, mailbox post, owner recheck, enqueue, and
   any remote IPI all occur after the algorithm lock is released.
6. The domain rereads the earliest live deadline and is the only path that
   invokes current-hart hardware arm/cancel.

For the shared global queue, `ReactorTimerDomain` has an explicit single-driver
claim. A later per-hart-shard implementation is allowed only after AP task
execution and contention evidence exist; it must provide target-shard insert
and remote reprogram semantics.

## Three Time Paths

- Clock read: `ClockRead -> timekeeper -> MonotonicCounterIf`. This path does
  not use the reactor timer domain. `CLOCK_REALTIME` is monotonic plus offset,
  not an RTC hot read.
- Deadline: sleep, futex timeout, timerfd, POSIX timers, interval timers, and
  reactor preemption use `DeadlineRegistrar -> ReactorTimerDomain -> TimerQueue
  -> HAL deadline timer`. Expiry returns to reactor routing; semantic owners
  recheck their own state.
- Persistent RTC: devfs and boot use `RtcDeviceOps -> HalRtcDevice ->
  PersistentClockIf`. A hardware alarm IRQ first acknowledges hardware and
  publishes a device event. It reaches poll/epoll through ordinary wait-source
  routing, not as a task deadline. Software RTC-alarm emulation may borrow the
  registrar with a device delivery target.

## VVAR and vDSO

`tx-time::vvar` owns VVAR layout, clock calibration, snapshots, and serialized
publication. `tx-vdso` retains user-mode fast-path assembly. VM/exec retains
the VVAR mapping and `AT_SYSINFO_EHDR` decision because mapping is address-space
and loader work, not timekeeper work. No vDSO assembly or VM mapping code is
moved merely because its payload is time data.

## Consumer Rules

Timerfd stores its clock, interval, expiration count, cancel-on-set generation,
and guard; expiry publishes its wait source. POSIX timers and `ITIMER_REAL`
store signal/overrun state and use expiry only as a due-scan hint. Futex and
epoll objects store wait/readiness state; their syscall waits may install a
temporary task deadline but must not gain a private queue. RTC stores device
event and alarm state. No semantic object stores a registrar handle or concrete
queue.

## Migration

1. Add `tx-time` with API and data types re-exported compatibly from the old
   service surface; no call site changes in this slice.
2. Move timekeeper, typed RTC adapters, and VVAR content/publication to
   `tx-time`; leave VVAR mapping and vDSO assembly in their existing homes.
3. Introduce `TimerEngine` and indexed min-heap behind private `TimerQueue`.
   Preserve tokens, guards, and existing registrar behavior with focused tests.
4. Move concrete queue instance, route table, driver claim, deadline-change
   signaling, and current-hart arm into `ReactorTimerDomain`.
5. Move timer routing out of the algorithm lock. Route task, signal,
   wait-source, and device deliveries after due-batch extraction.
6. Migrate old `wake::timer` callers to `tx-time` APIs; delete the scripts
   substrate registrar escape hatch after generic script deadlines use the
   stable registrar facade.
7. Retire the old timer module only after lints forbid its imports and focused
   time, reactor, timerfd, POSIX timer, RTC, and SMP tests pass.

## Verification

- Queue contract tests: earliest ordering, equal-deadline deterministic token
  order, cancel, rearm, stale expiry, and due-batch extraction outside routing.
- Reactor tests: deadline-change wake before idle, single-driver claim,
  current-hart arm/cancel, remote owner requeue, and one IPI for remote wake.
- Semantic tests: timerfd periodic/rebase/cancel-on-set, POSIX overrun,
  interval timer, futex timeout, epoll timeout, and RTC hardware/emulated alarm.
- Timekeeper tests: concurrent realtime set/read publication and VVAR sequence
  integrity.
- Gates: `cargo xtask lint invariants time-layering`, `time-wake-retired`,
  focused package tests, `cargo xtask progress validate`, `cargo xtask lint
  docs`, and `git diff --check`.
