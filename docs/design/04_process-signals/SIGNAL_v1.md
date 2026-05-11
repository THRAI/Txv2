# POSIX Signal Compatibility Shim — v1

<!-- txdoc:04-PROCESS-SIGNALS-SIGNAL-V1 -->

## Status

<!-- txdoc:SIGNAL-STATUS-1 -->

Draft v1.

This document specifies the **POSIX signal compatibility shim**: the layer that presents POSIX-compliant signal semantics to userspace over txKernel's factored Gewalt-vs-event native architecture. It is a shim specification; the native mechanisms it translates onto are specified elsewhere.

Companion documents:

- [`REACTOR_v0.md`](../02_execution/REACTOR_v0.md) — reactor contract, preemption mechanism, `request_userspace_run`.
- [`THREAD_RUNTIME_v1.md`](../02_execution/THREAD_RUNTIME_v1.md) — thread state, two-site delivery discipline, signal-state placement, stop state.
- [`PROCESS_v1.md`](./PROCESS_v1.md) — process model, Frame layout, GroupExit coordination, SIGCHLD wiring.
- [`SCHEDULER_v0.md`](../02_execution/SCHEDULER_v0.md) — scheduler policy (for SIGSTOP/SIGCONT interaction).
- [`BUS_v1.md`](../01_substrate/BUS_v1.md) — RawPort, RawQueue, Subscription primitives.
- [`SIGNAL_ATTACHMENTS_v1.md`](./SIGNAL_ATTACHMENTS_v1.md) — publication catalog; `signal_port`, `exit_source`, `signalfd_readable`, etc.
- [`object_model_v2.md`](../00_meta-framework/object_model_v2.md) and [`EBR_ZONE_INTERFACE_v1.md`](../01_substrate/EBR_ZONE_INTERFACE_v1.md) — `Binding<T>`, `Weak<T>`, and retention evidence vocabulary.
- [`01_CONCEPTS_v5.md`](../../Txv3/01_CONCEPTS_v5.md) — planes, scripts, publication, and factoring vocabulary.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — SIG-*, STEP-*, SCRIPT-* rules this spec respects.

### What this document pins

<!-- txdoc:SIGNAL-WHAT-THIS-DOCUMENT-PINS-1 -->

- The Gewalt/event factoring as architectural commitment, not rhetorical framing.
- `sig_actions` as a routing table; sigaction as a routing-configuration syscall.
- `PendingSignalQueue` structure for catchable signals (standard bitset + RT FIFO).
- The delivery-selection algorithm run at yield-adapt and at AST.
- `deliver_posix_signal` as the central producer entry point.
- `deliver_synchronous_fault` as the trap-context entry point for hardware faults.
- The trap-return signal contract consumed through `KernelTrapSink<P>` and HAL `SignalFrameIf`.
- Multistage handler flow (kernel → user handler → kernel sigreturn → user pre-signal).
- SA_RESTART / ERESTARTSYS mechanics.
- POSIX syscall catalog: sigaction, sigprocmask, sigpending, sigsuspend, sigaltstack, sigreturn, kill, tkill, tgkill, raise, abort, sigqueue, sigtimedwait.
- signalfd and pidfd as thin bus-adapter fds (no private state duplication).

### Zone-derived type policy

<!-- txdoc:SIGNAL-ZONE-DERIVED-TYPE-POLICY-1 -->

SIGNAL is a compatibility shim, not an entity-owning subsystem. It routes and
updates state owned by PROCESS, THREAD_RUNTIME, and fd/page-backed subsystems:

| Signal-layer declaration | Public handle | Reclamation role |
|---|---|---|
| process signal target | `Cap<ProcessIdentity>` or witness-derived `IdentRef<'g, ProcessIdentity>` | addressability for routing |
| thread signal target | `Cap<ThreadIdentity>` plus payload upgrade when delivery needs runtime state | addressability then operational payload use |
| `SigActionTable` / pending queues | values on ProcessPayload or ThreadPayload | payload state, no independent zone |
| signalfd / pidfd backing | owning fd/RNode subsystem's cap/evidence | adapter object, not duplicated signal truth |
| delivery witnesses | witness types carrying `IdentRef<'g, ...>` | EBR-scoped observation only |

Signal code therefore never selects a zone policy. It consumes role-shaped
evidence supplied by the owning subsystems and re-observes after bus wakes.

### What this document does not pin

<!-- txdoc:SIGNAL-WHAT-THIS-DOCUMENT-DOES-NOT-PIN-1 -->

- POSIX timers (`timer_create`, `setitimer`) — future POSIX_TIMERS_v1.
- POSIX message queues (`mq_notify`) — future POSIX_MQUEUE_v1.
- Ptrace-signal interaction — observation subsystem.
- VDSO sigreturn entry — Phase 2 (stack-based trampoline used in Phase 1).
- Per-user RLIMIT_SIGPENDING enforcement — Phase 2.
- SIGIO / SIGURG end-to-end async-I/O wiring — OpenFile.fown noted, full pipeline deferred.
- Cross-pidnamespace signal routing — Phase 2 multi-ns.
- Native event-queue API (eventfd-generalized, NOTIFY_v1 / EVENT_QUEUE_v1) — future.

---

## Part I: Introduction and ontology

<!-- txdoc:SIGNAL-PART-I-INTRODUCTION-ONTOLOGY-1 -->

### 1. The Gewalt/event factoring

<!-- txdoc:SIGNAL-THE-GEWALT-EVENT-FACTORING-1 -->

POSIX conflates two distinct primitives under a single API. txKernel factors them apart structurally; this spec describes the compatibility shim that lets POSIX programs see a unified signal surface.

**Gewalt** — kernel-imposed control-flow transitions on a thread or process. The thread-as-evaluator is operated on; it does not "receive" anything. Victims of SIGKILL cease; of SIGSTOP pause below handler level; of synchronous faults transition to error continuations. No handlers, no masks, no pending queues — these are metalevel operations.

**Events** — typed records produced by subsystems (child exit, timer expiry, pipe break, I/O ready, urgent data), consumed at thread discretion via queues. Control flow is never hijacked; events wait for the thread to check.

Under the Gewalt/event decomposition, POSIX signals split roughly:

| Category | POSIX signals | Native realization |
|---|---|---|
| Gewalt | SIGKILL, SIGSTOP, SIGCONT, synchronous faults (SIGSEGV, SIGBUS, SIGFPE, SIGILL) with SIG_DFL | Control primitives in PROCESS_v1, THREAD_RUNTIME_v1, SCHEDULER_v0 |
| Event | SIGCHLD, SIGALRM, SIGIO, SIGWINCH, SIGURG, SIGPIPE, SIGUSR1, SIGUSR2, SIGRTMIN..SIGRTMAX | Bus publications per SIGNAL_ATTACHMENTS_v1 + POSIX routing through this spec |
| Hybrid (control-op-with-handler-override) | Synchronous faults with handler installed; SIGTERM with handler | Native control op consulted; shim handler path taken if configured |

The shim's job is: translate POSIX signal producers and consumers onto the two-primitive reality. Programs that use native-event APIs (pidfd, signalfd, timerfd) bypass most of the shim; programs that use sigaction get the shim's full handler-dispatch machinery.

### 2. Why this factoring matters for the spec

<!-- txdoc:SIGNAL-WHY-THIS-FACTORING-MATTERS-SPEC-1 -->

Three concrete consequences of the factoring shape this spec:

**Consequence 1: Control operations are specified elsewhere, cited here.**

The spec does not re-specify step_thread_exit (THREAD_RUNTIME_v1 §7), GroupExit (PROCESS_v1 §5), or stop_state transitions (THREAD_RUNTIME_v1 §6). When userspace calls `kill(pid, SIGKILL)`, the shim invokes GroupExit. When `kill(pid, SIGSTOP)` is called, the shim invokes the group-stop control op. These mechanisms exist independent of signals; the shim is a caller.

**Consequence 2: Pending queues carry only catchable signals.**

SIGKILL / SIGSTOP / SIGCONT do not enter pending queues. They are routed directly to control ops at the syscall entry point. The delivery-selection algorithm never encounters them, which simplifies the algorithm and removes impossible cases.

**Consequence 3: Native event consumers bypass pending queues.**

signalfd reads drain directly from `target.group_pending` (when a POSIX program uses signalfd for a process-directed signal) OR subscribe to native bus wires (when available). pidfd and timerfd are not POSIX-signal constructs at all; they expose native events through fd-shaped reads. The signal spec references these for completeness but doesn't own their semantics.

### 3. This document's structure

<!-- txdoc:SIGNAL-THIS-DOCUMENT-S-STRUCTURE-1 -->

- Part I (this): factoring and consequences.
- Part II: sig_actions as a routing table; per-signal disposition; `SigActionTable` structure and interface; `sigaction` syscall.
- Part III: Mask discipline and `sigprocmask`.
- Part IV: Pending queues; `PendingSignalQueue` structure; `post` and `dequeue`; standard vs RT semantics.
- Part V: Delivery mechanics — routing producers, selection algorithm, AST, signal frame, handler invocation, sigreturn, multistage handler flow.
- Part VI: Cross-cutting behaviors — SA_RESTART and ERESTARTSYS, synchronous-fault routing, stop/continue interaction, SIGCHLD, SIGPIPE, forbidden routings.
- Part VII: Syscall catalog.
- Part VIII: Native consumers — signalfd and pidfd as thin bus adapters.
- Part IX: Substrate catalog — what primitives this spec uses and where they come from.
- Part X: POSIX alignment (committed, deferred).
- Part XI: Open questions.

---

## Part II: sig_actions as a routing table

<!-- txdoc:SIGNAL-PART-II-SIG-ACTIONS-AS-ROUTING-TABLE-1 -->

### 4. SigActionTable

<!-- txdoc:SIGNAL-SIGACTIONTABLE-1 -->

Per-process signal-disposition table. Lives on `Frame.sig_actions: Shared<SigActionTable>` per PROCESS_v1 §3. Shared across threads in a process; shared across processes when CLONE_SIGHAND is passed; COW-copied at fork otherwise.

```rust
pub const NSIG: usize = 64;

pub struct SigActionTable {
    /// One atomic entry per signal number. Per-signal atomicity means
    /// sigaction on signum S does not contend with sigaction on signum T.
    entries: [AtomicSigActionEntry; NSIG],
}

/// Entry shape. 16 bytes (on 64-bit) packed to fit a single atomic CAS.
#[repr(C)]
pub struct SigActionEntry {
    pub disposition: Disposition,
    pub flags: SaFlags,
    pub sa_mask: SignalMask,
    // Note: sa_restorer (glibc-private trampoline ptr) is not stored here;
    // Phase 1 writes trampoline bytes on the user stack at delivery time
    // (see §16.3).
}

pub enum Disposition {
    Default,                                 // SIG_DFL; per-signal default action applies
    Ignore,                                  // SIG_IGN; signal dropped at routing
    Handler(UserPtr<SignalHandlerFn>),       // 1-arg handler: fn(Signum)
    SigInfoHandler(UserPtr<SigInfoHandlerFn>), // SA_SIGINFO 3-arg: fn(Signum, &SigInfo, &UContext)
}

pub struct SaFlags(u32);
impl SaFlags {
    pub const NOCLDSTOP: Self;   // SIGCHLD: suppress on child stop/continue
    pub const NOCLDWAIT: Self;   // SIGCHLD: auto-reap children
    pub const SIGINFO: Self;     // handler is SigInfoHandler (3-arg)
    pub const RESTART: Self;     // restart interruptible syscalls
    pub const NODEFER: Self;     // don't add this signal to mask during handler
    pub const RESETHAND: Self;   // reset to SIG_DFL after first delivery
    pub const ONSTACK: Self;     // use sigaltstack if configured
}
```

The atomic per-entry wrapper is packed-CAS of a 128-bit value (on 64-bit platforms). Per [Finding 1 from our review], per-entry CAS avoids table-level contention; `SigActionTable` is not protected by any lock.

#### 4.1 Operations

<!-- txdoc:SIGNAL-OPERATIONS-1 -->

```rust
impl SigActionTable {
    /// Read the current entry for `sig`. Atomic.
    pub fn get(&self, sig: Signum) -> SigActionEntry;

    /// Replace the entry for `sig`. Returns the old entry. Atomic.
    pub fn set(&self, sig: Signum, new: SigActionEntry) -> SigActionEntry;

    /// CAS: replace only if current matches expected_old.
    /// Rarely needed externally; sigaction uses `set`.
    pub fn cas(
        &self,
        sig: Signum,
        expected_old: SigActionEntry,
        new: SigActionEntry,
    ) -> Result<(), SigActionEntry>;

    /// exec reset per POSIX: for each entry, if disposition is Handler
    /// or SigInfoHandler, reset to Default (SIG_DFL). Ignore entries stay
    /// Ignore; Default stays Default. Flags reset to empty; sa_mask cleared.
    /// Called from step_execve's publish phase (PROCESS_v1 §7.2).
    pub fn exec_reset(&self);
}
```

