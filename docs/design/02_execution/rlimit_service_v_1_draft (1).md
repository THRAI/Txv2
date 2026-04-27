# RLimit Service

<!-- txdoc:02-EXECUTION-RLIMIT-SERVICE-V-1-DRAFT-1 -->

## Status
<!-- txdoc:RLIMIT-STATUS -->

Draft v1.1.

Aligned with the current txKernel framework:

- rlimit is a **service subsystem** in reduced form;
- scripts remain **state-blind** and do not make resource-limit decisions directly;
- full subsystems own their own semantic bindings and commit points;
- resource-limit enforcement is expressed as **ledger + reservation**, not as grant minting.

---

## Purpose
<!-- txdoc:RLIMIT-PURPOSE -->

The rlimit service is txKernel's subsystem for **per-process resource ceilings and their consumption accounting**.

It is **not**:

- a namespace,
- a capability system,
- a grant-minting subsystem,
- a replacement for substrate reservation primitives.

Its job is to:

1. hold durable per-process resource ceilings;
2. expose pure policy checks over those ceilings;
3. support limit-changing transitions such as `setrlimit` and `prlimit`;
4. cooperate with substrate credit reservations so that actual resource consumption is linearized at commit time;
5. cooperate with credit release so that usage falls when the corresponding resource is actually freed.

The design separates:

- **what ceiling applies to this process?** — durable rlimit state;
- **how much is currently consumed?** — stable usage counters;
- **may this operation consume N units right now?** — step-time check plus linear reservation.

---

## Core claim
<!-- txdoc:RLIMIT-CORE-CLAIM -->

txKernel resource limits use a **ledger-and-reservation model**.

### The ledger
<!-- txdoc:RLIMIT-THE-LEDGER -->

The kernel maintains a durable rlimit ledger answering:

> what resource ceilings apply to this process?

This ledger is represented by an `RLimitBag` object. It is durable, process-local policy state, and the authoritative basis for resource-limit checks.

### Usage counters
<!-- txdoc:RLIMIT-USAGE-COUNTERS -->

The kernel also maintains a stable usage-counter object answering:

> how much of each limited resource is currently committed for this process?

These counters are mutated by substrate credit reservation and release primitives. They are not replaced by `setrlimit`; they persist across limit changes.

### Reservations
<!-- txdoc:RLIMIT-RESERVATIONS -->

Operations do not mint reusable authority objects from rlimit state.

Instead, operations:

1. perform a pure admissibility check against the ledger;
2. attempt a linear reservation against the stable usage counters for the requested quantity;
3. commit that reservation if and only if the enclosing semantic operation commits;
4. later release the accounted quantity when the corresponding resource is freed.

The key difference from cred is structural:

- cred produces **subsystem-local grants**;
- rlimit produces **operation-local reservations**.

### Consequence
<!-- txdoc:RLIMIT-CONSEQUENCE -->

An rlimit check saying “allowed” does **not** guarantee that the operation will succeed. Concurrent operations may consume the last available budget first. The ledger answers whether the operation is admissible in principle; the reservation linearizes actual consumption.

---

## Position in the system
<!-- txdoc:RLIMIT-POSITION-IN-THE-SYSTEM -->

Rlimit is a **service subsystem**.

It does not own user-visible names and does not resolve path / pid / fd signifiers. Instead:

- full semantic subsystems such as fd, process, VM, and signal call into `rlim::checks::*` when an operation may consume a limited resource;
- those full subsystems acquire substrate credit reservations during their reserve phase;
- those full subsystems release charges when the corresponding semantic resources are actually freed;
- those full subsystems remain the publication sites for their own semantic objects.

In particular:

- **rlimit authorizes**;
- **substrate credit linearizes accounting**;
- **the owning subsystem reserves, commits, and later releases**.

There is no rlimit-side publication of foreign subsystem objects.

---

## Scope
<!-- txdoc:RLIMIT-SCOPE -->

### In scope for v1
<!-- txdoc:RLIMIT-IN-SCOPE-FOR-V1 -->

- a durable `RLimitBag` type for ceilings;
- a stable `RLimitUsage` type for committed usage counters;
- storage of both in process policy;
- pure authorization checks over rlimit state;
- limit-changing transitions such as `setrlimit`;
- a scoped `prlimit` story (self-only in v1);
- cooperation with substrate credit reservation and release for selected resources;
- read-only projections of rlimit state.

