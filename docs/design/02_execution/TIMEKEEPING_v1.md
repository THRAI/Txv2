# Timekeeping Service

<!-- txdoc:02-EXECUTION-TIMEKEEPING-SERVICE-V1 -->

## Status
<!-- txdoc:TIMEKEEPING-STATUS -->

Draft v1.

This document specifies the architecture needed for Linux/POSIX wall-clock
time, POSIX timers, and the `adjtimex` / `clock_adjtime` family. It extends
the current wall-clock/vDSO work into a service subsystem while preserving the
HAL contract that `TimeIf` is monotonic-only.

Normative anchors:

- [`HAL_v1.md`](../01_substrate/HAL_v1.md) §14: `TimeIf` is the platform
  monotonic clock and deadline source. It does not own wall-clock time or CPU
  time.
- [`MODULE_MAP_v1.md`](../00_meta-framework/MODULE_MAP_v1.md) §1-2:
  timekeeping is a service subsystem: durable policy/ledger state consumed by
  semantic owners.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) §2.3:
  `OnTimer` and protocol deadlines are driver/wake machinery, not time
  semantics.
- [`SIGNAL_v1.md`](../04_process-signals/SIGNAL_v1.md) and
  [`SIGNAL_ATTACHMENTS_v1.md`](../04_process-signals/SIGNAL_ATTACHMENTS_v1.md):
  POSIX timers are signal producers; timerfd is an fd adapter over timer
  expiry readiness.

Linux reference points for behavior and field names:

- `external/linux-rv-6.17/kernel/time/timekeeping.c`
- `external/linux-rv-6.17/kernel/time/ntp.c`
- `external/linux-rv-6.17/kernel/time/time.c`
- `external/linux-rv-6.17/kernel/time/posix-timers.c`
- `external/linux-rv-6.17/kernel/time/itimer.c`
- `external/linux-rv-6.17/include/uapi/linux/timex.h`
- `external/linux-rv-6.17/include/uapi/linux/time_types.h`

---

## Purpose
<!-- txdoc:TIMEKEEPING-PURPOSE -->

The timekeeping service owns Tx's kernel-wide civil-time model:

1. converting a platform monotonic source into clock domains visible to
   userspace;
2. maintaining `CLOCK_REALTIME`, `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`,
   `CLOCK_BOOTTIME`, coarse clocks, and `CLOCK_TAI` projections;
3. applying step and slew adjustments requested through Linux time syscalls;
4. publishing vDSO/VVAR conversion state;
5. notifying absolute-time consumers when wall-clock state changes;
6. providing the clock basis for POSIX timers, interval timers, timerfd,
   absolute sleeps, and timed waits.

The service is not:

- a HAL extension;
- a scheduler policy;
- a signal subsystem;
- a reactor timer wheel;
- a CPU accounting subsystem.

HAL supplies the hardware monotonic reading and per-hart deadlines. The
timekeeping service interprets that reading as kernel clock domains. Reactor
and `TimerWheel` provide wake mechanics. Signal, timerfd, futex, mq, ppoll,
and syscall shims consume timekeeping projections but do not own the global
clock state.

---

## Core Claim
<!-- txdoc:TIMEKEEPING-CORE-CLAIM -->

txKernel timekeeping is a **clocksource + discipline + publication** service.

### Clocksource
<!-- txdoc:TIMEKEEPING-CLOCKSOURCE -->

The clocksource is the monotonic hardware-derived counter basis. In v1 this
may be represented by `TimeIf::read_ns()`. Full Linux-like discipline may add
a raw-cycle surface later, but that is a HAL extension and must be justified as
its own compatibility slice.

The clocksource must be:

- monotonic for `CLOCK_MONOTONIC` consumers;
- cheap enough for syscall and scheduler hot paths;
- independent of realtime adjustments;
- usable for absolute monotonic deadlines.

### Discipline
<!-- txdoc:TIMEKEEPING-DISCIPLINE -->

The discipline layer stores and mutates the relationship between monotonic time
and civil clocks.

It owns:

- realtime offset and generation;
- TAI offset;
- NTP status bits;
- frequency/offset slew state;
- maxerror and esterror accounting;
- precision, tolerance, tick, and mode fields reported through `timex`;
- leap-state bookkeeping;
- vDSO conversion parameters and basetimes.

Step changes (`clock_settime`, `settimeofday`, `ADJ_SETOFFSET`) and discipline
changes (`ADJ_OFFSET`, `ADJ_FREQUENCY`, `ADJ_TICK`, `ADJ_TAI`, etc.) are
different operations. Step changes may immediately move realtime; discipline
changes alter how realtime evolves from the monotonic basis.

### Publication
<!-- txdoc:TIMEKEEPING-PUBLICATION -->

The service publishes only state changes. It does not write exact current time
to shared memory on every tick.

Publication targets:

- VVAR/vDSO conversion snapshots;
- realtime-generation changes for absolute realtime waiters;
- clock-was-set notifications for timerfd and POSIX timers;
- future trace/audit hooks for time discipline changes.

Subscribers must re-evaluate their predicate after a wake. A wake is not
truth; this follows the existing signal and wait-source publication rules.

---

## Position in the System
<!-- txdoc:TIMEKEEPING-POSITION-IN-THE-SYSTEM -->

Timekeeping is a **service subsystem**.

It stores durable kernel-wide time policy and exposes small query/mutation
APIs. Full semantic subsystems remain responsible for their own objects:

- thread runtime and scheduler own CPU accounting;
- signal owns signal routing and pending queues;
- timerfd owns fd-readiness state and expiration counters;
- POSIX timer support owns per-process timer ids and timer lifetime;
- syscall shims own Linux ABI translation;
- reactor owns task polling and wait resolution;
- HAL owns platform counters and deadline programming.

Timekeeping must not allocate foreign semantic objects or publish their
readiness. Instead, it emits clock-change notifications that foreign owners
consume in their own commit/recheck paths.

---

## Scope
<!-- txdoc:TIMEKEEPING-SCOPE -->

### In Scope for v1
<!-- txdoc:TIMEKEEPING-IN-SCOPE-V1 -->

- `Timekeeper` service state above `TimeIf`.
- `CLOCK_REALTIME = CLOCK_MONOTONIC + disciplined offset`.
- `CLOCK_MONOTONIC_RAW` initially equal to monotonic unless a raw clocksource
  lands.
- `CLOCK_BOOTTIME` initially equal to monotonic until suspend accounting exists.
- `CLOCK_TAI` as realtime plus stored TAI offset.
- vDSO/VVAR snapshot ownership.
- wall-clock generation and notification.
- read-only `adjtimex` / `clock_adjtime` query fields.
- step-time changes through `clock_settime`, `settimeofday`, and
  `ADJ_SETOFFSET`.
- narrow `adjtimex` bookkeeping mutations that can be represented truthfully:
  `ADJ_TAI`, `ADJ_MAXERROR`, `ADJ_ESTERROR`, `ADJ_STATUS`, `ADJ_NANO`, and
  `ADJ_MICRO`.
- shared clock conversion for `timerfd`, `clock_nanosleep`, future POSIX
  timers, `getitimer` / `setitimer`, futex realtime waits, mq timed waits,
  ppoll/pselect timeouts, and syscall clock queries.

### Deferred
<!-- txdoc:TIMEKEEPING-DEFERRED -->

- raw-cycle HAL extension if `TimeIf::read_ns()` is insufficient for
  frequency discipline precision;
- full NTP PLL/FLL parity;
- slew/frequency mutation modes: `ADJ_OFFSET`, `ADJ_FREQUENCY`, `ADJ_TICK`,
  `ADJ_TIMECONST`, `ADJ_OFFSET_SINGLESHOT`, and `ADJ_OFFSET_SS_READ`;
- PPS discipline;
- suspend/resume and alarm clocks;
- per-time-namespace offsets;
- CPU-time clocks and CPU interval timers;
- leap-second insertion/deletion behavior visible to userspace;
- kernel audit trail for NTP changes;
- hardware RTC persistence;
- per-user `RLIMIT_SIGPENDING` enforcement for timer-created realtime signals.

---