#### 4.2 Sharing semantics

<!-- txdoc:SIGNAL-SHARING-SEMANTICS-1 -->

Per PROCESS_v1 §3.1, `Shared<SigActionTable>` supports:

- **`share`** (CLONE_SIGHAND set): increment refcount; return same table. Sibling threads see each other's `sigaction` updates immediately (per-entry atomic).
- **`fork_copy`** (fork without CLONE_SIGHAND): structurally-shared root copy; first mutation after fork triggers full-table COW. Practically, since fork commits before either side typically calls sigaction, the COW happens on first sigaction by either side.
- **`get`**: guarded read-reference; used by the delivery algorithm.
- **`get_mut`**: write access; triggers COW if refcount > 1. Used rarely (sigaction uses `get` then per-entry CAS).

Because per-entry mutations are atomic, sigaction doesn't need `get_mut` — it acquires `get` (shared reference) and performs per-entry CAS on the atomic entry. COW is only triggered by operations that rewrite the whole table structure, which doesn't happen in the signal hot path.

### 5. The sigaction syscall

<!-- txdoc:SIGNAL-THE-SIGACTION-SYSCALL-1 -->

```rust
pub fn sys_sigaction(
    sig: Signum,
    act: Option<UserPtr<SigActionEntry>>,
    oldact: Option<UserPtr<SigActionEntry>>,
) -> Result<(), Errno>;
```

#### 5.1 Validation

<!-- txdoc:SIGNAL-VALIDATION-1 -->

Per POSIX and our Gewalt factoring:

- SIGKILL (9) and SIGSTOP (19) cannot have dispositions set. If `act` is `Some` and `sig` is SIGKILL or SIGSTOP, return `EINVAL`.
- Reading SIGKILL or SIGSTOP (`act` is None, `oldact` is Some) returns `SigActionEntry { disposition: Default, flags: empty, sa_mask: empty }` — their nominal defaults.
- Signals outside `[1, NSIG)` are invalid: `EINVAL`.

This is consistent with "SIGKILL has no handler because there's nothing for the victim to handle" — the routing table simply does not route these signals, and userspace cannot configure them to.

#### 5.2 Flow

<!-- txdoc:SIGNAL-FLOW-1 -->

```rust
async fn script_sigaction(sig: Signum, act: Option<&SigActionEntry>, oldact: Option<&mut SigActionEntry>)
    -> Result<(), Errno>
{
    // Validate
    if sig == SIGKILL || sig == SIGSTOP {
        if act.is_some() { return Err(Errno::EINVAL); }
        // Reads of SIGKILL/SIGSTOP return nominal defaults without touching the table.
        if let Some(o) = oldact { *o = NOMINAL_UNCATCHABLE; }
        return Ok(());
    }
    if sig.is_invalid() { return Err(Errno::EINVAL); }

    let proc = current_process();
    let table_ref = proc.payload.frame.sig_actions.get(&guard);

    // Atomic: swap the entry. Returns old.
    let old = if let Some(new_entry) = act {
        table_ref.set(sig, *new_entry)
    } else {
        table_ref.get(sig)   // read-only query
    };

    if let Some(out) = oldact {
        *out = old;
    }

    Ok(())
}
```

No reservations; pure atomic swap. No step-phase complexity. The linearization point is the atomic entry swap.

#### 5.3 Interaction with in-flight deliveries

<!-- txdoc:SIGNAL-INTERACTION-FLIGHT-DELIVERIES-1 -->

A sigaction call that replaces a handler with SIG_IGN (for instance) can race with an ongoing signal post. The race is benign:

- If the post reads the old disposition (handler) before the CAS: routes to pending per handler rules; next AST pass observes the new disposition and either delivers via the new handler or drops (if SIG_IGN).
- If the post reads the new disposition (SIG_IGN) after the CAS: drops.

Both outcomes are POSIX-compatible. Programs that need strict ordering must serialize at the application level (typically by blocking the signal first, then sigaction-ing it, then unblocking).

---

## Part III: Mask discipline

<!-- txdoc:SIGNAL-PART-III-MASK-DISCIPLINE-1 -->

### 6. Signal masks

<!-- txdoc:SIGNAL-SIGNAL-MASKS-1 -->

Each thread has a signal mask on `ThreadPayload.signal_mask: AtomicSignalMask`. The mask filters which signals will be delivered to the thread. Signals that are pending but masked remain pending; they deliver when unmasked.

```rust
pub struct SignalMask(u64);

impl SignalMask {
    pub const EMPTY: Self = SignalMask(0);
    pub const ALL: Self = SignalMask(!0);

    pub fn contains(self, sig: Signum) -> bool { /* bit test */ }
    pub fn with(self, sig: Signum) -> Self { /* bit set */ }
    pub fn without(self, sig: Signum) -> Self { /* bit clear */ }
    pub fn union(self, other: Self) -> Self;
    pub fn intersect(self, other: Self) -> Self;
    pub fn complement(self) -> Self;
}

pub struct AtomicSignalMask(AtomicU64);

impl AtomicSignalMask {
    pub fn load(&self) -> SignalMask;
    pub fn store(&self, m: SignalMask);
    pub fn swap(&self, m: SignalMask) -> SignalMask;
    pub fn or(&self, bits: SignalMask);    // SIG_BLOCK
    pub fn and_not(&self, bits: SignalMask); // SIG_UNBLOCK
    pub fn compare_exchange(&self, old: SignalMask, new: SignalMask) -> Result<(), SignalMask>;
}
```

### 6.1 Uncatchable signals bypass the mask

<!-- txdoc:SIGNAL-UNCATCHABLE-SIGNALS-BYPASS-MASK-1 -->

SIGKILL and SIGSTOP cannot be added to the mask. Attempts via sigprocmask silently clear those bits from the requested new mask before applying it (per POSIX: "If SIGKILL or SIGSTOP is specified ... these signals shall be silently ignored.").

Synchronous-fault signals (SIGSEGV, SIGBUS, SIGFPE, SIGILL) *can* be masked via sigprocmask, but the mask does not protect the thread: if the thread synchronously generates one of these with the signal masked and disposition SIG_DFL or SIG_IGN, the action is force-terminate (see §16).

### 7. The sigprocmask syscall

<!-- txdoc:SIGNAL-THE-SIGPROCMASK-SYSCALL-1 -->

```rust
pub fn sys_sigprocmask(
    how: SigHow,
    set: Option<UserPtr<SignalMask>>,
    oldset: Option<UserPtr<SignalMask>>,
) -> Result<(), Errno>;

pub enum SigHow {
    Block,     // new_mask = old_mask ∪ set
    Unblock,   // new_mask = old_mask ∩ ¬set
    SetMask,   // new_mask = set
}
```

#### 7.1 Flow

<!-- txdoc:SIGNAL-FLOW-2 -->

```rust
async fn script_sigprocmask(how: SigHow, set: Option<&SignalMask>, oldset: Option<&mut SignalMask>)
    -> Result<(), Errno>
{
    let thread = current_thread();
    let mut loaded = thread.payload.signal_mask.load();

    if let Some(out) = oldset { *out = loaded; }

    if let Some(req) = set {
        // Strip SIGKILL, SIGSTOP from the requested set per POSIX.
        let safe_set = req.without(SIGKILL).without(SIGSTOP);

        let new_mask = match how {
            SigHow::Block   => loaded.union(safe_set),
            SigHow::Unblock => loaded.intersect(safe_set.complement()),
            SigHow::SetMask => safe_set,
        };

        thread.payload.signal_mask.store(new_mask);

        // After mask change, re-evaluate deliverability.
        // If unmasking made a pending signal deliverable, update summary.
        if matches!(how, SigHow::Unblock | SigHow::SetMask) {
            refresh_signal_summary(&thread);
        }
    }

    Ok(())
}
```

`refresh_signal_summary` recomputes the `has_deliverable` bit on `signal_summary` by examining `thread_pending` and `group_pending` minus `signal_mask`. If any catchable signal is pending and unmasked, set the bit. This drives yield-adapt's interrupt detection (see §13).

#### 7.2 pthread_sigmask

<!-- txdoc:SIGNAL-PTHREAD-SIGMASK-1 -->

`pthread_sigmask` is identical to `sigprocmask` from the kernel's perspective. Both operate on the current thread's mask. The distinction exists in glibc/NPTL for historical reasons (sigprocmask is undefined behavior in multi-threaded processes per POSIX; pthread_sigmask is the supported form). Kernel-side: same syscall.

---

## Part IV: Pending queues

<!-- txdoc:SIGNAL-PART-IV-PENDING-QUEUES-1 -->

### 8. PendingSignalQueue structure

<!-- txdoc:SIGNAL-PENDINGSIGNALQUEUE-STRUCTURE-1 -->

The shim maintains two pending queues per process:

- `ThreadPayload.thread_pending: PendingSignalQueue` — thread-directed pending (from tkill, tgkill to this tid, synchronous faults targeting this thread).
- `ProcessPayload.group_pending: PendingSignalQueue` — process-directed pending (from kill, SIGCHLD, SIGPIPE on write by thread, etc.).

A "pending" signal is one whose disposition is Handler or SigInfoHandler (i.e., routes to handler delivery) that has been posted but not yet delivered.

Signals with disposition Default or Ignore where the default is "drop" are not enqueued; they are resolved at post time (dropped for Ignore; default-action applied for Default).

```rust
pub struct PendingSignalQueue {
    // Standard signals (1..32): keep-first semantics.
    // Bitset indicates which are pending; siginfo slot stores the first-arrived siginfo.
    std_bits: AtomicU32,
    std_siginfos: [AtomicCell<Option<SigInfo>>; 32],

    // RT signals (SIGRTMIN..SIGRTMAX, i.e., 32..64): FIFO per-signum.
    // Each ring holds up to RT_RING_CAP entries; overflow drops newest with
    // a per-thread overflow counter (for diagnostic / RLIMIT_SIGPENDING in Phase 2).
    rt_queues: [BoundedRing<SigInfo, RT_RING_CAP>; NUM_RT],
}

const RT_RING_CAP: usize = 32;     // per-signum; RLIMIT_SIGPENDING total deferred
const NUM_RT: usize = 32;           // SIGRTMIN..SIGRTMAX
```

`AtomicCell<Option<SigInfo>>` is a single-slot atomic container; implementation is an atomic pointer into a siginfo slab inside the queue, or an inline structure with a CAS-guarded "populated" flag. Internal to the signal subsystem.

`BoundedRing<T, N>` is an MPSC bounded ring with:
- `push(entry) -> Result<(), Overflow>` — post-side, many-producers.
- `pop() -> Option<T>` — delivery-side, single consumer (the enqueuer per delivery selection).
- `is_empty() -> bool`, `len() -> usize` — query.