### Enforced resources in v1
<!-- txdoc:RLIMIT-ENFORCED-RESOURCES-IN-V1 -->

v1 should explicitly scope the first enforced set rather than pretending to cover the full POSIX/Linux catalog.

A recommended v1 set is:

- `RLIMIT_NOFILE`
- `RLIMIT_STACK`
- `RLIMIT_SIGPENDING`
- `RLIMIT_MEMLOCK`

These are a good v1 set because they can be enforced with straightforward per-process counters or direct VM integration:

- `RLIMIT_NOFILE`: ordinary per-process descriptor accounting;
- `RLIMIT_STACK`: enforced at stack-growth time in VM;
- `RLIMIT_SIGPENDING`: enforced at signal-queue insertion time;
- `RLIMIT_MEMLOCK`: enforced when locked-memory accounting exists.

Kinds such as `RLIMIT_NPROC`, `RLIMIT_CPU`, `RLIMIT_AS`, `RLIMIT_DATA`, and `RLIMIT_CORE` are deferred for recognizable reasons:

- per-user accounting (`RLIMIT_NPROC`) is not v1 scope;
- time accounting (`RLIMIT_CPU`) is not v1 scope;
- address-space / heap-shape accounting (`RLIMIT_AS`, `RLIMIT_DATA`) requires deeper VM integration than v1 needs;
- core-dump accounting (`RLIMIT_CORE`) is irrelevant until core dumps exist.

### Out of scope
<!-- txdoc:RLIMIT-OUT-OF-SCOPE -->

- kernel-wide memory reclaim policy;
- scheduling fairness;
- global quota systems;
- minting reusable authority objects from resource-limit state.

---

## Data model
<!-- txdoc:RLIMIT-DATA-MODEL -->

### Canonical limit object
<!-- txdoc:RLIMIT-CANONICAL-LIMIT-OBJECT -->

```rust
pub struct RLimitBag {
    pub limits: [RLimitEntry; RLIMIT_KIND_COUNT],
}

pub struct RLimitEntry {
    pub soft: u64,
    pub hard: u64,
}
```

The intended meanings are:

- `soft`: the current effective ceiling for ordinary enforcement;
- `hard`: the administrative ceiling above which unprivileged raises are forbidden.

### Stable usage object
<!-- txdoc:RLIMIT-STABLE-USAGE-OBJECT -->

```rust
pub struct RLimitUsage {
    pub counters: [AtomicU64; RLIMIT_KIND_COUNT],
}
```

The intended meaning is:

- `counters[k]`: currently committed usage for the resource kind `k`.

This split is load-bearing. `setrlimit` replaces or rewrites the ceiling object, but substrate credit reservations must continue to operate on a stable usage-counter object. Keeping `used` inside `RLimitBag` would race with concurrent bag replacement.

### Resource kinds
<!-- txdoc:RLIMIT-RESOURCE-KINDS -->

```rust
pub enum RLimitKind {
    NoFile,
    Stack,
    SigPending,
    MemLock,
    // other kinds retained for forward compatibility
}
```

The exact set is implementation detail, but v1 should distinguish clearly between:

- kinds represented in the type;
- kinds actually enforced.

### Placement in process policy
<!-- txdoc:RLIMIT-PLACEMENT-IN-PROCESS-POLICY -->

Rlimit state is process-local policy state and lives alongside other policy objects:

```rust
pub struct ProcessPolicy {
    pub cred: Cap<Credential>,
    pub rlimits: Cap<RLimitBag>,
    pub rlim_usage: Cap<RLimitUsage>,
    pub signal_mask: SigMask,
}
```

---

## Why rlimit does not use snapshots
<!-- txdoc:RLIMIT-WHY-RLIMIT-DOES-NOT-USE-SNAPSHOTS -->

Unlike `Credential`, rlimit state is not naturally carried as a syscall-entry by-value snapshot.

The reason is structural:

- credential checks are primarily identity-derived and read-mostly;
- rlimit checks are about **shared availability** and may race with concurrent consumption.

A stale by-value snapshot of current usage is therefore not very informative. The meaningful linearization point is not the snapshot; it is the reservation.

Accordingly:

- scripts do not carry an rlimit snapshot as metadata;
- the owning subsystem observes current rlimit state during its step;
- the owning subsystem then performs the reservation in its reserve phase.

---

## Ownership split
<!-- txdoc:RLIMIT-OWNERSHIP-SPLIT -->

### Process owns subject lifetime
<!-- txdoc:RLIMIT-PROCESS-OWNS-SUBJECT-LIFETIME -->

Process owns:

- the policy container that references the current `RLimitBag` and `RLimitUsage`;
- the process/thread lifetime to which those limits apply.

### Rlimit owns limit semantics
<!-- txdoc:RLIMIT-RLIMIT-OWNS-LIMIT-SEMANTICS -->

Rlimit owns:

- the structure of `RLimitBag` and `RLimitUsage`;
- pure admissibility predicates over limit state;
- transitions such as `setrlimit` and `prlimit`;
- formatting and read-only projection of limit state.

### Full subsystems own resource semantics
<!-- txdoc:RLIMIT-FULL-SUBSYSTEMS-OWN-RESOURCE-SEMANTICS -->

FD, signal, VM, and other semantic subsystems own:

- the actual objects whose creation or growth consumes resources;
- the semantic commit points at which those objects become real;
- the decision of how much resource a given operation must reserve;
- the point at which a previously-accounted resource is truly freed and its charge may be released.

Rlimit does not decide what an `fd`, `sigpending` entry, or stack-growth step *means*. It only answers whether the requested charge is admissible and cooperates in linearizing the charge.

---

## Checks surface
<!-- txdoc:RLIMIT-CHECKS-SURFACE -->

Rlimit checks are pure admissibility checks.

They answer questions such as:

- may this process create N more file descriptors in principle?
- may this process grow its stack by N bytes in principle?
- may this process enqueue N more pending signals in principle?
- may this process lock N more bytes of memory in principle?
- may this process raise a soft or hard limit to the requested values?

A proposed surface is:

```rust
rlim::checks::require_fd_creation(
    limits: &RLimitBag,
    usage: &RLimitUsage,
    delta: u32,
    guard: &Guard,
) -> Result<FdCreationPermitted<'g>, Errno>;

rlim::checks::require_stack_growth(
    limits: &RLimitBag,
    usage: &RLimitUsage,
    bytes: u64,
    guard: &Guard,
) -> Result<StackGrowthPermitted<'g>, Errno>;

rlim::checks::require_sigpending_enqueue(
    limits: &RLimitBag,
    usage: &RLimitUsage,
    delta: u32,
    guard: &Guard,
) -> Result<SigPendingPermitted<'g>, Errno>;

rlim::checks::require_memlock_charge(
    limits: &RLimitBag,
    usage: &RLimitUsage,
    bytes: u64,
    guard: &Guard,
) -> Result<MemlockPermitted<'g>, Errno>;

rlim::checks::require_setrlimit(
    current: &RLimitBag,
    cred: &Credential,
    kind: RLimitKind,
    new_soft: u64,
    new_hard: u64,
    guard: &Guard,
) -> Result<SetRLimitAuthorized<'g>, Errno>;
```

These checks answer admissibility only. They do **not** reserve budget and they do **not** promise success under concurrency.

### Cred dependency
<!-- txdoc:RLIMIT-CRED-DEPENDENCY -->

Some rlimit checks are cred-gated. The canonical example is `require_setrlimit`, where raising the hard limit requires privilege (for example, `CAP_SYS_RESOURCE`). This is an intentional service-to-service value-type boundary:

- rlimit consumes `&Credential` as a value input where policy requires it;
- rlimit does not import cred internals;
- cred is not asked to perform the rlimit transition.

### Stack-growth call site
<!-- txdoc:RLIMIT-STACK-GROWTH-CALL-SITE -->

`require_stack_growth` is unusual among the v1 checks because its natural caller is the VM fault handler rather than a syscall script. When a stack-extension fault is recognized as a grow-stack case, VM consults rlimit before extending the mapping. On failure, VM translates the rejection into the appropriate fault response rather than returning an ordinary syscall errno.

---

## Rlimit witnesses
<!-- txdoc:RLIMIT-RLIMIT-WITNESSES -->

Rlimit witnesses are **zero-sized provenance tokens**.