## Data Model
<!-- txdoc:TIMEKEEPING-DATA-MODEL -->

### Timekeeper
<!-- txdoc:TIMEKEEPING-TIMEKEEPER -->

```rust
pub struct Timekeeper {
    // clocksource snapshot
    cycle_last: AtomicU64,
    mask: AtomicU64,
    mult: AtomicU64,
    shift: AtomicU64,

    // bases in nanoseconds
    monotonic_base_ns: AtomicU64,
    realtime_base_ns: AtomicI64,
    raw_base_ns: AtomicU64,
    boottime_base_ns: AtomicU64,

    // civil-time discipline state
    realtime_offset_ns: AtomicI64,
    tai_offset_sec: AtomicI32,
    generation: AtomicU64,

    // NTP/timex state
    ntp: SpinMutex<NtpDisciplineState>,

    // publication
    time_change_port: RawPort,
}
```

The concrete implementation may split this into smaller structs, but these
roles must remain distinct: conversion state, civil offset state, NTP state,
and publication state.

### NTP Discipline State
<!-- txdoc:TIMEKEEPING-NTP-STATE -->

```rust
pub struct NtpDisciplineState {
    pub modes: u32,
    pub status: u32,
    pub offset_ns: i64,
    pub freq_scaled_ppm: i64,
    pub maxerror_us: i64,
    pub esterror_us: i64,
    pub constant: i64,
    pub precision_us: i64,
    pub tolerance_scaled_ppm: i64,
    pub tick_us: i64,
    pub pps: PpsStats,
    pub time_state: TimeState,
}
```

v1 may hold zero/default PPS stats, but the fields must be represented so
`adjtimex` readback is stable and future mutation modes do not require an ABI
rewrite.

### VVAR Snapshot
<!-- txdoc:TIMEKEEPING-VVAR-SNAPSHOT -->

The vDSO-visible snapshot is derived from the timekeeper. It is not a second
clock.

Fields:

- seqlock sequence;
- `cycle_last`;
- `mask`;
- `mult`;
- `shift`;
- realtime basetime;
- monotonic basetime;
- raw/boottime/TAI basetimes when exposed;
- coarse clock cache if coarse clocks are cached separately.

The snapshot is refreshed on:

- timekeeper initialization;
- clocksource/conversion parameter change;
- successful realtime step;
- discipline updates that alter conversion;
- bounded periodic refresh if required to avoid delta overflow.

It is not refreshed on every scheduler tick merely to write exact current time.

---

## Public Service Surface
<!-- txdoc:TIMEKEEPING-PUBLIC-SURFACE -->

### Query APIs
<!-- txdoc:TIMEKEEPING-QUERY-APIS -->

```rust
pub fn clock_now<P: TimeIf>(clock: ClockId) -> Result<u64, TimeError>;
pub fn clock_resolution(clock: ClockId) -> Result<Timespec, TimeError>;
pub fn realtime_generation() -> u64;
pub fn snapshot_for_vvar<P: TimeIf>() -> VvarSnapshot;
pub fn monotonic_deadline_for<P: TimeIf>(clock: ClockId, deadline_ns: u64)
    -> Result<Deadline, TimeError>;
```

`monotonic_deadline_for` converts absolute realtime/TAI/boottime deadlines into
monotonic deadlines. Consumers store both the original clock-domain target and
the converted monotonic deadline when wall-clock revalidation is required.

### Mutation APIs
<!-- txdoc:TIMEKEEPING-MUTATION-APIS -->

```rust
pub fn set_realtime_ns<P: TimeIf>(
    subject: &SubjectContext,
    realtime_ns: u64,
) -> Result<TimeChange, TimeError>;

pub fn adjtimex<P: TimeIf>(
    subject: &SubjectContext,
    clock: ClockId,
    request: TimexRequest,
) -> Result<TimexResult, TimeError>;
```

Mutations must:

1. validate user-visible range and mode rules before changing state;
2. perform credential/capability checks through the cred service;
3. update timekeeper state;
4. publish VVAR and clock-change notifications;
5. return a stable `TimeChange` describing whether wall time stepped, slew
   state changed, TAI changed, or no state changed.