If tx-fnd/bounded-ring exists, use it. Otherwise, inline (it's internal to signal subsystem). The substrate-usage catalog (§Part IX) names this as the only signal-internal data structure not reducible to existing primitives.

### 9. Pending queue operations

<!-- txdoc:SIGNAL-PENDING-QUEUE-OPERATIONS-1 -->

```rust
impl PendingSignalQueue {
    /// Post a signal with siginfo. Returns whether this is the first instance
    /// of this signal newly-pending (for summary-bit updates).
    pub fn post(&self, sig: Signum, siginfo: SigInfo) -> PostOutcome;

    /// Dequeue an instance of `sig`. For standard: clears the bit, returns
    /// the stored siginfo. For RT: pops oldest. Returns None if not pending.
    pub fn dequeue(&self, sig: Signum) -> Option<SigInfo>;

    /// Bitmap of currently-pending signals (for sigpending).
    pub fn pending_mask(&self) -> SignalMask;

    /// Find the next deliverable signal given a mask filter.
    /// Returns the lowest-numbered deliverable signum, None if none.
    /// Used by the delivery-selection algorithm (§14).
    pub fn next_deliverable(&self, mask_filter: SignalMask) -> Option<Signum>;
}

pub enum PostOutcome {
    NewlyPending,           // standard signal: bit was clear → set; or RT: first instance
    AlreadyPending,         // standard signal: bit already set; siginfo kept as-is (keep-first)
    QueuedAdditionally,     // RT: additional instance appended
    Overflow,               // RT: ring full; diagnostic
}
```

### 10. Post semantics

<!-- txdoc:SIGNAL-POST-SEMANTICS-1 -->

```rust
fn post(&self, sig: Signum, siginfo: SigInfo) -> PostOutcome {
    if sig.is_rt() {
        match self.rt_queues[sig.rt_index()].push(siginfo) {
            Ok(()) => {
                let was_empty = /* was first entry */;
                if was_empty { PostOutcome::NewlyPending }
                else { PostOutcome::QueuedAdditionally }
            }
            Err(_) => PostOutcome::Overflow,
        }
    } else {
        // Standard signal: set bit; store siginfo only if newly-pending.
        let bit = 1u32 << sig.as_u8();
        let prev = self.std_bits.fetch_or(bit, AcqRel);
        if prev & bit == 0 {
            // Newly pending; store siginfo.
            self.std_siginfos[sig.as_u8() as usize].store(Some(siginfo));
            PostOutcome::NewlyPending
        } else {
            // Already pending; retain existing siginfo (keep-first).
            PostOutcome::AlreadyPending
        }
    }
}
```

### 11. Standard vs RT semantics

<!-- txdoc:SIGNAL-STANDARD-RT-SEMANTICS-1 -->

POSIX distinguishes:

**Standard signals (1..32)**: coalescing. "If multiple instances of a standard signal are generated while it is blocked, POSIX permits the implementation to deliver just one instance when unblocked." We use keep-first: the first post's siginfo is retained; subsequent posts set the bit (already set) and discard their siginfo.

**Rationale for keep-first**: matches Linux. Alternative (keep-last) would overwrite, costing slightly more for the rare case where the last is more useful. First-arrived is typically the "cause" siginfo in most use cases.

**Real-time signals (SIGRTMIN..SIGRTMAX)**: queued. Every post is distinct; FIFO ordered per signum. siginfos fully preserved. RLIMIT_SIGPENDING (Phase 2) will cap total queued RT siginfos per user.

POSIX delivery priority among pending signals:

- Synchronous faults first (these are direct-delivery, not queue-selected; see §16).
- Among catchable pending signals: lowest signum wins; for RT signals of the same number, FIFO order.
- Standard signals (lower numbers) are delivered before RT signals when both pending and not masked.

The delivery-selection algorithm (§14) implements this ordering.

---

## Part V: Delivery mechanics

<!-- txdoc:SIGNAL-PART-V-DELIVERY-MECHANICS-1 -->

### 12. The central producer entry point: `deliver_posix_signal`

<!-- txdoc:SIGNAL-THE-CENTRAL-PRODUCER-ENTRY-POINT-DELIVER-POSIX-SIGNAL-1 -->

All POSIX-signal production across the kernel flows through one entry point:

```rust
pub fn deliver_posix_signal(
    target: SignalTarget,
    signum: Signum,
    siginfo: SigInfo,
) -> DeliveryOutcome;

pub enum SignalTarget {
    Thread(Cap<ThreadIdentity>),
    Process(Cap<ProcessIdentity>),
    ProcessGroup(Cap<ProcessGroup>),
}

pub enum DeliveryOutcome {
    ControlOpInvoked,   // SIGKILL/SIGSTOP/SIGCONT routed to control primitive
    PendingEnqueued,    // catchable signal routed to pending queue
    Ignored,            // SIG_IGN; dropped after sig_actions consult
    ForcedTerminate,    // SIG_DFL with Term default; direct termination invoked
    DefaultStop,        // SIG_DFL with Stop default; stop control op invoked
    DefaultContinue,    // SIG_DFL with Cont default; continue control op invoked
    NativeEventOnly,    // signal_port fired for native subscribers; nothing further
}
```

Callers: every subsystem that produces a POSIX signal. See §21 for the producer catalog.

**On thread-level primitives and process-level dispatch.** `deliver_posix_signal`'s routing is explicit about a structural truth: **Gewalt is thread-level; process-level operations are fan-out compositions**. Every semantic action this entry point performs ultimately acts on a specific thread's state — its `signal_summary`, its `thread_pending`, its `stop_state`, its reactor waker. When the target is `SignalTarget::Process(_)` or `SignalTarget::ProcessGroup(_)`, the routing code iterates the appropriate threads DLL (provided by the process entity as the dispatcher) and applies thread-level primitives to each. The process is the **container and iteration target**, not the semantic unit of signal delivery. See PROCESS_v1 §1.3 "The thread/process provision" for the full framing; this section cashes it out in the specific case of signal routing.

### 12.1 Routing logic

<!-- txdoc:SIGNAL-ROUTING-LOGIC-1 -->

```rust
pub fn deliver_posix_signal(
    target: SignalTarget,
    signum: Signum,
    siginfo: SigInfo,
) -> DeliveryOutcome {
    // Uncatchable signals bypass the routing table entirely.
    match signum {
        SIGKILL => return route_sigkill(target),
        SIGSTOP => return route_sigstop(target),
        SIGCONT => return route_sigcont(target, &siginfo),
        _ => {}
    }

    // Resolve target to a set of (thread, process) pairs for delivery.
    let targets = expand_target(target);
    let mut outcome = DeliveryOutcome::Ignored;

    for (thread_opt, proc) in targets {
        // Fire native event channel unconditionally — observers subscribe here.
        // (Skipped for SIGKILL/SIGSTOP which bypass; handled above.)
        proc.identity.signal_port.fire(SignalGenerated {
            sig: signum,
            siginfo: siginfo.clone(),
        });

        // Consult routing table on target process.
        let table = proc.payload.frame.sig_actions.get(&guard);
        let entry = table.get(signum);

        match entry.disposition {
            Disposition::Ignore => {
                // Per POSIX: special-case SIGCHLD with SIG_IGN = auto-reap.
                // Handled in step_process_exit (PROCESS_v1 §7.3.3); not here.
                outcome = DeliveryOutcome::Ignored;
            }

            Disposition::Default => {
                match default_action(signum) {
                    DefaultAction::Term | DefaultAction::Core => {
                        // No handler; force termination at process level.
                        invoke_group_exit_with_signal(proc.clone(), signum);
                        outcome = DeliveryOutcome::ForcedTerminate;
                    }
                    DefaultAction::Ignore => {
                        outcome = DeliveryOutcome::Ignored;
                    }
                    DefaultAction::Stop => {
                        invoke_group_stop(proc.clone());
                        outcome = DeliveryOutcome::DefaultStop;
                    }
                    DefaultAction::Cont => {
                        invoke_group_continue(proc.clone());
                        outcome = DeliveryOutcome::DefaultContinue;
                    }
                }
            }

            Disposition::Handler(_) | Disposition::SigInfoHandler(_) => {
                // Route to pending queue for handler delivery.
                let (queue, delivery_thread) = match thread_opt {
                    Some(t) => (&t.payload.thread_pending, t),
                    None => {
                        // Process-directed: pick a thread per THREAD_RUNTIME §5.6.
                        let t = select_target_thread(&proc, signum);
                        (&proc.payload.group_pending, t)
                    }
                };

                let post_outcome = queue.post(signum, siginfo.clone());
                if matches!(post_outcome, PostOutcome::NewlyPending | PostOutcome::QueuedAdditionally) {
                    // Update signal_summary and wake the thread if waiting.
                    mark_deliverable(&delivery_thread, signum);
                }
                outcome = DeliveryOutcome::PendingEnqueued;
            }
        }
    }

    outcome
}
```

### 12.2 Target expansion

<!-- txdoc:SIGNAL-TARGET-EXPANSION-1 -->

```rust
fn expand_target(target: SignalTarget) -> Vec<(Option<Cap<ThreadIdentity>>, Cap<ProcessIdentity>)> {
    match target {
        SignalTarget::Thread(t) => {
            let proc = t.process.load(&guard).expect("thread has process");
            vec![(Some(t), proc)]
        }
        SignalTarget::Process(p) => {
            vec![(None, p)]   // delivery will select a thread
        }
        SignalTarget::ProcessGroup(pg) => {
            let mut result = vec![];
            for entry in pg.members.iter(&guard) {
                let proc = entry.element(&guard).to_cap();
                result.push((None, proc));
            }
            result
        }
    }
}
```

Pgroup expansion iterates `ProcessGroup.members` DLL under epoch guard. Per-member delivery is independent; one member failing does not affect others.

### 12.3 Route helpers for uncatchable signals

<!-- txdoc:SIGNAL-ROUTE-HELPERS-UNCATCHABLE-SIGNALS-1 -->

Uncatchable signals skip the routing table and invoke control primitives directly.

```rust
fn route_sigkill(target: SignalTarget) -> DeliveryOutcome {
    // SIGKILL → invoke GroupExit on each target process.
    for (_, proc) in expand_target(target) {
        // Fire signal_port for observability (tracers, audit).
        proc.identity.signal_port.fire(SignalGenerated {
            sig: SIGKILL,
            siginfo: siginfo_kill(/* from sender */),
        });

        // Directly invoke GroupExit per PROCESS_v1 §5.
        // exit_status encodes "killed by SIGKILL".
        invoke_group_exit(proc, ExitStatus::Signaled(SIGKILL));
    }
    DeliveryOutcome::ControlOpInvoked
}

fn route_sigstop(target: SignalTarget) -> DeliveryOutcome {
    // SIGSTOP → force-stop all threads in each target process.
    for (_, proc) in expand_target(target) {
        proc.identity.signal_port.fire(SignalGenerated { sig: SIGSTOP, siginfo: /* */ });
        invoke_group_stop(proc);
    }
    DeliveryOutcome::ControlOpInvoked
}

fn route_sigcont(target: SignalTarget, siginfo: &SigInfo) -> DeliveryOutcome {
    // SIGCONT → continue stopped threads. Distinctive: SIGCONT may also
    // trigger handler delivery if a handler is installed, but only AFTER
    // the continue itself.
    for (_, proc) in expand_target(target) {
        proc.identity.signal_port.fire(SignalGenerated { sig: SIGCONT, siginfo: siginfo.clone() });

        // Step 1: un-stop threads.
        invoke_group_continue(proc.clone());

        // Step 2: if handler installed, route through pending for handler delivery.
        let table = proc.payload.frame.sig_actions.get(&guard);
        let entry = table.get(SIGCONT);
        match entry.disposition {
            Disposition::Handler(_) | Disposition::SigInfoHandler(_) => {
                let t = select_target_thread(&proc, SIGCONT);
                proc.payload.group_pending.post(SIGCONT, siginfo.clone());
                mark_deliverable(&t, SIGCONT);
            }
            _ => {}  // default or ignore; continue-effect is sufficient
        }
    }
    DeliveryOutcome::ControlOpInvoked
}
```

Control-op helpers (`invoke_group_exit`, `invoke_group_stop`, `invoke_group_continue`) are thin wrappers around mechanisms specified in PROCESS_v1 §5 and THREAD_RUNTIME_v1 §6. The signal spec does not re-specify them.

### 12.4 The SignalEvent carried on signal_port

<!-- txdoc:SIGNAL-THE-SIGNALEVENT-CARRIED-SIGNAL-PORT-1 -->

```rust
// Declaration (catalog entry; see SIGNAL_ATTACHMENTS_v1 §3.3)
pub struct SignalGenerated {
    pub sig: Signum,
    pub siginfo: SigInfo,   // full siginfo payload (~48 bytes on 64-bit)
}
```

`signal_port` fires once per `deliver_posix_signal` invocation regardless of disposition. This is the native event channel for ptrace tracers, signalfds, and audit/strace.

### 13. Marking a thread deliverable

<!-- txdoc:SIGNAL-MARKING-THREAD-DELIVERABLE-1 -->

After `post` enqueues a catchable signal, the target thread's summary must be updated and, if waiting, woken.

```rust
fn mark_deliverable(thread: &Cap<ThreadIdentity>, sig: Signum) {
    let payload = thread.payload.load(&guard).expect("thread live");
    let mask = payload.signal_mask.load();

    // Is this signal deliverable (not masked)?
    if !mask.contains(sig) {
        // Set has_deliverable on summary.
        let mut summary = payload.signal_summary.load();
        summary = summary.with_deliverable();
        payload.signal_summary.store(summary);

        // Wake the thread's task if parked on an Interruptible wait.
        // The wake is via the task's waker; yield-adapt will re-observe
        // and return Interrupted if summary.has_deliverable() is set.
        reactor::wake_task(thread.task_id);
    }
    // If masked: signal stays pending; summary unchanged; thread not woken.
    // When sigprocmask unmasks, refresh_signal_summary re-checks.
}
```

Per REACTOR_v0, `wake_task` is a hint; the thread re-observes on next poll. If the thread was running (not waiting), the wake is a no-op at reactor level, and the thread discovers the deliverable signal at its next AST pass.

### 14. The delivery-selection algorithm

<!-- txdoc:SIGNAL-THE-DELIVERY-SELECTION-ALGORITHM-1 -->

Run at two sites:

- **Site A (yield-adapt)**: when a yield-adapt wake fires, yield-adapt checks if the wake was signal-caused. If so, returns `WaitOutcome::Interrupted` to the calling script.
- **Site B (AST)**: at every kernel→user transition, HAL invokes `ast_check`; signal subsystem runs the selection and either approves userspace-return or installs a handler frame.

Both sites use the same selection:

```rust
fn select_next_signal(thread: &Cap<ThreadIdentity>) -> Option<(Signum, SigInfo)> {
    let payload = thread.payload.load(&guard)?;
    let mask = payload.signal_mask.load();

    // Thread-directed pending first; then group-directed.
    // Within each, lowest signum wins.
    let t_pending = payload.thread_pending.pending_mask();
    let deliverable_t = t_pending.intersect(mask.complement());

    if let Some(sig) = deliverable_t.lowest() {
        let siginfo = payload.thread_pending.dequeue(sig)
            .expect("dequeue must succeed; mask says pending");
        return Some((sig, siginfo));
    }

    let process = resolve_process(thread)?;
    let g_pending = process.payload.group_pending.pending_mask();
    let deliverable_g = g_pending.intersect(mask.complement());

    if let Some(sig) = deliverable_g.lowest() {
        let siginfo = process.payload.group_pending.dequeue(sig)
            .expect("dequeue must succeed; mask says pending");
        return Some((sig, siginfo));
    }

    None
}
```

Thread-directed signals have priority over group-directed; this matches Linux and POSIX intent. Lowest-numbered signum within each queue matches POSIX "low numbers first"; RT signals (higher numbers) naturally come after standard.

### 15. AST and the trap-return signal contract

<!-- txdoc:SIGNAL-AST-TRAP-RETURN-SIGNAL-CONTRACT-1 -->

Every kernel→user transition consults the signal subsystem to decide whether to deliver a handler, terminate the thread, or proceed to userspace. In the axHal-style HAL, this is not a HAL-owned hook slot or runtime vtable. The platform trap shell calls `KernelTrapSink<P>`; the kernel's trap sink calls these named signal functions; HAL remains responsible only for trap-frame and signal-frame ABI helpers through `TrapIf` and `SignalFrameIf`.

```rust
impl SignalSubsystem {
    /// Called at every kernel→user transition after the HAL's trap-return
    /// bookkeeping. Determines whether to return to pre-trap userspace,
    /// redirect to a handler, or initiate termination.
    fn ast_check<P: SignalFrameIf>(
        &self,
        thread: &Cap<ThreadIdentity>,
        tf: TrapFrameMut<'_>,
    ) -> AstOutcome;

    /// Called from KernelTrapSink::on_illegal_or_sync_fault or
    /// KernelTrapSink::on_page_fault after VM declines the fault.
    fn handle_synchronous_fault(
        &self,
        thread: &Cap<ThreadIdentity>,
        fault: FaultInfo,
    ) -> FaultOutcome;
}

pub enum AstOutcome {
    /// No signals pending; trap return proceeds to the pre-trap PC.
    Continue,

    /// Handler installed; SignalFrameIf has configured regs per frame,
    /// so trap return enters the handler.
    DeliverHandler(SignalFrameInfo),

    /// Thread has been marked for termination (signal_summary.termination set).
    /// The trap sink should not return to userspace; thread_future will
    /// observe termination and exit.
    /// In practice this manifests as "task is not re-dispatched to userspace"
    /// — the reactor sees the thread's userspace-run wait resolve with
    /// TrapInfo::FatalSignal (or similar) and proceeds to reap.
    InitiateTermination,
}

pub enum FaultOutcome {
    /// Handler will run; fault is delegated to user.
    DeliverHandler(SignalFrameInfo),

    /// No handler (or SIG_DFL/SIG_IGN with Term default for sync fault).
    /// Force-terminate the thread immediately.
    ForceTerminate,
}

pub struct SignalFrameInfo {
    pub handler_pc: UserPtr<()>,           // handler function address
    pub new_mask: SignalMask,              // mask to set during handler
    pub frame_addr: UserPtr<()>,           // where signal frame was written
    pub return_pc: UserPtr<()>,            // where handler returns to (sigreturn trampoline)
    pub handler_flavor: HandlerFlavor,     // 1-arg vs 3-arg (SA_SIGINFO) dispatch
}

pub enum HandlerFlavor {
    Simple,      // void handler(int sig)
    SigInfo,     // void handler(int sig, siginfo_t*, ucontext_t*)
}

pub struct FaultInfo {
    pub kind: FaultKind,                   // page fault, illegal insn, FP, bus error
    pub address: Option<UserAddr>,         // for page faults
    pub trap_pc: UserAddr,                 // where the faulting instruction was
    pub arch_specific: ArchFaultDetails,
}
```

### 15.1 ast_check implementation

<!-- txdoc:SIGNAL-AST-CHECK-IMPLEMENTATION-1 -->

```rust
impl SignalSubsystem {
    fn ast_check<P: SignalFrameIf>(
        &self,
        thread: &Cap<ThreadIdentity>,
        tf: TrapFrameMut<'_>,
    ) -> AstOutcome {
        let payload = thread.payload.load(&guard).expect("thread live at AST");
        let summary = payload.signal_summary.load();

        // Priority 1: termination.
        if summary.termination() {
            // Thread is being killed. Arrange for thread_future to observe
            // on next poll. Concretely: if thread_future is waiting on
            // request_userspace_run, resolve that wait with FatalSignal
            // TrapInfo; if it is polling another future, the next poll
            // boundary will observe summary.termination and exit.
            return AstOutcome::InitiateTermination;
        }

        // Priority 2: select next deliverable signal.
        let selected = select_next_signal(thread);
        if let Some((sig, siginfo)) = selected {
            let proc = resolve_process(thread).expect("thread has process");
            let table = proc.payload.frame.sig_actions.get(&guard);
            let entry = table.get(sig);

            // If disposition was Handler when posted but changed to SIG_IGN
            // in the meantime, check and drop.
            match entry.disposition {
                Disposition::Ignore => return AstOutcome::Continue,  // drop; loop would re-check
                Disposition::Default => {
                    // Can happen if handler was reset to SIG_DFL between post and delivery.
                    match default_action(sig) {
                        DefaultAction::Ignore => return AstOutcome::Continue,
                        DefaultAction::Term | DefaultAction::Core => {
                            // Terminate now.
                            invoke_group_exit_with_signal(proc, sig);
                            return AstOutcome::InitiateTermination;
                        }
                        _ => { /* shouldn't reach */ return AstOutcome::Continue; }
                    }
                }
                Disposition::Handler(pc) | Disposition::SigInfoHandler(pc) => {
                    // Build signal frame.
                    let frame = build_signal_frame(&payload, sig, siginfo, entry, pc);
                    return AstOutcome::DeliverHandler(frame);
                }
            }
        }

        // Priority 3: stop request.
        if summary.stop_requested() {
            // Thread transitions to Stopped; blocks on stop channel (see THREAD_RUNTIME §6).
            // This requires thread_future engagement (the stop transition is
            // done at site B in thread_future, not at AST). AST simply returns
            // Continue here; thread_future's next poll handles the stop.
            return AstOutcome::Continue;
        }

        AstOutcome::Continue
    }

    fn handle_synchronous_fault(&self, thread: &Cap<ThreadIdentity>, fault: FaultInfo) -> FaultOutcome {
        let sig = map_fault_to_signal(fault.kind);   // SIGSEGV, SIGBUS, SIGFPE, SIGILL
        let siginfo = make_fault_siginfo(sig, &fault);

        let proc = resolve_process(thread).expect("thread has process");
        let table = proc.payload.frame.sig_actions.get(&guard);
        let entry = table.get(sig);

        // Synchronous faults bypass the mask for action-selection.
        // POSIX-undefined territory resolved to: always force-Term if no handler.
        match entry.disposition {
            Disposition::Handler(pc) | Disposition::SigInfoHandler(pc) => {
                // Fire signal_port for observability.
                proc.identity.signal_port.fire(SignalGenerated { sig, siginfo: siginfo.clone() });

                // Build frame and deliver. Handler will either longjmp out or
                // adjust state; if it returns normally, re-executing the faulting
                // instruction likely re-traps (ABI says this is acceptable).
                let payload = thread.payload.load(&guard).expect("thread live at fault");
                let frame = build_signal_frame(&payload, sig, siginfo, entry, pc);
                FaultOutcome::DeliverHandler(frame)
            }
            Disposition::Default | Disposition::Ignore => {
                // SIG_IGN on a synchronous fault is nonsensical per POSIX but
                // not rejected by sigaction; Linux forces Term. We match.
                proc.identity.signal_port.fire(SignalGenerated { sig, siginfo });
                invoke_group_exit_with_signal(proc, sig);
                FaultOutcome::ForceTerminate
            }
        }
    }
}
```

### 16. Signal frame construction

<!-- txdoc:SIGNAL-SIGNAL-FRAME-CONSTRUCTION-1 -->

Abstract. The selected platform owns the per-architecture register layout through `SignalFrameIf`; signal spec owns the POSIX-visible semantic content.

```rust
fn build_signal_frame<P: SignalFrameIf>(
    tf: TrapFrameMut<'_>,
    payload: &ThreadPayload,
    sig: Signum,
    siginfo: SigInfo,
    entry: SigActionEntry,
    handler_pc: UserPtr<()>,
) -> Result<SignalFrameInfo, FaultInfo> {
    // Compute new mask per §16.1.
    let old_mask = payload.signal_mask.load();
    let new_mask = compute_handler_mask(old_mask, sig, entry.sa_mask, entry.flags);

    // Choose stack: alt_stack if SA_ONSTACK and configured, else user stack.
    let stack_top = if entry.flags.contains(SaFlags::ONSTACK) {
        payload.alt_stack.load().and_then(|alt| alt.active_top())
            .unwrap_or_else(|| current_user_sp())
    } else {
        current_user_sp()
    };

    // The selected platform writes:
    //   - register context (GPRs, FPRs, ...)
    //   - siginfo_t (POSIX layout)
    //   - ucontext_t (POSIX layout; includes old_mask)
    //   - sigreturn trampoline bytes (Phase 1; Phase 2 uses VDSO entry point)
    //
    // Layout is HAL's responsibility; signal gives it the semantic inputs.
    let placement = P::write_signal_frame(tf, SignalFrameWrite {
        stack_top,
        sig_no: sig,
        siginfo: siginfo.to_user_abi(),
        old_mask: old_mask.to_user_abi(),
        flags: entry.flags.to_user_abi(),
        handler_pc,
    })?;

    // If SA_RESETHAND: reset disposition to Default after this delivery.
    if entry.flags.contains(SaFlags::RESETHAND) {
        let proc = /* ... */;
        let table = proc.payload.frame.sig_actions.get(&guard);
        let mut reset_entry = entry;
        reset_entry.disposition = Disposition::Default;
        reset_entry.flags = SaFlags(0);
        reset_entry.sa_mask = SignalMask::EMPTY;
        table.set(sig, reset_entry);
    }

    // Update mask atomically.
    payload.signal_mask.store(new_mask);

    Ok(SignalFrameInfo {
        handler_pc,
        new_mask,
        frame_addr: placement.frame_addr,
        return_pc: placement.trampoline_pc,
        handler_flavor: if entry.flags.contains(SaFlags::SIGINFO) {
            HandlerFlavor::SigInfo
        } else {
            HandlerFlavor::Simple
        },
    })
}
```

### 16.1 Mask computation for handler entry

<!-- txdoc:SIGNAL-MASK-COMPUTATION-HANDLER-ENTRY-1 -->

```rust
fn compute_handler_mask(
    old_mask: SignalMask,
    sig: Signum,
    sa_mask: SignalMask,
    flags: SaFlags,
) -> SignalMask {
    // Base: old mask plus handler's sa_mask.
    let mut new_mask = old_mask.union(sa_mask);

    // Additionally block the signal being delivered, unless SA_NODEFER.
    if !flags.contains(SaFlags::NODEFER) {
        new_mask = new_mask.with(sig);
    }

    // SIGKILL and SIGSTOP always unmasked.
    new_mask = new_mask.without(SIGKILL).without(SIGSTOP);

    new_mask
}
```

### 16.2 Alternate signal stack

<!-- txdoc:SIGNAL-ALTERNATE-SIGNAL-STACK-1 -->

Per `sigaltstack(2)`: a thread can configure an alternate stack to be used for signal handlers when the normal stack might overflow (e.g., SIGSEGV on stack overflow).

```rust
pub struct AltSignalStack {
    pub base: UserPtr<u8>,
    pub size: usize,
    pub flags: AltStackFlags,
}

// Stored on ThreadPayload per THREAD_RUNTIME_v1 §2.3:
//   pub alt_stack: AtomicOption<AltSignalStack>,
```

`sys_sigaltstack` is a simple atomic read-old/write-new. Validation: `size >= MINSIGSTKSZ`; base is reasonable userspace address.

At handler delivery: if `SA_ONSTACK` is set for the signal AND `alt_stack` is configured AND `alt_stack` is not already active (prevents nested-alt-stack clobbering), use alt_stack. Otherwise normal user SP.

### 16.3 Trampoline placement (Phase 1)

<!-- txdoc:SIGNAL-TRAMPOLINE-PLACEMENT-PHASE-1-1 -->

The handler returns via a tail call that invokes sigreturn. In Phase 1, we embed the trampoline bytes on the user stack immediately below the signal frame. The HAL writes, for RISC-V:

```
# Stack layout (grows down):
#   ...pre-signal frame...
#   [signal frame:
#      saved regs, siginfo_t, ucontext_t, saved mask]
#   [trampoline:
#      li a7, __NR_rt_sigreturn
#      ecall
#   ]
#   [handler local frame starts here]
```

`return_pc` in `SignalFrameInfo` points at the trampoline bytes. Handler's `ret` lands there, trampoline executes the sigreturn syscall.

Security note: userspace can craft fake frames and call sigreturn directly (known as Sigreturn-Oriented Programming). This is an accepted Linux-compatibility property; Phase 1 does not harden beyond basic frame validation. Phase 2 VDSO-based trampolines add a small amount of hardening (trampoline bytes in a kernel-read-only VDSO page).

### 17. sigreturn

<!-- txdoc:SIGNAL-SIGRETURN-1 -->

`sys_sigreturn` is a special syscall invoked only from the sigreturn trampoline at handler return.

```rust
pub fn sys_sigreturn() -> !;
```

Flow:

```rust
async fn script_sigreturn<P: SignalFrameIf>(tf: TrapFrameMut<'_>) -> ! {
    let thread = current_thread();
    let payload = thread.payload.load(&guard).expect("alive");
    let user_sp = current_user_sp();

    // Read the signal frame from user stack. Sanity-validate the layout
    // but do not harden against crafted frames (Linux-compatibility).
    let frame = P::read_signal_frame(user_sp)
        .unwrap_or_else(|_| {
            // Corrupt frame → SIGSEGV (matches Linux).
            invoke_group_exit_with_signal(current_process(), SIGSEGV);
            unreachable!();
    });

    // Restore pre-signal state.
    payload.signal_mask.store(frame.saved_mask.to_signal_mask());
    P::restore_signal_frame(tf, &frame);

    // Return to pre-signal PC. Next AST pass will evaluate any newly-arrived
    // signals (cascading delivery if another signal is pending and unmasked).
    // Trap return resumes at the pre-signal PC with restored regs.

    // This diverges from thread_future: sigreturn is handled through
    // SignalFrameIf in terms of register restoration, but the syscall dispatch does wake
    // thread_future (it's a TrapInfo::Syscall trap). The script completes
    // immediately after restoring state; thread_future then re-runs AST on
    // the way back out, which may deliver another stacked signal.
    return_from_syscall_with_restored_context();
}
```

### 18. Multistage handler flow

<!-- txdoc:SIGNAL-MULTISTAGE-HANDLER-FLOW-1 -->

The full flow when a handler is installed and a signal arrives:

1. **Signal posted.** `deliver_posix_signal` enqueues to pending; wakes thread if interruptible.
2. **Thread enters kernel.** Via syscall, fault, or interrupt. At kernel→user transition, HAL invokes `ast_check`.
3. **AST selects signal.** Dequeues from pending; reads sig_actions; disposition is Handler.
4. **Frame built on user stack.** HAL writes saved regs, siginfo, ucontext, saved mask, trampoline bytes. Mask updated to include sa_mask + this signal (unless SA_NODEFER).
5. **Handler runs.** HAL's sret lands in handler at `handler_pc`. Handler runs in userspace with modified mask.
6. **Another signal might arrive mid-handler.** If unmasked by sa_mask, delivered at next kernel→user transition (after some trap). Frame stacks on top of current frame. This is nested handler delivery.
7. **Handler returns.** `ret` lands on trampoline; trampoline invokes sigreturn syscall.
8. **sigreturn script runs.** Reads frame; restores regs and mask; returns.
9. **HAL returns to pre-signal context.** sret to pre-signal PC.
10. **AST runs again.** If another signal pending and mask allows, cascade-deliver. Otherwise return to pre-signal userspace.

Steps 3–4 happen at AST; steps 7–9 happen at sigreturn dispatch. Steps 5–6 are pure userspace execution with the new mask in effect. Steps 1–2 and 10 are the kernel boundary.

Nested handlers stack naturally on the user stack. Each nested level has its own signal frame with its own saved mask; sigreturn at each level restores that level's mask.

### 18.1 SA_RESETHAND

<!-- txdoc:SIGNAL-SA-RESETHAND-1 -->

If set: after step 4, the disposition is reset to Default. Subsequent identical signals, if they arrive before the handler re-installs itself, fall through to the default action.

This is an anti-recursion mechanism for handlers that want one-shot semantics. Common pattern: install once, handle once, re-sigaction inside the handler if further handling wanted.

### 18.2 SIGALTSTACK interaction

<!-- txdoc:SIGNAL-SIGALTSTACK-INTERACTION-1 -->

If SA_ONSTACK is set AND `alt_stack` is configured AND `alt_stack` is not already active: frame goes on alt_stack. Nested handlers can recursively use alt_stack if the flag is set; the "alt_stack already active" check uses a bit in `AltSignalStack.flags` that's set on entry and cleared at sigreturn.

Typical use: SIGSEGV handler installed with SA_ONSTACK to handle stack-overflow SIGSEGVs (where the normal stack is exhausted).


---

## Part VI: Cross-cutting behaviors

<!-- txdoc:SIGNAL-PART-VI-CROSS-CUTTING-BEHAVIORS-1 -->

### 19. SA_RESTART and ERESTARTSYS

<!-- txdoc:SIGNAL-SA-RESTART-ERESTARTSYS-1 -->

POSIX: when a slow (blocking, interruptible) syscall is interrupted by a signal whose handler has SA_RESTART, the handler runs and the syscall restarts transparently. Without SA_RESTART, the syscall returns EINTR.

Implementation:

- A blocking syscall parks on `reactor::wait(channel, mask, WaitProtocol::Interruptible)`. On signal post that interrupts, yield-adapt returns `WaitOutcome::Interrupted`.
- The script receives `Interrupted` and returns `ERESTARTSYS` (internal errno; not POSIX-visible as such).
- thread_future's state-machine unwind observes ERESTARTSYS as it propagates up through the dispatch_trap future.
- At the trap-return path, before HAL does sret, the trap-return code checks: if the syscall returned ERESTARTSYS and a signal is being delivered with SA_RESTART: rewind the trap PC to the syscall instruction, then deliver the signal. Handler runs; when handler returns via sigreturn, pre-signal PC is the syscall entry, so the syscall re-executes.
- If ERESTARTSYS and the handler doesn't have SA_RESTART: return EINTR to userspace (transform the errno to EINTR).

```rust
// In the trap-return path, after ast_check decides to deliver a handler:
// (pseudocode at KernelTrapSink / SignalFrameIf boundary)

let syscall_result = script_result;  // from dispatch_trap
match (syscall_result, ast_outcome) {
    (Err(ERESTARTSYS), AstOutcome::DeliverHandler(frame)) => {
        let handler_flags = current_handler_sa_flags();
        if handler_flags.contains(SaFlags::RESTART) {
            // Rewind PC; handler will run; after sigreturn, syscall re-executes.
            P::rewind_syscall_pc(tf);
            // SignalFrameIf has installed the handler frame; trap return enters it.
            return TrapAction::Resume;
        } else {
            // No SA_RESTART: return EINTR to userspace.
            write_syscall_result(tf, Err(EINTR));
            // Then install the signal frame; handler runs before userspace code
            // observes EINTR at all.
            return TrapAction::Resume;
        }
    }
    (Err(ERESTARTSYS), AstOutcome::Continue) => {
        // No handler to deliver; convert to EINTR.
        // This happens e.g. if a Killable wait was interrupted by SIGKILL;
        // ast_check returned InitiateTermination before this branch, though,
        // so practically this case is rare.
        write_syscall_result(&mut thread.payload.regs, Err(EINTR));
    }
    (Ok(v), _) | (Err(_), _) => {
        // Normal return path.
    }
}
```

#### 19.1 ERESTARTSYS family (Linux-derived variants)

<!-- txdoc:SIGNAL-ERESTARTSYS-FAMILY-LINUX-DERIVED-VARIANTS-1 -->

Linux has four internal restart-errno values differing in subtle cases:

- `ERESTARTSYS`: restart iff SA_RESTART set in handler (our primary).
- `ERESTARTNOINTR`: always restart regardless of SA_RESTART. Used for some kernel-internal waits where EINTR would be wrong.
- `ERESTARTNOHAND`: restart only if no handler (else EINTR). Used for some syscalls that aren't cleanly restartable with a handler.
- `ERESTART_RESTARTBLOCK`: restart via `restart_syscall` system call with saved state. Used for `nanosleep` et al. where remaining-time must be preserved.

Phase 1 implements ERESTARTSYS; the others are deferred (Phase 2) and relevant only for specific syscalls that will be implemented later.

### 20. Synchronous-fault entry point

<!-- txdoc:SIGNAL-SYNCHRONOUS-FAULT-ENTRY-POINT-1 -->

`deliver_synchronous_fault` is the HAL fault-handler entry, called from trap context on the faulting thread. Unlike `deliver_posix_signal`:

- Runs synchronously with the fault; no pending-queue enqueue.
- Bypasses the mask in effect for action selection (see §15.1).
- Returns `FaultOutcome` to the HAL for immediate action.

```rust
pub fn deliver_synchronous_fault(
    thread: &Cap<ThreadIdentity>,
    signum: Signum,
    siginfo: SigInfo,
) -> FaultOutcome;
```

Called by the HAL from `handle_synchronous_fault`; returns either DeliverHandler (handler is installed; build frame now) or ForceTerminate (terminate the thread immediately).

The signal is considered "delivered" without ever entering a pending queue. If a handler runs and returns normally, sigreturn restores pre-fault regs; re-executing the faulting instruction typically re-traps. (Signal handlers for SIGSEGV usually longjmp out or adjust state via ucontext.)

### 21. Producer catalog

<!-- txdoc:SIGNAL-PRODUCER-CATALOG-1 -->

All kernel subsystems that generate POSIX signals:

| Producer | Signal | Invocation |
|---|---|---|
| HAL fault handler | SIGSEGV, SIGBUS, SIGFPE, SIGILL | `deliver_synchronous_fault(thread, sig, siginfo)` |
| sys_kill syscall | any, per arg | `deliver_posix_signal(SignalTarget::Process/Pgroup, sig, siginfo_kill())` |
| sys_tkill syscall | any, per arg | `deliver_posix_signal(SignalTarget::Thread(target), sig, siginfo_tkill())` |
| sys_tgkill syscall | any, per arg | as above, with tgid validated |
| sys_rt_sigqueueinfo syscall | RT signals | `deliver_posix_signal(SignalTarget::Process, sig, user_siginfo)` |
| raise (library call via sys_kill or sys_tgkill to self) | any | as above |
| step_process_exit (publish phase) | SIGCHLD | `deliver_posix_signal(SignalTarget::Process(parent), SIGCHLD, siginfo_chld())` |
| VFS step_write on broken pipe | SIGPIPE | `deliver_posix_signal(SignalTarget::Thread(writer), SIGPIPE, siginfo_pipe())` |
| TTY input-processing on VINTR | SIGINT | `deliver_posix_signal(SignalTarget::ProcessGroup(fg_pgrp), SIGINT, siginfo_kernel())` |
| TTY input-processing on VQUIT | SIGQUIT | as above |
| TTY input-processing on VSUSP | SIGTSTP | as above |
| TTY hangup | SIGHUP | `deliver_posix_signal(SignalTarget::ProcessGroup(session_leader_pgrp), SIGHUP, ...)` |
| TTY size change | SIGWINCH | `deliver_posix_signal(SignalTarget::ProcessGroup(fg_pgrp), SIGWINCH, ...)` |
| TTY background write/read attempt | SIGTTOU, SIGTTIN | as above |
| Socket urgent data arrival | SIGURG | consult OpenFile.fown, `deliver_posix_signal(fown.target, SIGURG, ...)` |
| fd readiness (F_SETOWN) | SIGIO or F_SETSIG signum | consult OpenFile.fown, deliver |
| Process-group orphan at session leader exit | SIGHUP, SIGCONT | PROCESS_v1 §8 cascade |
| Timer expiry (POSIX timers) | per sigevent | deferred (POSIX_TIMERS_v1) |
| Message queue notification | per sigevent | deferred (POSIX_MQUEUE_v1) |

Each producer owns its own signal-generation state (F_SETOWN's fown, TTY's fg_pgrp binding, Timer's sigevent config, etc.). The signal spec just specifies the entry point they invoke.

### 22. SIGCHLD specifics

<!-- txdoc:SIGNAL-SIGCHLD-SPECIFICS-1 -->

Generated on child state change: exit, stop, continue. Default action is Ign (POSIX); programs must install a handler to observe.

**Generation sites:**

- Child exit (step_process_exit): always.
- Child stop (SIGSTOP etc.): only if parent does NOT have SA_NOCLDSTOP set.
- Child continue (SIGCONT): only if parent does NOT have SA_NOCLDSTOP set.

**SA_NOCLDWAIT behavior:**

If parent has SA_NOCLDWAIT set for SIGCHLD OR the disposition is explicitly SIG_IGN: children should be auto-reaped. Concretely: on child exit, do not create a zombie; directly reclaim. This is PROCESS_v1's concern but consulted from the same sig_actions reads.

```rust
// At step_process_exit, after the child's payload has been dropped but
// before the zombie is created:
let parent = child.parent.load(&guard)?;
let table = parent.payload.frame.sig_actions.get(&guard);
let chld_entry = table.get(SIGCHLD);

let auto_reap = chld_entry.disposition == Disposition::Ignore
    || chld_entry.flags.contains(SaFlags::NOCLDWAIT);

if auto_reap {
    // Skip zombie creation; reclaim immediately.
    immediately_reap(child);
} else {
    // Normal path: zombie created; SIGCHLD sent; parent can waitpid.
    make_zombie(child);
    deliver_posix_signal(
        SignalTarget::Process(parent),
        SIGCHLD,
        siginfo_chld(child.pid, child.exit_status),
    );
}
```

This is specified more completely in PROCESS_v1's reap path; this spec provides the sig_actions-consultation logic.

### 23. SIGPIPE specifics

<!-- txdoc:SIGNAL-SIGPIPE-SPECIFICS-1 -->

Generated when a thread writes to a pipe/socket with no reader. Per-thread (targets the writing thread, not the process).

```rust
// In VFS's step_write on a pipe:
fn step_write_pipe(pipe: &Pipe, buf: &[u8]) -> StepOutcome<usize> {
    if !pipe.readers_exist() {
        // Broken pipe.
        let writer_thread = current_thread();
        deliver_posix_signal(
            SignalTarget::Thread(writer_thread),
            SIGPIPE,
            siginfo_pipe(),
        );
        return StepOutcome::Err(Errno::EPIPE);
    }
    // ... normal write path
}
```

Userspace convention: SIG_IGN or a handler that checks EPIPE returns. Almost all production code does `signal(SIGPIPE, SIG_IGN)` at startup.

SO_NOSIGPIPE (socket option) and MSG_NOSIGNAL (send flag) suppress SIGPIPE for specific sockets/sends. These are VFS/socket concerns; the check happens before `deliver_posix_signal` is called.

### 24. Stop/continue interaction with THREAD_RUNTIME §6

<!-- txdoc:SIGNAL-STOP-CONTINUE-INTERACTION-THREAD-RUNTIME-6-1 -->

Stop/continue use THREAD_RUNTIME_v1 §6's `stop_state` machinery. The signal spec describes the shim-visible routing.

**SIGSTOP:** bypasses sig_actions; directly invokes `invoke_group_stop`. Every thread in the target process transitions `stop_state` from Running to StopRequested. Threads observe this at site B (next syscall return or AST) and park on stop_channel.

**SIGTSTP / SIGTTIN / SIGTTOU:** these have default Stop but are catchable. Default action invokes `invoke_group_stop` (same as SIGSTOP). Handler installed: routes to pending for handler delivery.

**SIGCONT:** bypasses sig_actions for the continue-effect; `invoke_group_continue` transitions stop_state back to Running. Then, if a handler is installed, routes to pending for handler delivery (see §12.3).

```rust
fn invoke_group_stop(proc: Cap<ProcessIdentity>) {
    let payload = proc.payload.load(&guard).expect("alive");
    for entry in payload.threads.iter(&guard) {
        let thread = entry.element(&guard).to_cap();
        let tpayload = thread.payload.load(&guard).expect("alive");
        tpayload.stop_state.transition_to_stop_requested();
        // Wake interruptible waits; Killable waits ignore stop (per
        // THREAD_RUNTIME §6, only fatal signals interrupt Killable).
        let mut summary = tpayload.signal_summary.load();
        summary = summary.with_stop_requested();
        tpayload.signal_summary.store(summary);
        reactor::wake_task(thread.task_id);
    }
}

fn invoke_group_continue(proc: Cap<ProcessIdentity>) {
    let payload = proc.payload.load(&guard).expect("alive");
    for entry in payload.threads.iter(&guard) {
        let thread = entry.element(&guard).to_cap();
        let tpayload = thread.payload.load(&guard).expect("alive");
        tpayload.stop_state.transition_to_running();
        // Fire stop_channel so threads parked there resume.
        tpayload.stop_channel.fire();
    }
}
```

Per-thread stop/cont granularity (ptrace SINGLESTEP etc.) is handled by the observation subsystem, not this spec. Group-level SIGSTOP/SIGCONT are the POSIX surface.

### 25. Forbidden configurations

<!-- txdoc:SIGNAL-FORBIDDEN-CONFIGURATIONS-1 -->

The following are rejected at the syscall boundary:

- `sigaction(SIGKILL, ...)` with non-null `act` → EINVAL.
- `sigaction(SIGSTOP, ...)` with non-null `act` → EINVAL.
- `sigprocmask(BLOCK, {SIGKILL, SIGSTOP})` — silently strips those bits, doesn't fail. Per POSIX.
- Sending SIGKILL or SIGSTOP to init (pid 1) — Linux ignores silently; we follow.
- `sigaction` with invalid handler address — accepted; fault occurs at handler-invocation time.
- `sigaction` with reserved flag bits set — EINVAL (Phase 2 might accept and ignore for forward compat; Phase 1 rejects).
- Signals outside [1, NSIG) — EINVAL.

---

## Part VII: Syscall catalog

<!-- txdoc:SIGNAL-PART-VII-SYSCALL-CATALOG-1 -->

All syscalls follow the script-as-coroutine pattern per SUBSYSTEM_ANATOMY §3. Signal syscalls are generally fast (atomic CAS + maybe a pending-queue op); few involve multi-phase reservations.

### 26. sigaction

<!-- txdoc:SIGNAL-SIGACTION-1 -->

```rust
pub fn sys_sigaction(
    sig: Signum,
    act: Option<UserPtr<SigActionEntry>>,
    oldact: Option<UserPtr<SigActionEntry>>,
) -> Result<(), Errno>;
```

Per §5. Atomic per-entry swap; no reservations.

### 27. sigprocmask / pthread_sigmask

<!-- txdoc:SIGNAL-SIGPROCMASK-PTHREAD-SIGMASK-1 -->

```rust
pub fn sys_sigprocmask(
    how: SigHow,
    set: Option<UserPtr<SignalMask>>,
    oldset: Option<UserPtr<SignalMask>>,
) -> Result<(), Errno>;
```

Per §7. Atomic mask update; calls `refresh_signal_summary` after unblock.

### 28. sigpending

<!-- txdoc:SIGNAL-SIGPENDING-1 -->

```rust
pub fn sys_sigpending(set: UserPtr<SignalMask>) -> Result<(), Errno>;
```

Returns the union of `thread_pending.pending_mask()` and `group_pending.pending_mask()` for the current thread's process. No dequeue; purely query.

### 29. sigsuspend

<!-- txdoc:SIGNAL-SIGSUSPEND-1 -->

```rust
pub fn sys_sigsuspend(mask: UserPtr<SignalMask>) -> Result<!, Errno>;
```

Atomically replace the thread's mask with the argument mask, block, and wait for an unmasked signal. Returns only when a handler runs (which is after the handler returns, via sigreturn restoring the old mask).

Implementation:

```rust
async fn script_sigsuspend(new_mask: &SignalMask) -> Result<!, Errno> {
    let thread = current_thread();
    let payload = thread.payload.load(&guard).expect("alive");

    // Save old mask; install new.
    let old_mask = payload.signal_mask.swap(*new_mask);

    // Arrange to restore old_mask at next sigreturn via a flag or via
    // writing to the forthcoming signal frame. Standard approach: set
    // a per-thread "suspend_saved_mask" that's consulted by the next
    // signal frame build.
    payload.saved_mask_for_suspend.store(Some(old_mask));

    // Block on signal_summary.has_deliverable going true.
    loop {
        let summary = payload.signal_summary.load();
        if summary.has_deliverable() || summary.termination() {
            break;
        }
        reactor::wait(
            thread.signal_summary_channel(),
            SummaryMask::DELIVERABLE | SummaryMask::TERMINATION,
            WaitProtocol::Interruptible,
        ).await;
    }

    // Returning here hands control back to thread_future; next AST will
    // deliver the handler, and sigreturn will restore old_mask.
    // The syscall itself doesn't return to userspace normally: the handler
    // runs, sigreturn restores pre-suspend context, and the post-suspend
    // return value is EINTR.
    return Err(Errno::EINTR);
}
```

Per POSIX, `sigsuspend` always returns -1 with errno=EINTR (it's always "interrupted by a signal").

### 30. sigaltstack

<!-- txdoc:SIGNAL-SIGALTSTACK-1 -->

```rust
pub fn sys_sigaltstack(
    ss: Option<UserPtr<AltSignalStack>>,
    old_ss: Option<UserPtr<AltSignalStack>>,
) -> Result<(), Errno>;
```

Atomic read-old/write-new on `ThreadPayload.alt_stack`. Validation: `size >= MINSIGSTKSZ` (typically 2048 bytes); base is valid userspace. Cannot change alt_stack while it is currently in use (SS_ONSTACK flag is set).

### 31. sigreturn

<!-- txdoc:SIGNAL-SIGRETURN-2 -->

```rust
pub fn sys_sigreturn() -> !;
```

Per §17. Not meant to be called by ordinary code; invoked by the sigreturn trampoline at handler return.

### 32. kill

<!-- txdoc:SIGNAL-KILL-1 -->

```rust
pub fn sys_kill(pid: i32, sig: Signum) -> Result<(), Errno>;
```

Target resolution per POSIX:
- `pid > 0`: signal the process with that pid.
- `pid == 0`: signal every process in the caller's process group.
- `pid == -1`: signal every process the caller has permission to signal (skip init).
- `pid < -1`: signal every process in process group `-pid`.

Permission: per cred v1.2 — caller's euid must match target's euid (or caller has CAP_KILL), except for SIGCONT within the same session.

Flow:
```rust
async fn script_kill(pid: i32, sig: Signum) -> Result<(), Errno> {
    if sig == Signum(0) {
        // Signal 0: permission check only, no actual send.
        let target = resolve_pid_target(pid)?;
        check_signal_permission(&current_cred(), &target, sig)?;
        return Ok(());
    }

    let target = resolve_pid_target(pid)?;
    for proc_or_group in target.expand() {
        check_signal_permission(&current_cred(), &proc_or_group, sig)?;
        let siginfo = siginfo_kill(current_pid(), current_uid());
        deliver_posix_signal(proc_or_group, sig, siginfo);
    }
    Ok(())
}
```

### 33. tkill / tgkill

<!-- txdoc:SIGNAL-TKILL-TGKILL-1 -->

```rust
pub fn sys_tkill(tid: Tid, sig: Signum) -> Result<(), Errno>;
pub fn sys_tgkill(tgid: Pid, tid: Tid, sig: Signum) -> Result<(), Errno>;
```

`tkill` signals a specific thread by tid; `tgkill` additionally validates that the tid belongs to the specified tgid (prevents race where tid is recycled to another process between lookup and signal).

`tkill` is deprecated in favor of `tgkill` but supported for legacy.

Permission: same as `kill`, targeting the thread's process.

### 34. sigqueue / rt_sigqueueinfo

<!-- txdoc:SIGNAL-SIGQUEUE-RT-SIGQUEUEINFO-1 -->

```rust
pub fn sys_rt_sigqueueinfo(
    pid: Pid,
    sig: Signum,
    info: UserPtr<SigInfo>,
) -> Result<(), Errno>;
```

User-specified siginfo; mainly useful for RT signals where siginfo is preserved per-instance. Userspace's `sigqueue(3)` wraps this.

Validation: user-provided `info.si_code` must be non-negative (reserved values). `info.si_signo` must match `sig`. Corresponding tgsigqueueinfo variant takes tgid+tid.

### 35. sigtimedwait / sigwaitinfo

<!-- txdoc:SIGNAL-SIGTIMEDWAIT-SIGWAITINFO-1 -->

```rust
pub fn sys_rt_sigtimedwait(
    set: UserPtr<SignalMask>,
    info: Option<UserPtr<SigInfo>>,
    timeout: Option<UserPtr<Timespec>>,
) -> Result<Signum, Errno>;
```

Synchronous signal-drain. Waits until a signal in `set` is pending for the calling thread; dequeues one and returns its signum, optionally writing its siginfo.

Per POSIX: dequeued signals do not invoke their handler. The thread consumes the signal by waiting on it; no frame is built.

Implementation: variant of sigsuspend — atomically install a mask that unblocks `set`, wait for deliverable, dequeue without dispatching.

```rust
async fn script_sigtimedwait(set: &SignalMask, timeout: Option<Duration>)
    -> Result<(Signum, SigInfo), Errno>
{
    let thread = current_thread();
    let payload = thread.payload.load(&guard).expect("alive");

    // Check if any signal in set is already pending.
    if let Some((sig, info)) = dequeue_first_matching(&payload, set) {
        return Ok((sig, info));
    }

    // Atomic wait: unblock set during the wait, restore on return.
    let saved_mask = payload.signal_mask.swap(payload.signal_mask.load().intersect(set.complement()));

    let outcome = if let Some(t) = timeout {
        reactor::wait_timeout(
            thread.signal_summary_channel(),
            SummaryMask::DELIVERABLE,
            WaitProtocol::InterruptibleTimeout(t),
        ).await
    } else {
        reactor::wait(
            thread.signal_summary_channel(),
            SummaryMask::DELIVERABLE,
            WaitProtocol::Interruptible,
        ).await
    };

    // Restore mask.
    payload.signal_mask.store(saved_mask);

    match outcome {
        WaitOutcome::Ready => {
            dequeue_first_matching(&payload, set)
                .ok_or(Errno::EAGAIN)  // race; spurious
                .map(|(s, i)| (s, i))
        }
        WaitOutcome::TimedOut => Err(Errno::EAGAIN),
        WaitOutcome::Interrupted => Err(Errno::EINTR),  // non-set signal intervened
        WaitOutcome::Killed => unreachable!("fatal signals handled elsewhere"),
    }
}
```

`sigwaitinfo` is `sigtimedwait` with no timeout. POSIX thread library's `sigwait(3)` wraps these.

---

## Part VIII: Native consumers (signalfd, pidfd)

<!-- txdoc:SIGNAL-PART-VIII-NATIVE-CONSUMERS-SIGNALFD-PIDFD-1 -->

### 36. signalfd

<!-- txdoc:SIGNAL-SIGNALFD-1 -->

Exposes a thread-or-process's pending signals as a readable fd. Implemented as a thin VFS RNode with subscription to the target's `signal_port`.

```rust
// Zone-allocated per VFS RNode convention. One signalfd zone.
pub struct SignalFd {
    // RNode structural header (type-tagged: RNodeType::SignalFd).
    structural: RNodeStructural,

    // Payload side.
    payload: PayloadBinding<SignalFdPayload>,
}

pub struct SignalFdPayload {
    /// Filter mask. signals outside this mask are not readable via this fd.
    /// Atomic to allow sys_signalfd modification without reallocation.
    pub mask: AtomicSignalMask,

    /// Target process (the process whose pending queue we drain).
    /// Phase 1: always the process that created the signalfd.
    pub target: Weak<ProcessIdentity>,

    /// BUS_v1 subscription handle on target's signal_port.
    /// On signal_port fire, signalfd's waker runs; re-evaluates readability.
    pub subscription: Subscription,

    /// Readability wire (level-triggered). Consumed by epoll, poll, blocking read.
    pub readable: RawQueue<SignalFdReadiness>,
}

pub struct SignalFdReadiness(u32);
impl SignalFdReadiness {
    pub const HAS_SIGNAL: Self = SignalFdReadiness(1);
}
```

#### 36.1 Creation

<!-- txdoc:SIGNAL-CREATION-1 -->

```rust
async fn script_signalfd(fd_arg: i32, mask: &SignalMask, flags: SignalFdFlags)
    -> Result<Fd, Errno>
{
    if fd_arg == -1 {
        // Create new signalfd.
        let rnode = zone::signalfd::allocate()?;
        let process = current_process();

        // Subscribe to the process's signal_port with full interest;
        // mask filtering is done in the waker's re-evaluation.
        let subscription = process.identity.signal_port.subscribe(
            SignalPortInterest::All,
            signalfd_waker_for(&rnode),
        );

        let payload = SignalFdPayload {
            mask: AtomicSignalMask::new(*mask),
            target: Weak::from(process.identity.clone()),
            subscription,
            readable: RawQueue::new(),
        };

        rnode.payload.publish(payload);

        // Wrap in OpenFile, install in fd_table.
        let openfile = OpenFile::new(rnode.to_cap(), flags_to_openflags(flags));
        let fd = current_process().payload.frame.fd_table.install(openfile)?;
        Ok(fd)
    } else {
        // Modify existing signalfd's mask.
        let openfile = current_process().payload.frame.fd_table.get(fd_arg)?;
        let rnode = openfile.rnode();
        if rnode.type_tag() != RNodeType::SignalFd {
            return Err(Errno::EINVAL);
        }
        let payload = rnode.payload.get(&guard).expect("alive");
        payload.mask.store(*mask);

        // Re-evaluate readability with new mask.
        signalfd_reevaluate_readable(&payload);

        Ok(fd_arg)
    }
}
```

#### 36.2 Waker logic

<!-- txdoc:SIGNAL-WAKER-LOGIC-1 -->

The subscription's waker runs on every `signal_port.fire`. Its job is to re-evaluate whether this signalfd is now readable.

```rust
fn signalfd_waker(sfd_rnode: &Cap<SignalFd>) {
    let payload = sfd_rnode.payload.load(&guard).unwrap_or_else(|| {
        // Payload reclaimed — signalfd closing. Nothing to do.
        return;
    });

    signalfd_reevaluate_readable(&payload);
}

fn signalfd_reevaluate_readable(payload: &SignalFdPayload) {
    let target = payload.target.upgrade(&guard);
    let Some(target) = target else {
        // Target died; signalfd is permanently not-readable (or returns
        // SIGCHLD-like "process exited" info; Phase 1: plain not-readable).
        return;
    };

    let target_payload = target.payload.load(&guard);
    let Some(target_payload) = target_payload else {
        return;
    };

    let mask = payload.mask.load();
    let group_pending = target_payload.group_pending.pending_mask();
    let has_signal = !group_pending.intersect(mask).is_empty();

    if has_signal {
        payload.readable.fire(SignalFdReadiness::HAS_SIGNAL);
    } else {
        payload.readable.clear(SignalFdReadiness::HAS_SIGNAL);
    }
}
```

Re-evaluation is level-triggered: the readable wire reflects current state, not transitions. Subscribers (epoll) observe level truth.

#### 36.3 Read

<!-- txdoc:SIGNAL-READ-1 -->

```rust
impl RNodeOps for SignalFd {
    fn read(&self, open: &OpenFile, buf: &mut [u8]) -> StepOutcome<usize, Errno> {
        let payload = self.payload.load(&guard).ok_or(Errno::EBADF)?;
        let target = payload.target.upgrade(&guard).ok_or(Errno::ESRCH)?;
        let target_payload = target.payload.load(&guard).ok_or(Errno::ESRCH)?;
        let mask = payload.mask.load();

        // Try to dequeue a mask-matching signal from target's group_pending.
        if let Some((sig, info)) = dequeue_matching(&target_payload.group_pending, mask) {
            // Write siginfo_t format to buf (signalfd_siginfo layout, POSIX).
            let written = write_signalfd_siginfo(buf, sig, &info)?;

            // Update readable state in case that was the last matching signal.
            signalfd_reevaluate_readable(&payload);

            return StepOutcome::Done(written);
        }

        // No signal. If nonblocking, return EAGAIN.
        if open.flags.contains(OpenFlags::NONBLOCK) {
            return StepOutcome::Err(Errno::EAGAIN);
        }

        // Block on readable wire.
        StepOutcome::Yield {
            shape: YieldShape::OnWaitSource {
                source: payload.readable.channel(),
                interests: SignalFdReadiness::HAS_SIGNAL,
            },
            progress: Progress::ZERO,
        }
    }

    fn poll(&self, _open: &OpenFile) -> PollMask {
        let payload = self.payload.load(&guard).map_or(PollMask::empty(), |p| {
            if p.readable.current() & SignalFdReadiness::HAS_SIGNAL != 0 {
                PollMask::IN
            } else {
                PollMask::empty()
            }
        });
        payload
    }

    fn close(&self, _open: &OpenFile) -> StepOutcome<(), Errno> {
        let payload = self.payload.take(&guard);
        if let Some(p) = payload {
            p.subscription.unsubscribe();
            // readable wire finalized automatically at RNode reclamation.
        }
        StepOutcome::Done(())
    }
}
```

Read drains from target's group_pending directly. Dequeue is atomic; races with AST handler-delivery are resolved at the queue level (whichever dequeues first wins).

#### 36.4 Race with handler delivery

<!-- txdoc:SIGNAL-RACE-HANDLER-DELIVERY-1 -->

Classic Linux caveat: signalfd and handler delivery race on the same pending queue. If a process has both a handler installed for SIGCHLD AND a signalfd for SIGCHLD AND does not block SIGCHLD:

- A SIGCHLD post enters group_pending.
- Some thread's AST pass dequeues it for handler delivery.
- Meanwhile, the signalfd read thread calls read; dequeue finds the queue empty.
- Handler runs; read returns no data (or blocks).

Or vice versa: signalfd read dequeues before AST; handler doesn't fire.

Linux-compatible by design. Programs using signalfd must `sigprocmask(BLOCK, {SIGCHLD})` to prevent handler delivery.

### 37. pidfd

<!-- txdoc:SIGNAL-PIDFD-1 -->

Exposes process lifecycle (exit, signals to the target process) as a readable fd.

```rust
pub struct PidFd {
    structural: RNodeStructural,
    payload: PayloadBinding<PidFdPayload>,
}

pub struct PidFdPayload {
    pub target: Weak<ProcessIdentity>,
    pub exit_subscription: Subscription,   // on target.exit_source
    pub readable: RawQueue<PidFdReadiness>,
}
```

Simpler than signalfd — no mask, one kind of event (target exit). Readable fires when target has exited; read returns exit status.

Phase 1 implements basic pidfd_open and pidfd_send_signal; pidfd_getfd and richer pidfd features are Phase 2.

### 38. Other fd-adapter specs

<!-- txdoc:SIGNAL-OTHER-FD-ADAPTER-SPECS-1 -->

- **timerfd**: subscribes to a reactor timer's expiry port; read returns expiration count. Specified in POSIX_TIMERS_v1 (future).
- **eventfd**: has its own internal counter (not subscribe-based); read drains counter. Specified in a separate small spec.
- **inotify / fanotify**: subscribe to VFS fsnotify_port wires; specified in future observation/filesystem-notify specs.

---

## Part IX: Substrate catalog

<!-- txdoc:SIGNAL-PART-IX-SUBSTRATE-CATALOG-1 -->

This section enumerates every substrate primitive the signal spec uses and where it comes from. It is a checklist confirming no new substrate is introduced.

### 39. Primitives from tx-fnd (foundation)

<!-- txdoc:SIGNAL-PRIMITIVES-TX-FND-FOUNDATION-1 -->

| Primitive | Used for | Defined in |
|---|---|---|
| `AtomicU64`, `AtomicU32`, `AtomicU8` | signal_mask, signal_summary, sig_actions entries, pending bitsets | tx-fnd/atomic |
| `AtomicCell<T>` | Slot for standard-signal siginfo storage | tx-fnd/atomic |
| Zone allocators | SignalFd, PidFd entities | tx-fnd/zone |
| `BoundedRing<T, N>` | RT per-signum FIFO queues | tx-fnd/bounded-ring (or inline internal) |
| `Cap<T>`, `PayloadCap<T>`, `Weak<T>` | Entity retention throughout | object_model |
| Epoch guard | All subscriber walks, DLL iterations | tx-fnd/epoch |

### 40. Primitives from object model / zone interface

<!-- txdoc:SIGNAL-PRIMITIVES-OBJECT-MODEL-ZONE-INTERFACE-1 -->

| Primitive | Used for | Defined in |
|---|---|---|
| `Binding<T>` | Not directly; used transitively via PayloadBinding | object_model_v2 §7 |
| `PayloadBinding<T>` | SignalFd.payload, PidFd.payload | object_model_v2 §7 / EBR_ZONE_INTERFACE_v1 |
| `Weak<T>` | SignalFd.target, PidFd.target (tolerate target death) | EBR_ZONE_INTERFACE_v1 |

### 41. Primitives from BUS_v1

<!-- txdoc:SIGNAL-PRIMITIVES-BUS-V1-1 -->

| Primitive | Used for | Defined in |
|---|---|---|
| `RawPort<T>` | ProcessIdentity.signal_port (fired here); target.signal_port subscription | BUS_v1 §1 |
| `RawQueue<M>` | SignalFd.readable, PidFd.readable; `signalfd_readable` wire | BUS_v1 §1 |
| `Subscription` | SignalFd.subscription, PidFd.exit_subscription | BUS_v1 §4 |
| `Subscribable` trait | Implemented by ProcessIdentity for signal_port and exit_source | BUS_v1 §4 |

### 42. Primitives from REACTOR_v0

<!-- txdoc:SIGNAL-PRIMITIVES-REACTOR-V0-1 -->

| Primitive | Used for | Defined in |
|---|---|---|
| `WaitProtocol` | Signal-interruption in sigsuspend, sigtimedwait, sigwait | REACTOR_v0 §Wait |
| `WaitOutcome` | Distinguish Ready/Interrupted/Killed/TimedOut at wait return | REACTOR_v0 §Wait |
| `Channel`, `Waker` | Signal-summary channel; waker for task on deliverable update | REACTOR_v0 |
| `wake_task(TaskId)` | Mark-deliverable path; wake interruptibly-waiting thread | REACTOR_v0 |
| `request_userspace_run` | thread_future's userspace-run wait; interacts with AST | REACTOR_v0 §Preemption |

### 43. Primitives from SIGNAL_ATTACHMENTS_v1

<!-- txdoc:SIGNAL-PRIMITIVES-SIGNAL-ATTACHMENTS-V1-1 -->

| Attachment | Used for | Cataloged in |
|---|---|---|
| `ProcessIdentity.signal_port` | Fires on every signal produced | SIGNAL_ATTACHMENTS_v1 §3.3 |
| `ProcessIdentity.exit_source` | Subscribed by pidfd | SIGNAL_ATTACHMENTS_v1 §3.3 |
| `SignalFd.signalfd_readable` | Fires on mask-matching pending | SIGNAL_ATTACHMENTS_v1 §3.4 |
| `PidFd.pidfd_readable` | Fires on target exit | SIGNAL_ATTACHMENTS_v1 §3.4 |
| `Socket.urgent_port` | SIGURG generation path | SIGNAL_ATTACHMENTS_v1 §3.6 |
| `ThreadIdentity.thread_exit_source` | pthread_join; clear_child_tid | SIGNAL_ATTACHMENTS_v1 §3.3 |

### 44. Primitives from PROCESS_v1 / THREAD_RUNTIME_v1 / SCHEDULER_v0

<!-- txdoc:SIGNAL-PRIMITIVES-PROCESS-V1-THREAD-RUNTIME-V1-SCHEDULER-V0-1 -->

| Mechanism | Used for | Defined in |
|---|---|---|
| `GroupExit` / `invoke_group_exit` | SIGKILL, SIG_DFL with Term default | PROCESS_v1 §5 |
| `invoke_group_stop` / `invoke_group_continue` | SIGSTOP, SIGCONT routing | THREAD_RUNTIME_v1 §6 |
| `stop_state`, `stop_channel` | Thread-level stop/continue mechanics | THREAD_RUNTIME_v1 §6 |
| `select_target_thread` | Process-directed signal picks a thread | THREAD_RUNTIME_v1 §5.6 |
| `refresh_signal_summary` | Recompute deliverable after mask change | THREAD_RUNTIME_v1 §5.2 |
| `Shared<T>` | sig_actions table sharing; CLONE_SIGHAND | PROCESS_v1 §3 |

### 45. Internal to signal spec (defined here)

<!-- txdoc:SIGNAL-INTERNAL-SIGNAL-SPEC-DEFINED-HERE-1 -->

| Type | Purpose |
|---|---|
| `Signum`, NSIG, SIGRTMIN, SIGRTMAX | Signal taxonomy |
| `SignalMask`, `AtomicSignalMask` | Mask type |
| `InterruptSummary`, `AtomicInterruptSummary` | Packed summary for hot-path checks |
| `SigInfo` | POSIX siginfo_t (kernel-side representation) |
| `SigActionEntry`, `Disposition`, `SaFlags`, `SigActionTable` | Routing table |
| `PendingSignalQueue` with `post` / `dequeue` / `next_deliverable` | Shim pending state |
| `SignalTarget`, `DeliveryOutcome` | Producer entry point types |
| `SignalFrameIf`, `AstOutcome`, `FaultOutcome`, `SignalFrameInfo`, `FaultInfo` | Trap-return / HAL signal-frame contract |
| Syscall implementations | sigaction, sigprocmask, kill, tkill, tgkill, sigqueue, sigtimedwait, sigsuspend, sigaltstack, sigreturn |
| `SignalFd`, `PidFd` entity shapes | Native-consumer fd entities |

### 46. Not used

<!-- txdoc:SIGNAL-NOT-USED-1 -->

Explicitly confirmed as outside the Phase 1 signal spec's substrate usage:

- `MUTATION_COMPOSITIONS_v1` primitives (`structural_move`, etc.) — signalfds don't participate in DLL topology changes; they use BUS_v1 subscriptions instead.
- `BITMAP_RESERVATION_v1` — no bitmap allocation in signals.
- `AtomicOneShot` (GroupExit's carrier) — used transitively via invoke_group_exit, not directly.
- VDSO / vvar pages — Phase 2 for sigreturn.
- Per-user resource zones for RT signals — Phase 2 for RLIMIT_SIGPENDING.

---

## Part X: POSIX alignment

<!-- txdoc:SIGNAL-PART-X-POSIX-ALIGNMENT-1 -->

### 47. Committed Phase 1 surface

<!-- txdoc:SIGNAL-COMMITTED-PHASE-1-SURFACE-1 -->

- Standard signal catalog (1..31): all cataloged with POSIX default actions.
- Realtime signals (32..63): full FIFO-per-signum ordering, siginfo preservation, sigqueue.
- sigaction with flags: SA_RESTART, SA_SIGINFO, SA_NOCLDSTOP, SA_NOCLDWAIT, SA_NODEFER, SA_ONSTACK, SA_RESETHAND.
- sigprocmask, pthread_sigmask.
- sigpending.
- sigsuspend.
- sigaltstack.
- kill, tkill, tgkill.
- raise (userspace library over tkill).
- abort (userspace library over raise(SIGABRT)).
- sigqueue / rt_sigqueueinfo.
- sigtimedwait / sigwaitinfo (via rt_sigtimedwait).
- sigreturn with stack-based trampoline.
- Synchronous-fault delivery (SIGSEGV, SIGBUS, SIGFPE, SIGILL).
- Multistage handler flow including nested delivery.
- ERESTARTSYS with SA_RESTART.
- signalfd as thin bus adapter.
- pidfd as thin bus adapter (pidfd_open, pidfd_send_signal).
- SIGCHLD on child state change with SA_NOCLDSTOP and SA_NOCLDWAIT handling.
- SIGPIPE on broken pipe write.
- SIGSTOP / SIGCONT via control primitives.

### 48. Deferred to later phases

<!-- txdoc:SIGNAL-DEFERRED-LATER-PHASES-1 -->

- POSIX timers (timer_create, timer_settime, etc.) → POSIX_TIMERS_v1.
- POSIX message queues (mq_notify) → POSIX_MQUEUE_v1.
- SIGIO / SIGURG end-to-end F_SETOWN pipeline — OpenFile.fown storage noted; full async-I/O integration deferred.
- ERESTARTNOHAND, ERESTARTNOINTR, ERESTART_RESTARTBLOCK variants — Phase 2.
- VDSO-based sigreturn trampoline — Phase 2 optimization.
- RLIMIT_SIGPENDING per-user enforcement — Phase 2.
- Ptrace-signal interaction (PTRACE_SIGNALS, tracer intercepting delivery) — observation subsystem.
- Cross-pidnamespace signal routing — Phase 2 multi-namespace.
- Ancient APIs: sigvec, sigmask, siggetmask — not implemented (use modern equivalents).

---

## Part XI: Open questions

<!-- txdoc:SIGNAL-PART-XI-OPEN-QUESTIONS-1 -->

- **SigInfo ABI stability.** POSIX's `siginfo_t` is ABI-critical. The in-kernel representation must be faithfully translatable to per-arch siginfo_t for handler delivery. Exact layout pinned by HAL spec; this spec assumes 128-byte userspace siginfo_t with padding.

- **Signal_port payload size.** Firing `SignalGenerated { sig, siginfo }` carries ~52 bytes per event. Bus-internal event storage must accommodate this. If it doesn't, either (a) carry a Cap<SigInfo> with the payload allocated in a siginfo zone, or (b) shrink signal_port's payload to just signum with consumers doing a second read from pending. Phase 1 chooses (a) as more substrate-natural if event-size limits force it; otherwise inline.

- **AST invocation count.** ast_check runs on every kernel→user transition, including syscalls that never take a signal path. Performance-critical path; must be O(1) in the common case (no pending, no termination, no stop). The `signal_summary` atomic is the one read; zero-case returns Continue immediately.

- **CLONE_SIGHAND + fork interaction with exec.** If process A does CLONE_SIGHAND with B, then B does execve, execve calls exec_reset on the shared table. A sees the reset too. This matches Linux; documented here for clarity.

- **Multiple signalfds on same process.** Legal; each is a subscriber on signal_port. `signal_port.subscribe` allows multiple; each waker runs on each fire. O(N) work per signal post for N signalfds on the process.

- **SIG_IGN vs SIG_DFL semantics for SIGCHLD in the race with reap.** If parent sets SIGCHLD to SIG_IGN between child-exit-posting and parent's wait, the zombie status depends on whether the auto-reap decision has been made. Race is benign per Linux; the child is either reaped or reappears as a zombie, both POSIX-acceptable.

- **Signal delivery during execve.** execve's GroupExit collapses all threads but the caller. Signals pending at execve-entry that targeted other threads: those threads die before they can handle. Signals pending on the surviving thread: preserved across execve's address-space swap (signal state is in Frame/ThreadPayload, which persists). Handlers are reset per exec_reset but pending signals remain — the post-exec code will get SIG_DFL treatment for them.

- **Signal trap contract shape.** Pinned as named signal functions reached from `KernelTrapSink<P>`, plus `SignalFrameIf` for platform frame layout. There is no HAL signal-hooks slot and no runtime hook vtable.

---

## Short version

<!-- txdoc:SIGNAL-SHORT-VERSION-1 -->

> The signal spec is the POSIX compatibility shim over txKernel's factored Gewalt/event native architecture. Gewalt (SIGKILL, SIGSTOP, synchronous faults with SIG_DFL) invokes control primitives in PROCESS_v1 / THREAD_RUNTIME_v1 / SCHEDULER_v0 directly, bypassing routing. Events (SIGCHLD, SIGALRM, SIGPIPE, etc.) fire native bus wires (`signal_port`, `exit_source`, `signalfd_readable`) per SIGNAL_ATTACHMENTS_v1, which native consumers (signalfd, pidfd, timerfd) subscribe to. The shim consults `sig_actions` as a routing table: handler-installed signals route to per-process pending queues (standard bitset + RT FIFO); SIG_IGN drops; SIG_DFL applies POSIX default action. Delivery happens at two sites: yield-adapt interruption (returns Interrupted to script, translated to ERESTARTSYS or EINTR) and AST at kernel→user transitions (`KernelTrapSink<P>` reaches signal code, and `SignalFrameIf` installs signal frames). Multistage handler flow stacks frames naturally; sigreturn restores. SA_RESTART rewinds the trap PC when a handler with SA_RESTART runs. Synchronous faults take a distinct `deliver_synchronous_fault` entry with mask-bypass and force-Term for uncaught. Substrate: no new primitives; all state uses existing atomic / BUS_v1 / EBR_ZONE_INTERFACE_v1 / REACTOR_v0 / SIGNAL_ATTACHMENTS_v1 / PROCESS_v1 mechanisms. signalfd and pidfd are thin VFS RNodes with BUS_v1 subscriptions on target publications; no private siginfo rings.