This is the same structural reason as in cred:

- the caller already owns the checked `&RLimitBag` / `&RLimitUsage` inputs;
- the witness only needs to prove the check ran and succeeded under the current guard;
- actual linearization happens later in substrate reservation.

A representative shape is:

```rust
pub struct FdCreationPermitted<'g> {
    _guard: core::marker::PhantomData<&'g ()>,
    _priv: (),
}
```

All rlimit witnesses follow this pattern.

---

## Reservation model
<!-- txdoc:RLIMIT-RESERVATION-MODEL -->

The key enforcement point of rlimit is not the witness but the reservation.

A full operation follows this shape:

1. observe current `RLimitBag` and `RLimitUsage`;
2. call `rlim::checks::require_*`;
3. acquire a substrate credit reservation for the requested quantity against `RLimitUsage`;
4. perform the rest of the operation's reserve work;
5. if the semantic operation commits, commit the credit reservation too;
6. if the semantic operation aborts, the credit reservation drops and rolls back;
7. when the corresponding semantic resource is later freed, issue a credit release against the same stable `RLimitUsage`.

A representative reserve path is:

```rust
let _limit_w = rlim::checks::require_fd_creation(limits_ref, usage_ref, 2, &guard)?;
let charge = substrate::credit::reserve(usage_cap.clone(), RLimitKind::NoFile, 2)?;
```

Later, at the enclosing step's commit point:

```rust
substrate::credit::commit(charge);
```

When the corresponding resource is later freed:

```rust
substrate::credit::release(usage_cap.clone(), RLimitKind::NoFile, 1);
```

The exact release API spelling is substrate detail. The architectural requirement is that rlimit accounting must decrease when the accounted resource is actually freed; otherwise the counter would become a process-lifetime high-water mark rather than steady-state usage.

### Why both check and reservation exist
<!-- txdoc:RLIMIT-WHY-BOTH-CHECK-AND-RESERVATION-EXIST -->

The pure check is still useful even though the reservation is authoritative for actual consumption.

The check provides:

- early errno selection;
- framework-consistent authorization structure;
- a place to attach policy logic not reducible to a simple atomic decrement.

In particular, the check may evaluate:

- soft-limit admissibility,
- hard-limit transition rules,
- cred-gated policy such as privileged hard-limit raises.

The reservation provides:

- linearization under concurrency;
- rollback on step failure;
- the actual transition in committed usage.

### Linearity and drop discipline
<!-- txdoc:RLIMIT-LINEARITY-AND-DROP-DISCIPLINE -->

Credit reservations are linear reservation objects. They must be either committed or dropped; code outside substrate must not `mem::forget` or otherwise suppress their drop path. This is the same linear-reservation discipline used elsewhere in the framework.

---

## Example: fd creation
<!-- txdoc:RLIMIT-EXAMPLE-FD-CREATION -->

The following is a walkthrough of how a pipe/fd-creating step uses rlimit. The step itself belongs to the pipe/fd subsystem, not to rlimit.

### Shape
<!-- txdoc:RLIMIT-SHAPE -->

```rust
fn step_pipe_create(proc: Cap<ProcessIdentity>) -> StepOutcome<(Fd, Fd)> {
    let guard = epoch::guard();

    // Phase 1: observe — rlimit admissibility.
    let limits_ref = proc.policy().rlimits_ref(&guard);
    let usage_ref = proc.policy().rlim_usage_ref(&guard);
    let _rlim_w = rlim::checks::require_fd_creation(limits_ref, usage_ref, 2, &guard)?;
    // ... other subsystem checks

    // Phase 2: upgrade.
    let usage_cap = proc.policy().rlim_usage_cap();

    // Phase 3: reserve — rlimit consumption linearizes here.
    let charge = substrate::credit::reserve(usage_cap.clone(), RLimitKind::NoFile, 2)?;
    // ... other subsystem reservations

    // Phase 4: commit — rlimit charge finalizes alongside semantic commit.
    substrate::credit::commit(charge);
    // ... pipe publication, fd installation

    // Phase 5: publish.
    // ... subsystem publication

    StepOutcome::Done((fd0, fd1))
}
```

### What this example shows
<!-- txdoc:RLIMIT-WHAT-THIS-EXAMPLE-SHOWS -->