### Subscriber APIs
<!-- txdoc:TIMEKEEPING-SUBSCRIBER-APIS -->

```rust
pub fn subscribe_clock_change(kind: ClockChangeKind) -> TimeChangeSubscription;
pub fn notify_clock_change(change: TimeChange);
```

This surface is for kernel consumers only. It must not become a generic event
queue exposed to userspace. fd adapters such as timerfd and signalfd keep their
own userspace ABI and readiness state.

---

## Clock Domains
<!-- txdoc:TIMEKEEPING-CLOCK-DOMAINS -->

| Clock | v1 basis | Adjustment behavior |
|---|---|---|
| `CLOCK_MONOTONIC` | HAL monotonic | never stepped by realtime changes |
| `CLOCK_MONOTONIC_RAW` | same as monotonic until raw cycles land | never NTP-disciplined in v1 |
| `CLOCK_REALTIME` | monotonic plus disciplined offset | step and slew visible |
| `CLOCK_REALTIME_COARSE` | realtime, optionally cached | follows realtime semantics |
| `CLOCK_MONOTONIC_COARSE` | monotonic, optionally cached | follows monotonic semantics |
| `CLOCK_BOOTTIME` | monotonic until suspend accounting lands | unaffected by realtime step |
| `CLOCK_TAI` | realtime plus TAI offset | TAI offset updates visible |
| CPU clocks | thread/scheduler accounting | out of scope for timekeeping v1 |

Unsupported or non-existent clock ids return `EINVAL`. Existing clocks without
an implemented operation return `EOPNOTSUPP` only when Linux distinguishes
"valid clock, operation unsupported" from "invalid clock".

---

## Adjustment Semantics
<!-- txdoc:TIMEKEEPING-ADJUSTMENT-SEMANTICS -->

### Step Changes
<!-- txdoc:TIMEKEEPING-STEP-CHANGES -->

Step changes immediately alter `CLOCK_REALTIME` by changing the realtime
offset/base. They must increment the realtime generation and publish a
clock-change event.

Sources:

- `clock_settime(CLOCK_REALTIME)`;
- `settimeofday`;
- `clock_adjtime(CLOCK_REALTIME, ADJ_SETOFFSET)`;
- `adjtimex(ADJ_SETOFFSET)`.

### Slew and Frequency Changes
<!-- txdoc:TIMEKEEPING-SLEW-FREQUENCY -->

Slew/frequency changes alter how realtime advances relative to monotonic time.
They must not make `CLOCK_MONOTONIC` jump.

Linux modes to model:

- `ADJ_OFFSET`;
- `ADJ_FREQUENCY`;
- `ADJ_TICK`;
- `ADJ_TIMECONST`;
- `ADJ_MAXERROR`;
- `ADJ_ESTERROR`;
- `ADJ_STATUS`;
- `ADJ_TAI`;
- `ADJ_NANO`;
- `ADJ_MICRO`;
- `ADJ_OFFSET_SINGLESHOT`;
- `ADJ_OFFSET_SS_READ`.

v1 may return `EOPNOTSUPP` for mutation modes not yet implemented, but the
spec requires the mode matrix to be explicit in the syscall documentation and
tests. Read-only `modes == 0` must return stable state.

v1 compatibility target:

| Mode | v1 behavior |
|---|---|
| `modes == 0` | read-only query; fills `timex`; returns `TIME_OK` unless the stored status is an error state |
| `ADJ_SETOFFSET` | privileged realtime step by adding `time` to current realtime; publishes realtime generation change |
| `ADJ_TAI` | privileged TAI offset update; publishes TAI-offset change |
| `ADJ_MAXERROR` / `ADJ_ESTERROR` | privileged bookkeeping update; no realtime generation bump |
| `ADJ_STATUS` | privileged update of writable status bits; rejects read-only `STA_RONLY` bits with `EINVAL` |
| `ADJ_NANO` / `ADJ_MICRO` | privileged resolution-mode bookkeeping; affects `timex.status` / readback units only |
| `ADJ_OFFSET`, `ADJ_FREQUENCY`, `ADJ_TICK`, `ADJ_TIMECONST`, `ADJ_OFFSET_SINGLESHOT`, `ADJ_OFFSET_SS_READ` | `EOPNOTSUPP` until true slew/frequency discipline lands |

`clock_adjtime` uses the same matrix for `CLOCK_REALTIME`. Other clock ids
return `EINVAL` when invalid and `EOPNOTSUPP` when valid but not adjustable.

### Error and Status Accounting
<!-- txdoc:TIMEKEEPING-ERROR-STATUS -->

The service owns `timex` report fields:

- `status`;
- `maxerror`;
- `esterror`;
- `precision`;
- `tolerance`;
- `tick`;
- PPS fields;
- `tai`;
- return state (`TIME_OK`, `TIME_ERROR`, and future leap states).

When Tx has no PPS or leap implementation, it should report zero/default values
and a documented status rather than fabricating Linux behavior.

---

## Notifications and Consumers
<!-- txdoc:TIMEKEEPING-NOTIFICATIONS -->

### Clock Change Kinds
<!-- txdoc:TIMEKEEPING-CLOCK-CHANGE-KINDS -->

```rust
pub enum ClockChangeKind {
    RealtimeStep,
    RealtimeDiscipline,
    TaiOffset,
    ConversionState,
}

pub struct TimeChange {
    pub kind: ClockChangeKind,
    pub old_generation: u64,
    pub new_generation: u64,
}
```

The implementation may combine kinds in a bitmask, but subscribers must be
able to distinguish a realtime step from a pure query/no-op.

### Required Consumers
<!-- txdoc:TIMEKEEPING-REQUIRED-CONSUMERS -->

- `clock_gettime`, `gettimeofday`, `clock_getres`, `times`: query service
  projections.
- `clock_settime`, `settimeofday`, `adjtimex`, `clock_adjtime`: mutate or query
  service state.
- `clock_nanosleep`: realtime absolute waits re-evaluate after generation
  changes.
- `timerfd`: cancel-on-set and non-cancel realtime absolute timers subscribe to
  realtime changes.
- POSIX timers: realtime absolute timers revalidate; cancel semantics follow
  POSIX/Linux timer rules. Blocking wait drivers that can sleep past a process
  timer deadline must race their normal wait source with the process timer
  deadline, then re-enter signal publication and predicate recheck when that
  deadline wins.
- `getitimer` / `setitimer`: `ITIMER_REAL` consumes realtime/monotonic
  conversion; CPU interval timers wait for CPU accounting.
- futex realtime waits: absolute realtime waits re-evaluate after generation
  changes.
- POSIX mq timed waits and ppoll/pselect: use monotonic deadlines derived by
  the syscall shim and wait protocol.
- vDSO/VVAR: snapshots are generated by the service.

### Non-Consumers
<!-- txdoc:TIMEKEEPING-NON-CONSUMERS -->

These must not depend on timekeeping for semantic decisions:

- HAL timer interrupt delivery;
- page allocator;
- VFS path resolution;
- credential checks except for time-setting permission;
- scheduler selection policy, except CPU accounting when that later lands.

---

## POSIX Timers and Interval Timers
<!-- txdoc:TIMEKEEPING-POSIX-TIMERS -->

POSIX timers are not owned directly by the timekeeper. They are a process-owned
semantic object family that consumes timekeeping.

Required shape:

- per-process timer id table;
- `timer_create`, `timer_settime`, `timer_gettime`, `timer_getoverrun`,
  `timer_delete`;
- `SIGEV_NONE`;
- `SIGEV_SIGNAL`;
- default `NULL sevp` as `SIGEV_SIGNAL` + `SIGALRM`;
- overrun accounting per timer;
- expiry publication through `deliver_posix_signal`;
- timer cleanup on process exit and exec rules per Linux/POSIX.

`ITIMER_REAL` should be implemented as a distinguished per-process interval
timer delivering `SIGALRM`. `ITIMER_VIRTUAL` and `ITIMER_PROF` require CPU-time
accounting and are out of scope for this timekeeping service v1.

The timekeeper provides clock conversion and change notification. The POSIX
timer owner stores timer ids, signal configuration, overrun state, and process
lifetime bindings.