1. rlimit contributes a pure admissibility check;
2. the actual linearization happens in the reserve/commit path;
3. rlimit does not publish the `FdEntry` objects;
4. the owning subsystem remains the semantic owner of descriptor publication;
5. rollback is automatic if the enclosing step aborts before commit.

---

## Execution surface
<!-- txdoc:RLIMIT-EXECUTION-SURFACE -->

Rlimit execution contains limit-changing transitions, not ordinary consumption.

Representative steps:

- `step_setrlimit`
- `step_prlimit`

In v1, `prlimit` should be read as the self-only or tightly-scoped form. Full cross-process mutation semantics may be deferred if the required privilege and target-lifetime rules are not yet wanted.

A representative shape is:

```rust
fn step_setrlimit(
    proc: Cap<ProcessIdentity>,
    kind: RLimitKind,
    new_soft: u64,
    new_hard: u64,
    caller_cred: &Credential,
) -> StepOutcome<()> {
    let guard = epoch::guard();

    // Phase 1: observe.
    let current = proc.policy().rlimits_ref(&guard);
    let _w = rlim::checks::require_setrlimit(
        current,
        caller_cred,
        kind,
        new_soft,
        new_hard,
        &guard,
    )?;

    // Phase 2: upgrade.
    let old_cap = proc.policy().rlimits_cap();

    // Phase 3: reserve.
    let new_slot = substrate::zone::reserve::<RLimitBag>()?;
    let new_value = RLimitBag::recompute_set_limit(&old_cap, kind, new_soft, new_hard);

    // Phase 4: commit.
    let new_bag = substrate::zone::sign(new_slot, new_value);
    proc.policy_mut().replace_rlimits(new_bag.clone());

    // Phase 5: publish.
    rlim::trace_limit_changed(proc.pid(), kind, new_soft, new_hard);

    StepOutcome::Done(())
}
```

The process-side accessor names are illustrative. This document does not fix the exact process-subsystem API spelling.

---

## Lifecycle rules
<!-- txdoc:RLIMIT-LIFECYCLE-RULES -->

`fork` produces a **new rlimit ledger object with copied value** for the child. `RLimitBag` is small process-local policy state, not a structurally meaningful shared object whose identity must persist across divergent mutation histories. A `Shared<RLimitBag>` design would merely defer the copy until first mutation while introducing permanent COW machinery and the invariant that “current limits may be shared.” Eager copy is simpler.

Threads in one process share the same current `RLimitBag` and `RLimitUsage` through process policy.

`execve` preserves current rlimit state.

---

## Projections
<!-- txdoc:RLIMIT-PROJECTIONS -->

Rlimit exposes read-only projections of a process's current limit state.

These projections are consumed by interfaces such as:

- `/proc/<pid>/limits`
- `getrlimit`
- `prlimit`

The important rules are:

- projections are read-only;
- projections are metadata-only;
- projections do not mutate or reserve;
- user-visible formatting lives behind `rlim::project` rather than being re-derived ad hoc.

A representative surface is:

```rust
impl RLimitBag {
    pub fn proc_limits_projection(&self, usage: &RLimitUsage) -> ProcLimitsView;
    pub fn single_limit_projection(&self, usage: &RLimitUsage, kind: RLimitKind) -> RLimitView;
}
```

---

## Forward compatibility
<!-- txdoc:RLIMIT-FORWARD-COMPATIBILITY -->

The core types may represent more limit kinds than v1 actively enforces. This is intentional: only the explicitly scoped v1 subset is guaranteed to participate in checks, reservations, and releases, but adding more enforced kinds later should not require reshaping the core objects.

---

## Non-goals
<!-- txdoc:RLIMIT-NON-GOALS -->

This design does not introduce:

- reusable grant objects derived from rlimit state;
- script-side rlimit decision logic;
- global quota management;
- replacement of subsystem-local reserve/commit discipline.

---

## Short version
<!-- txdoc:RLIMIT-SHORT-VERSION -->

> txKernel resource limits use a ledger-and-reservation model. A durable `RLimitBag` records per-process ceilings. A stable `RLimitUsage` object records committed usage. Pure checks answer whether an operation is admissible in principle. Substrate credit reservations and releases linearize actual consumption, and a reservation commits only when the enclosing semantic operation commits.