Expiry delivery is not a new notification fabric. Process timer tables expose
their next monotonic deadline to syscall wait drivers; the reactor provides the
wake, and the signal subsystem publishes the resulting `SIGEV_SIGNAL` or
`SIGALRM` through its existing process-directed path. Full Linux
`EINTR`/restart/remnant behavior belongs to the blocking-syscall interruption
contract, not to the timekeeper itself. Individual wait drivers may adopt the
contract incrementally: fd reads such as timerfd can return `EINTR` once the
signal is published, while multiplexers such as epoll may first wake and
recheck before their full restart policy is centralized. Timer-only sleeps can
schedule the earlier of their own sleep deadline and the next process-timer
deadline, then return `EINTR` if the process timer wins. Wait-source protocols
with existing timeout support, such as futex wait, can compose the process
timer as an earlier timeout and translate that specific abort into signal
publication plus `EINTR`. POSIX mq blocking waits use the same rule while
preserving mq readiness ownership: the mq subsystem publishes both its legacy
channel and v3 wait source, and the syscall driver returns `EINTR` only when
the process-timer deadline wins the wait race. `ppoll` follows the same v1
shape by adding the process-timer deadline to its existing wait-source park;
the user-supplied timeout and signal-mask semantics remain part of the broader
poll/select conformance follow-up. Empty eventfd reads and write-overflow
eventfd writes use the same wait-source race and return `EINTR` when the
process-timer deadline wins. Blocking raw AIO `io_getevents` and child waits
via `wait4` use the same v1 race; `wait4` treats the timer signal as an
interruption and leaves still-running children in place. `signalfd` preserves
its signal-consumer role: a process-timer signal covered by the signalfd mask
becomes readable `signalfd_siginfo`, while an uncovered deliverable timer
signal interrupts with `EINTR` and an uncovered masked timer signal keeps the
read parked. `userfaultfd` preserves fault-queue ownership: queued fault
messages win over timer interruption, empty reads return `EINTR` only for a
deliverable timer signal, and masked timer signals leave the agent wait
parked. Faulting-thread cancellation remains outside this v1 timer policy and
belongs with endpoint-death/fatal-signal handling.

POSIX timer ids live in a dedicated timer table keyed by `ProcessIdentity`.
`ProcessPayload` holds the table capability/reference so fork/exec/exit can
apply Linux lifetime rules without bloating the process payload with timer
implementation details. The table owns timer ids and timer objects; time
namespaces are only projection lenses and must not own timers.

---

## Syscall ABI Policy
<!-- txdoc:TIMEKEEPING-SYSCALL-ABI-POLICY -->

### Already Wired Families
<!-- txdoc:TIMEKEEPING-ALREADY-WIRED-FAMILIES -->

The following should be migrated onto the service without behavior regression:

- `clock_gettime`;
- `clock_getres`;
- `clock_settime`;
- `gettimeofday`;
- `settimeofday`;
- `nanosleep`;
- `clock_nanosleep`;
- `timerfd_create`;
- `timerfd_settime`;
- `timerfd_gettime`;
- timerfd-shaped `read`.

### Missing Tail
<!-- txdoc:TIMEKEEPING-MISSING-TAIL -->

The service enables:

- `getitimer`;
- `setitimer`;
- `timer_create`;
- `timer_settime`;
- `timer_gettime`;
- `timer_getoverrun`;
- `timer_delete`;
- `adjtimex`;
- `clock_adjtime`.

The syscall shim owns Linux user-layout decoding and errno mapping. It must
not duplicate timekeeper state.

### Permission
<!-- txdoc:TIMEKEEPING-PERMISSION -->

Setting or disciplining realtime requires the existing Tx credential/capability
check pattern. v1 may gate to init/root credentials if full capability bits are
not yet expressive enough, but the service API must accept a subject context so
the rule can later become `CAP_SYS_TIME` without changing call sites.

---

## Audit and Blast Radius
<!-- txdoc:TIMEKEEPING-AUDIT-BLAST-RADIUS -->

The first implementation is **mostly ready** if scoped to the v1 service shell.
It is **not ready** for full Linux NTP parity until raw clocksource and CPU-time
accounting follow-ups are separately specified.

### Blast Radius Tiers
<!-- txdoc:TIMEKEEPING-BLAST-RADIUS-TIERS -->

| Tier | Scope | Expected surface | Risk |
|---|---|---:|---|
| V1 service shell | new `timekeeping` module, migrate `wall_clock`, vDSO snapshots, clock syscalls, timerfd clock-change hook | ~15-25 files | Medium |
| Timer/time syscall tail | POSIX timer table, itimers, timer_create family, syscall layouts/tests | ~25-45 files | Medium-high |
| Full discipline | raw-cycle HAL extension, real slew/frequency, CPU timers, time namespaces, suspend/alarm clocks | 60+ files | High |

### V1 Touch Points
<!-- txdoc:TIMEKEEPING-V1-TOUCH-POINTS -->

- `tx-subsystems`: add timekeeping service, retire direct `wall_clock` state,
  adapt timerfd clock-change subscription, keep timerfd ownership local.
- `tx-vdso` / VVAR mapping: consume `Timekeeper` snapshots and preserve the
  no-exact-time-per-tick policy.
- `tx-shims`: route existing time syscalls through the service and add
  `adjtimex` / `clock_adjtime` readback plus v1 mutation matrix.
- `tx-scripts` / `tx-substrate` / `tx-reactor`: reuse existing `OnTimer`,
  `TimerWheel`, and mailbox wake mechanics; do not add a new yield shape.
- `process` / `signal`: only the POSIX timer follow-up should add timer tables
  and signal producers. The v1 timekeeper shell should not mutate signal queues.
- `HAL`: no v1 changes. Raw-cycle `ClocksourceIf` is a later high-blast-radius
  extension.

### Readiness Verdict
<!-- txdoc:TIMEKEEPING-READINESS-VERDICT -->

Ready for implementation:

- service shell and API;
- migration of existing wallclock/vDSO/syscall reads;
- realtime step notification;
- read-only `adjtimex` / `clock_adjtime`;
- bookkeeping-only `adjtimex` modes listed in this spec.

Not ready without more specs:

- full slew/frequency discipline;
- CPU-time clocks and CPU interval timers;
- time namespaces;
- suspend/alarm clocks;
- leap second behavior;
- realtime signal queueing fidelity for full POSIX timer conformance.

## Implementation Notes
<!-- txdoc:TIMEKEEPING-IMPLEMENTATION-NOTES -->

### v1 Without HAL Expansion
<!-- txdoc:TIMEKEEPING-V1-WITHOUT-HAL-EXPANSION -->

The first implementation should avoid changing `TimeIf` if possible:

- keep `TimeIf::read_ns()` as the monotonic input;
- compute realtime and TAI projections from monotonic ns;
- keep `mult/shift/mask` stable for VVAR compatibility;
- report read-only `adjtimex` fields;
- support only the v1 mode matrix above; return deliberate `EOPNOTSUPP` for
  mutation modes requiring true frequency discipline.

This gives a smaller landing and keeps platform/test `TimeIf` implementations
stable.

### Full Discipline Follow-up
<!-- txdoc:TIMEKEEPING-FULL-DISCIPLINE-FOLLOW-UP -->

If Tx needs Linux-like frequency discipline, add a HAL clocksource extension:

```rust
pub trait ClocksourceIf {
    fn read_cycles() -> u64;
    fn cycle_mask() -> u64;
    fn frequency_hz() -> u64;
    fn stable_across_harts() -> bool;
}
```

This extension must not replace `TimeIf`. `TimeIf` remains the monotonic
deadline/read surface for scheduler and ordinary kernel consumers. The
timekeeper may consume `ClocksourceIf` when available.

### Locking
<!-- txdoc:TIMEKEEPING-LOCKING -->

Hot-path queries must be lock-free or seqlock-like. Mutations may take a
spinlock around NTP/discipline state and publish a seqlock update for VVAR.

IRQ handlers must not create EBR guards. Clock-change publication that needs
capability upgrades must run outside hard IRQ context, through the same
deferred/task path used by other semantic publications.

---

## Invariants
<!-- txdoc:TIMEKEEPING-INVARIANTS -->

TIME-1. `CLOCK_MONOTONIC` must never move backward because of realtime
adjustment.

TIME-2. HAL remains monotonic-only. Wall-clock and discipline state must not be
stored in board/platform crates.

TIME-3. VVAR contains conversion state and basetimes, not exact current time
rewritten on every tick.

TIME-4. Every successful realtime step increments realtime generation exactly
once.

TIME-5. Subscribers treat clock-change notifications as hints and re-evaluate
their own predicates under fresh observation.

TIME-6. POSIX timer signal delivery goes through the signal subsystem's
canonical signal producer entry point.

TIME-7. CPU-time clocks and interval timers must not claim correctness until
thread/process CPU accounting exists.

TIME-8. `adjtimex` mutation modes must either update explicit discipline state
or return deliberate unsupported errors. Silent success for unimplemented slew
policy is forbidden.

---

## Test Strategy
<!-- txdoc:TIMEKEEPING-TEST-STRATEGY -->

Unit tests:

- monotonic is unaffected by realtime step;
- realtime equals monotonic plus disciplined offset;
- TAI equals realtime plus TAI offset;
- generation increments once per successful step;
- VVAR snapshot changes only on configured publication points;
- read-only `timex` fields round-trip deterministically;
- unsupported mutation modes return the documented errno;
- notification subscribers are called once per published change.

Subsystem tests:

- timerfd cancel-on-set and rearm semantics through the new subscriber API;
- absolute realtime `clock_nanosleep` revalidates after forward/backward jumps;
- POSIX timer expiry posts the configured signal and overrun count;
- `ITIMER_REAL` delivers `SIGALRM`;
- futex/mq/ppoll realtime deadlines derive and revalidate correctly where
  those syscalls support realtime.

Syscall tests:

- `adjtimex` and `clock_adjtime` copy full user layouts;
- invalid clocks, null pointers, bad modes, and unprivileged mutations return
  Linux-shaped errors;
- `clock_gettime` syscall and vDSO agree within bounded delta;
- generated syscall status contains all timer/time tail constants.

Integration tests:

- LTP timer tests that only need wall/monotonic timers should move from missing
  to runnable;
- OSComp sleep/time probes should keep passing;
- QEMU boot should verify VVAR initialization and no early timekeeper panic.

---

## Open Questions
<!-- txdoc:TIMEKEEPING-OPEN-QUESTIONS -->

All v1-blocking questions are decided above:

1. v1 does not add raw-cycle HAL APIs; true slew/frequency modes wait for
   `ClocksourceIf`.
2. time namespaces are deferred until the namespace/setns line is ready;
   `NsProxy.time_ns` remains a projection lens, not a timer owner.
3. the v1 `adjtimex` mode matrix is fixed in
   `TIMEKEEPING-SLEW-FREQUENCY`.
4. `CLOCK_BOOTTIME` equals monotonic until suspend/resume accounting lands.
5. POSIX timer ids live in a dedicated process-keyed timer table, referenced
   from `ProcessPayload`.

Remaining nonblocking research questions:

- exact LTP subsets to target first for `adjtimex`, POSIX timers, and itimers;
- whether future raw-cycle support should be a separate `ClocksourceIf`
  supertrait on `TxPlatform` or a narrower optional platform adapter;
- whether leap-second state should appear before or after real NTP slew.

---

## Summary
<!-- txdoc:TIMEKEEPING-SUMMARY -->

Timekeeping is the service subsystem that turns HAL monotonic time into Linux
clock domains, time discipline, and shared vDSO state. It does not own task
polling, signal routing, or timerfd/POSIX timer entities. Those consumers ask
the service for clock projections and subscribe to clock-change hints, then
publish their own semantic outcomes.

The first implementation should land the service shell, migrate existing
wallclock/vDSO/syscall reads to it, and expose read-only `adjtimex` state plus
honest unsupported errors for unimplemented discipline modes. Full NTP
frequency/slew parity and CPU-time clocks are deliberate follow-ups.
