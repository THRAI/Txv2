# Decision D9: Signal subsystem wake-substrate migration

**Date:** 2026-05-11
**Status:** decided (research-only ADR; implementation deferred)
**Worker:** W-X (research-only)
**Companion:** [D2 (WaitSource coexistence)](2026-05-11-d2-waitsource-coexists-with-rawport.md),
[D4 (bus/mailbox layering)](2026-05-11-d4-bus-mailbox-layering.md),
[PR-3 shape ADR](2026-05-11-pr-3-wake-substrate-shape.md),
[2026-05-05 signal-day1](2026-05-05-signal-day1.md),
[2026-05-05 signal-delivery-sweep-day1](2026-05-05-signal-delivery-sweep-day1.md),
[2026-05-05 signal-gewalt-event-factoring](2026-05-05-signal-gewalt-event-factoring.md)

## 1. Problem statement

The PR-3D wake-substrate migration plan in D4 §6 names the five
mechanical bus consumers — `pipe`, `futex`, `exit_source`, `tty`, `vfs`
— each of which fits the one-`WaitSource`-per-object template
(register subscribers, fire mask, fan out events). Signal is the
sixth and last consumer mentioned in the W-S handoff, and **it does
not fit that template**. Signal routing is shaped by POSIX delivery
semantics, not by a single semantic object whose state transitions
wake every subscriber:

- `kill(pid, sig)` targets a **process**, but the delivery rule is
  "post to **one** eligible thread whose `sigmask` permits `sig`."
  This is a *target selection* problem, not a fan-out problem. A
  pipe with three blocked readers wakes all three when bytes
  arrive; `kill(pid, SIGTERM)` to a 50-thread process should wake
  exactly one of them.
- `tgkill(pid, tid, sig)` targets a **specific thread** by tid. No
  selection, direct post.
- `kill(-pgid, sig)` targets a **process group** (every member
  process). The fan-out is over processes, not over a single
  process's threads.
- **Process-directed** vs **thread-directed** distinction: a
  `SIGSEGV` synthesised by the trap shell from a fault on thread T
  is thread-directed (T is the only candidate); `kill(pid, SIGTERM)`
  is process-directed (any eligible thread serves).
- **Gewalt signals** (SIGKILL, SIGSTOP, SIGCONT — per
  `SIGNAL_v1` §1) bypass the pending queues entirely. They are
  control ops, not catchable signals. SIGKILL invokes
  `step_exit_group_with_signal`; SIGSTOP/SIGCONT update the
  `stop_requested` summary bit on **every** live thread. They are
  the only true broadcast cases.
- **Realtime signals** (32..=64) queue per-occurrence; standard
  signals coalesce in a 64-bit bitset. Day-1's pending queue does
  not yet implement RT queuing; this ADR does not lift that
  restriction, only ensures the eventual wake plumbing accommodates
  it.

The composite consequence: signal delivery needs *per-thread* wake
endpoints, plus a *routing decision* that runs under the lock that
serialises pending-mask updates against disposition reads — two
threads must not both wake on the same signum when one of them is
sufficient.

## 2. Survey: current signal subsystem

Files read:

```
crates/tx-subsystems/src/signal.rs                          ~1080 LoC
crates/tx-subsystems/src/signal/tests/delivery.rs           ~480 LoC
crates/tx-subsystems/src/signal/tests/kill_permission.rs
crates/tx-subsystems/src/signal/tests/step_reset_for_exec_tests.rs
crates/tx-subsystems/src/signal/tests/tty_bridge.rs
crates/tx-subsystems/src/thread_runtime/execution.rs        post_signal, step_sigprocmask
crates/tx-subsystems/src/thread_runtime/structure.rs        ThreadPayload fields
```

### 2.1 Per-thread state

`ThreadPayload` in `thread_runtime::structure.rs`:

```rust
pub struct ThreadPayload {
    pub(crate) task:                  SpinMutex<Option<TaskKey>>,
    pub(crate) signal_mask:           AtomicU64,           // blocked-signal mask
    pub(crate) thread_pending:        PendingSignalQueue,  // 64-bit bitset
    pub(crate) signal_summary:        AtomicU8,            // InterruptSummary (deliverable_signal | termination | stop_requested)
    pub userspace_slot:               UserspaceRunSlot,
    pub(crate) active_request:        SpinMutex<Option<UserspaceRunRequest>>,
    ...
}
```

`InterruptSummary` is a *denormalised view* maintained by
`post_signal`, `step_sigprocmask`, `step_thread_exit`, and the
SIGKILL/SIGSTOP/SIGCONT routing path. The bits are observed at the
`Channel::wait_event` poll boundary
(`tx-reactor/src/wait.rs:75–82`) via `InterruptSource::interrupt_summary`
and a `WaitProtocol::is_interruptible() && summary.deliverable_signal`
test that classifies the wait outcome as `Interrupted`.

### 2.2 Per-process state

`ProcessPayload` (referenced at `signal.rs:7`, `signal.rs:556`,
`signal.rs:608`, etc.):

```rust
// (paraphrased from grep evidence)
ProcessPayload {
    threads:        SpinMutex<Vec<Cap<ThreadIdentity>>>,
    sig_actions:    SigActionTable,   // 64-slot disposition table behind SpinMutex
    group_pending:  PendingSignalQueue,
    cred:           ...,
}
```

`SigActionTable` is a `SpinMutex<[SigDisposition; 64]>`. The
disposition lookup (`payload.sig_actions().get(sig)`) and a
disposition write (`payload.sig_actions().set(sig, disp)`) are
independent critical sections; they do *not* synchronise with a
posting thread's pending-mask update.

### 2.3 Where wakes happen today

**None of the signal subsystem reaches into the bus.** A grep for
`Channel`, `Waker`, `RawPort`, `RawQueue`, `fire(`, `TaskMailbox`,
`WaitSource` across `signal.rs`, `signal/`, and the
`thread_runtime::execution` mutators returns zero hits.

The current model is "denormalised summary observed at poll
boundary":

1. `post_signal(thread, sig)` flips a bit in `thread_pending`
   under `Release`, then `update_summary(|s| s.deliverable_signal
   = true)` if the signal is not in `signal_mask`. No wake of any
   parked future occurs from the post itself.
2. A future blocked inside `Channel::wait_event` is woken **by
   whatever other thing fires the underlying channel** (the pipe
   ring filling, the futex unblocking, the exit_source going
   live). On its next poll, `WaitProtocol::classify_interrupt`
   reads `interrupt_summary().deliverable_signal` and resolves
   the future to `WaitOutcome::Interrupted` (returning `-EINTR`
   to the caller).
3. If nothing else fires the underlying channel, **the signal
   does not interrupt the wait at all** — the thread sits parked
   until a timeout, a non-signal wake, or the trap-return path
   reaches `ast_check` (which is invoked only on userspace
   re-entry, not while blocked in-kernel).

This is the **lost-wake hazard for signals** that PR-3 is supposed
to fix. The day-1 signal ADRs
(`2026-05-05-signal-day1`, `2026-05-05-signal-delivery-sweep-day1`,
`2026-05-05-signal-gewalt-event-factoring`) explicitly defer the
"actually interrupt the blocked wait" plumbing and call it out as
follow-up work. D9 is that follow-up's design.

### 2.4 Existing operations

| Operation | Free fn | StepOp wrap | Where it lives |
|---|---|---|---|
| `kill(pid, sig)` | `step_kill_process` (sig 547) | `KillProcessOp` (sig 920) | `signal.rs` |
| `kill(-pgid, sig)` | `step_kill_pgrp` (sig 640) | `KillPgrpOp` (sig 940) | `signal.rs` |
| `sigaction(sig, disp)` | `step_sigaction` (sig 666) | `SigactionOp` (sig 958) | `signal.rs` |
| `sigprocmask(how, mask)` | `step_sigprocmask` | `SigprocmaskOp` | `thread_runtime/execution.rs` |
| Gewalt routing | `route_gewalt` (sig 594) | (composed into `step_kill_process`) | `signal.rs` |
| TTY → signal bridge | `deliver_tty_dispatch` (sig 884) | — | `signal.rs` |
| Permission check | `script_kill_process` (sig 721), `script_kill_pgrp` (sig 786), `script_kill_probe` (sig 748) | — | `signal.rs` |

Selection-side primitives — `select_next_signal`, `ast_check`,
`ast_dispatch` (signal.rs §"Delivery-side types") — are pure
observers of pending+mask+disposition. They run on a thread's own
behalf at trap return; they do not park anyone.

### 2.5 Important wrinkles surfaced by the survey

1. **`step_kill_process` does not run an eligibility check.** It
   posts to `threads.iter().find(|t| !t.is_zombie())` — i.e. the
   first non-zombie thread, regardless of its `signal_mask`. This
   is a known POSIX-conformance gap (signum-mask filtering during
   process-directed delivery is supposed to pick a thread that
   has `sig` unblocked when one exists). The migration is the
   right moment to fix it.

2. **Disposition lookup is racy.** `step_sigaction` writes to
   `sig_actions` under one SpinMutex; `ast_check` reads from it
   under a different acquisition. Posting and disposition writes
   don't serialise. POSIX permits this in principle (the order of
   "kill" and "sigaction" is unspecified when concurrent), but
   it means the wake design **must not** make the wake-eligibility
   decision under the disposition lock — the lock is too narrow
   to serve as the routing lock. Eligibility check has to live
   on the threads list + per-thread state.

## 3. Wake-substrate primitives — what's already there

From `crates/tx-substrate/src/wake/mailbox.rs` and
`wait_source.rs` (per D4 these moved out of `tx-reactor` into
`tx-substrate::wake` in PR-3D-0):

```rust
pub struct TaskMailbox {
    generation: AtomicU64,
    queue:      SpinMutex<VecDeque<MailboxEvent>>,
    overflow:   AtomicBool,
    waker:      SpinMutex<Option<core::task::Waker>>,
}

pub enum MailboxEvent {
    SourceFired { generation: WaitGeneration, source: WaitSourceId, interests: InterestMask },
    AgentReplied { token_id: DelegateTokenId },        // PR-7B
    Abort        { token_id: DelegateTokenId, reason: AbortReason }, // PR-7B
}

pub struct WaitSource {
    id:           WaitSourceId,
    subscribers:  SpinMutex<Vec<Subscriber>>,
    ...
}
```

The crucial property: `TaskMailbox::post(MailboxEvent)` calls
`waker.wake_by_ref()` if a waker is registered. So *any* event
posted to a thread's mailbox wakes its `core::task::Waker` and
re-polls its future. The future's next poll observes
`interrupt_summary()` and proceeds.

`MailboxEvent` is `#[derive(Clone, Copy, Debug, Eq, PartialEq)]`.
Adding a new variant is an additive change that affects the
existing `match` site in `ActiveWait::matches` (which only treats
`SourceFired` as a wait-source match and explicitly ignores
`AgentReplied`/`Abort`).

## 4. Design options

### Option A — `MailboxEvent::SignalDelivered { signum }` variant; signal subsystem holds direct `Weak<TaskMailbox>` per thread

**Sketch.** Add to `MailboxEvent`:

```rust
SignalDelivered {
    signum: Signum,
    /// "process-directed" vs "thread-directed" so the driver
    /// can attribute the wake to siginfo si_code later.
    routing: SignalRouting,
},
```

`ThreadPayload` grows a `mailbox: Weak<TaskMailbox>` field
(populated when the thread is bound to a reactor task — D4 already
foresees this). `post_signal` (and `route_gewalt`'s per-thread
loop) post a `SignalDelivered` event to the chosen thread's
mailbox after the bitset CAS. `ActiveWait::matches` ignores
`SignalDelivered` (it isn't a wait-source match) — but the
mailbox's `post` still wakes the registered `Waker`, so the future
re-polls and `interrupt_summary().deliverable_signal` is observed
on the next pass.

**Routing**: `step_kill_process` enumerates `threads.lock()` under
the process's thread-list lock, picks the first thread with `sig`
unblocked and not already terminating, performs the bitset CAS,
posts `SignalDelivered`, drops the lock. Two `kill(pid, sig)` calls
racing on the same target serialise on the thread-list lock;
only one CAS succeeds in *first* setting the bit (POSIX
coalescence for standard signals), but both posters post — the
mailbox queue absorbs the duplicate, and `interrupt_summary` will
be observed the same way regardless.

**Pro:**
- Smallest change in semantic shape. Signal stays "denormalised
  summary observed at poll boundary"; the only addition is a
  wake hint that forces the future to re-poll *now* rather than
  waiting for some unrelated channel to fire.
- No per-thread `WaitSource` object. The lookup table is the
  process's `threads` Vec — already exists.
- Directly addresses the lost-wake hazard called out in §2.3.

**Con:**
- Introduces a `MailboxEvent` variant that does not pair with a
  `WaitSource` registration. `ActiveWait::matches` has to
  explicitly drop it (the same way it drops `AgentReplied`/
  `Abort`). The driver shape is "the mailbox wakes; the
  future re-polls; the future consults `interrupt_summary`."
  Less uniform than the pipe/futex flow but matches signal's
  actual semantics: there is no semantic object whose state
  transition is being observed.
- `signalfd`/`sigwaitinfo` (see §6) need a *second* path because
  they consume signals like an event source, not like an
  interrupt. Option A handles this with a *companion*
  `WaitSource` registered on the signalfd's `SignalSubscription`,
  not on the thread.

### Option B — Per-thread `Arc<WaitSource>` for signal-readiness

**Sketch.** Each `ThreadPayload` owns an `Arc<WaitSource>`
(`signal_wait_source`). `post_signal` calls
`payload.signal_wait_source.notify(interest_mask)`. The signum
itself does not travel through the event — the firing mask
encodes "a signal arrived", and the post-wake recheck inspects
`thread_pending` + `signal_mask` to find out which one.

**Pro:**
- Reuses the per-object template that the other five subsystems
  use. Future migrations (e.g. signal restart semantics) can
  re-use `PreparedWaitRegistration`'s lost-wake fix verbatim.
- The signal subsystem participates in the bus-retirement
  story uniformly: every wake goes through `WaitSource.notify`.

**Con:**
- `WaitSource` is "object-owned wait publication." A *thread* is
  not the right owner — it is a subscriber, not a publisher. The
  invariant we'd be inverting is **"sources retain
  publication identity"**. The signal-arrival predicate is
  "this thread's pending intersects this thread's
  !signal\_mask" — that is a *per-subscriber predicate, not a
  source-side predicate*. Forcing it through `WaitSource` means
  every subscriber to that source has to re-test for itself,
  which is exactly the shape `WaitSource` was designed to
  avoid.
- The eligibility check in `step_kill_process` would have to
  iterate threads (still doable, but now we are paying for a
  full `WaitSource` allocation per thread just to deliver one
  post-once event).
- Group-broadcast signals (`route_gewalt` for SIGSTOP/SIGCONT)
  iterate all threads, calling `notify` on each. That works,
  but it is the wrong primitive — these are control ops, not
  readiness transitions.

### Option C — Hybrid: per-process `WaitSource` for shared-pending changes (sigwait-shaped callers) + per-thread `MailboxEvent::SignalDelivered` for delivery

**Sketch.** Two channels:

1. **Per-thread direct post.** Same as Option A. The default
   delivery path posts a `SignalDelivered` event onto the chosen
   thread's mailbox. This wakes the running thread for
   handler/default-action processing.
2. **Per-process `WaitSource` on `group_pending`.** Used only by
   `sigwait`/`sigwaitinfo`/`signalfd` callers. A caller that
   wants to *consume signals as events* registers a
   `WaitRegistrationGuard` on the process's
   `signal_event_source`; transitions on `group_pending` fire it.
   This source's `notify` mask carries the signum bits, so
   `sigwaitinfo` can resume with a specific signum.

**Pro:**
- Keeps Option A's clean, low-cost delivery path for the common
  case (running threads getting `kill(pid, sig)`).
- Gives a *real* `WaitSource` to `signalfd`-style consumers,
  which actually fit the source-fires-event pattern (they
  treat signals as a queue to drain). The eligibility check
  that picks a thread vs. routes to a signalfd is encoded by
  the disposition table — `signalfd` installs a
  `SigDisposition::ConsumedBySignalfd` (new variant) so
  `step_kill_process` skips the thread-walk and notifies the
  process's signal-event source directly.
- Cleanly separates the *interrupt-the-blocked-thread* concern
  (Option A path) from the *deliver-signals-as-events* concern
  (the per-process WaitSource path).

**Con:**
- Two paths to maintain. The disposition lookup must classify
  signalfd-bound signums before deciding which path to take.
- The per-process source is allocated even for processes that
  never install a signalfd. (Mitigation: lazily create it on
  first `signalfd` syscall.)

## 5. Per-process broadcast cases (SIGSTOP / SIGCONT)

These are control ops, not deliveries. `route_gewalt` already
iterates `threads.lock()` and flips
`signal_summary.stop_requested` on every live thread. Under any of
options A/B/C, the migration adds **one** thing: after flipping
the bit, post `MailboxEvent::SignalDelivered { signum:
SIGSTOP_or_SIGCONT, routing: ProcessDirected }` to each thread's
mailbox so that **every** parked future is re-polled and observes
the new summary.

For SIGKILL the answer is simpler: `step_exit_group_with_signal`
already zombifies every thread. The wake-side concern reduces to
"make sure each thread's mailbox is posted before its `Cap`
becomes a zombie reference." The natural integration point is
`set_thread_zombie` (in `thread_runtime/execution.rs`); it can
post a final `SignalDelivered { signum: SIGKILL, routing:
ProcessDirected }` (or, better, a new `MailboxEvent::Terminated`
variant) before dropping the payload. The future's next poll will
see `summary.termination` and return `WaitOutcome::Killed`.

## 6. `pselect` / `sigwaitinfo` / `signalfd` integration

- **`pselect(set, mask)` / `ppoll`** — atomically install the
  signal mask for the duration of the wait. Under any option,
  the wake path is unchanged: `step_sigprocmask` updates the
  mask, recomputes `summary.deliverable_signal`, and (under
  the recommended Option A/C wake plumbing) posts a
  `SignalDelivered` event if unblocking exposed a pending signal.
  The blocked `pselect` future re-polls and `classify_interrupt`
  returns `Interrupted`. **No new primitive needed.**
- **`sigwaitinfo(set, info)`** — caller wants to *consume* the
  first matching pending signal and have it not run a handler.
  This fits a `WaitSource`-on-pending shape. Option C's
  per-process `signal_event_source` is the natural endpoint:
  the future registers with `interests = set.bits()`, and
  `notify` carries the firing signum bits. Resume clears the
  bit from `thread_pending`/`group_pending` and returns the
  signum.
- **`signalfd(set)`** — like `sigwaitinfo` but exposed as an fd
  whose readiness is "set ∩ pending ≠ 0". In Option C this is
  built on the same per-process `signal_event_source` plus a
  per-fd `WaitSource` mirroring readability. In Option A this
  is a follow-up: a `SignalSubscription` object owns its own
  `WaitSource`; the disposition table grows a
  `SigDisposition::ConsumedBy(SignalSubscriptionId)` variant
  so `step_kill_process` routes to the subscription's source
  instead of a thread. Either way, signalfd is not a Phase A
  concern — `signalfd(2)` is not yet implemented and the
  ABI is in `Deliberately deferred` per `signal.rs:17–29`.

## 7. Recommendation — **Option A** (with a recorded path to Option C for `signalfd`)

### Rationale

1. **Correctness.** Option A's wake plumbing exactly matches the
   `THREAD_RUNTIME_v1` §5.2 design: the
   `InterruptSummary.deliverable_signal` bit is *truth*, and
   `MailboxEvent::SignalDelivered` is a *hint that forces
   re-poll*. The future re-checks `interrupt_summary` and
   `WaitProtocol::classify_interrupt` does the right thing.
   No new selection logic crosses lock boundaries.

2. **Wake-path latency.** A direct
   `Weak<TaskMailbox>::upgrade()` + `mailbox.post(event)` is
   one allocation-free atomic write + one bounded queue push +
   one waker wake. No `WaitSource::notify` walk over a
   subscribers Vec. The thread-walk inside `step_kill_process`
   already does an O(threads) scan to find a non-zombie
   thread; the migration extends that scan with a
   sigmask-eligibility check (still O(threads)) and one
   mailbox post.

3. **Blast radius.** Option A adds **one variant** to
   `MailboxEvent`, **one field** to `ThreadPayload`, and changes
   **`post_signal` + `route_gewalt` + `set_thread_zombie`** to
   call `mailbox.post`. The eligibility-check fix in
   `step_kill_process` is a small follow-up local to that
   function. `select_next_signal`/`ast_check`/`ast_dispatch`,
   the disposition table, the StepOp wraps, and the
   permission-check wrappers are **all untouched**. The
   `signal/tests/delivery.rs` tests that observe
   `interrupt_summary` after `post_signal` still pass as-is
   because the summary update path is preserved.

4. **Option C is reachable from Option A.** When `signalfd`
   lands, adding the per-process `signal_event_source` is
   purely additive: the disposition table gets a new
   `SigDisposition::ConsumedBy(SignalSubscriptionId)` variant
   that `step_kill_process` checks before falling through to
   the thread-walk. Option A does not foreclose Option C.

5. **Per-thread `WaitSource` (Option B) is the wrong shape.** A
   thread is a subscriber, not a publisher. The signal-arrival
   predicate is per-subscriber, not per-source. Forcing the
   per-thread shape through `WaitSource` would invert the
   ownership rule the PR-3 ADR was at pains to establish.

### Trade-offs acknowledged

- Two `MailboxEvent` variants now do not match an `ActiveWait`
  (`AgentReplied`/`Abort` from PR-7B, plus the new
  `SignalDelivered`). The driver shape "post wakes the Waker
  regardless; the future inspects truth at poll" is consistent
  with all three.
- The per-thread `Weak<TaskMailbox>` field overlaps with the
  reactor's task-binding state; the implementation needs to
  populate it during the existing reactor-task hookup (the
  same place `ThreadPayload.task: Option<TaskKey>` is set).
  This wiring is independently scheduled by PR-3D / D4.

## 8. Migration plan

Three phases, ~3 days end-to-end. Each phase is independently
mergeable.

### Phase D9-A — `MailboxEvent::SignalDelivered` + post-on-deliver wiring (1 day)

**Goal.** Add the new event variant and have every catchable-signal
delivery path post to the chosen thread's mailbox.

Touches:

- `crates/tx-substrate/src/wake/mailbox.rs` — add
  `MailboxEvent::SignalDelivered { signum, routing }` variant.
  Update `ActiveWait::matches` to return `false` for it
  (mirroring `AgentReplied`/`Abort`).
- `crates/tx-subsystems/src/signal.rs` — add `SignalRouting`
  enum (`ProcessDirected` / `ThreadDirected` / `GroupDirected`).
- `crates/tx-subsystems/src/thread_runtime/structure.rs` — add
  `pub(crate) mailbox: SpinMutex<Option<Weak<TaskMailbox>>>` to
  `ThreadPayload`; provide `bind_mailbox(Weak<TaskMailbox>)` /
  `take_mailbox() -> Option<Weak<TaskMailbox>>`.
- `crates/tx-subsystems/src/thread_runtime/execution.rs` —
  modify `post_signal` to also post `SignalDelivered { signum,
  routing: ThreadDirected }` to the bound mailbox, after the
  `update_summary` call. No-op when the mailbox is unbound (the
  invariant during early bring-up).
- `crates/tx-subsystems/src/signal.rs` — modify
  `route_gewalt`'s thread loop to post `SignalDelivered { signum:
  SIGSTOP_or_SIGCONT, routing: ProcessDirected }` per thread
  after `update_summary`.
- `crates/tx-subsystems/src/thread_runtime/execution.rs` —
  modify `set_thread_zombie` to post `SignalDelivered { signum:
  SIGKILL, routing: ProcessDirected }` (or a new
  `MailboxEvent::Terminated` variant — choose during impl) to
  the bound mailbox before dropping the payload.

**Verification.**

- Existing `signal/tests/delivery.rs` continues to pass (the
  tests observe `interrupt_summary`, which still updates).
- New unit test: register a `TaskMailbox`, bind it to a thread
  via `bind_mailbox`, call `post_signal`, drain the mailbox,
  assert one `SignalDelivered` event with the expected signum.
- New unit test: SIGSTOP via `step_kill_process` posts
  `SignalDelivered` to *every* live thread's mailbox.

### Phase D9-B — Process-directed eligibility check (1 day)

**Goal.** Fix `step_kill_process`'s "first non-zombie thread"
defect by selecting an eligible thread (one whose `signal_mask`
permits the signum).

Touches:

- `crates/tx-subsystems/src/signal.rs:547` —
  `step_kill_process` rewrites the thread selection: under
  `payload.threads.lock()`, iterate threads, prefer the first
  one with `!signal_mask.is_blocked(sig)`. Fall back to the
  first non-zombie thread if none have the signal unblocked
  (POSIX permits the signal to remain pending on a thread that
  has it blocked).
- The selection runs *while holding* the thread-list lock, so
  two concurrent `step_kill_process` calls serialise on it. The
  CAS into `thread_pending` happens before the lock is dropped.
  This is the "routing under the same lock so two threads don't
  both wake on the same signum" property the W-S quote calls
  out.
- The mailbox post happens *after* the bitset CAS, *outside*
  the lock, to avoid the post-while-holding-list-lock hazard.

**Verification.**

- New unit test: process with 3 threads, T2 has SIGTERM
  unblocked, T1/T3 have it blocked. `kill(pid, SIGTERM)` must
  set the bit on T2's `thread_pending`, not T1 or T3.
- New unit test: process with 3 threads, all have SIGTERM
  blocked. `kill(pid, SIGTERM)` falls back to the first thread
  and posts the signal (it stays pending until that thread
  unblocks it).
- New unit test: two parallel `kill(pid, SIGTERM)` posts on a
  multi-thread process must result in exactly one bit set
  (coalescence) and exactly the chosen thread's mailbox
  receiving the post.

### Phase D9-C — `pselect`/`sigwaitinfo` recheck + test-pin (1 day)

**Goal.** Validate the wake path end-to-end against an
interruptible wait, and pin the `signalfd` follow-up.

Touches:

- `crates/tx-reactor/src/wait.rs` — no API change required, but
  audit `WaitFuture` / `WaitEventFuture::poll` to confirm that
  a posted `SignalDelivered` event causes the registered
  `Waker` to fire and re-poll. (The mailbox wakes the waker;
  the WaitFuture's poll then consults
  `InterruptSource::interrupt_summary`. This already works
  today — the test pin is to lock in the integration.)
- New integration test under
  `crates/tx-subsystems/src/signal/tests/`: park a thread on a
  `Channel::wait_event(_, WaitProtocol::Interruptible, ...)`
  whose underlying channel never fires; `kill(pid, SIGTERM)`
  from another thread must cause the wait to resolve as
  `Interrupted` within a bounded number of reactor steps. This
  is the lost-wake fix's primary regression test.
- ADR follow-up file `signalfd-followup.md` recording the
  Option C add-on path so the signalfd plan does not get
  re-litigated when it lands. (Filed but not implemented.)

**Verification.**

- The new integration test exists and passes.
- `cargo check --workspace` clean.
- `cargo xtask progress validate` if any JSON records are
  added.

## 9. Compatibility with existing tests

The 480-line `signal/tests/delivery.rs` body asserts on
`InterruptSummary`, `select_next_signal`, `ast_check`, exit-status
encoding, and Gewalt routing. **None of those assertions are
invalidated** by D9: the summary path remains the truth-bearing
mechanism, the selection-side helpers are untouched, and the
exit-status path is unchanged.

The one test that *might* be affected is whichever existing test
exercises `post_signal` without first binding a mailbox to the
thread — Phase D9-A handles that by making the mailbox post a
no-op when `Weak<TaskMailbox>::upgrade()` returns `None`. So all
the existing free-fn-only tests continue to pass with no
modification.

The `step_op_wraps` tests inside `signal.rs:982` continue to pass
because the StepOp wraps are unchanged.

## 10. Relationship to existing ADRs

- **PR-3 shape ADR** named "task-owned wake delivery + object-owned
  wait publication." D9 confirms that signal is the principled
  *non-object-owned* case: the wake destination is task-owned
  (per-thread `Weak<TaskMailbox>`) but the publication side is
  *not* object-owned in any meaningful sense — signals are
  multi-publisher, multi-subscriber, with the selection rule
  encoded in code, not in a `WaitSource`.
- **D2** chose parallel `WaitSource`/`RawPort` surfaces during
  the bus migration. D9 is fully compatible: signal does not
  consume the bus today, so the parallel-surface story does not
  apply. The new `MailboxEvent::SignalDelivered` variant is
  additive.
- **D4** mandated that `TaskMailbox` / `WaitSource` live in
  `tx-substrate::wake`. D9 adds a variant to `MailboxEvent`
  there, with no layering change.

## 11. Out-of-scope follow-ups (recorded, not committed)

- **Realtime signal queuing** (per-occurrence queue for signums
  32..=64). The current `PendingSignalQueue` is a bitset;
  realtime ABI requires a queue. D9's wake plumbing does not
  preclude this — `MailboxEvent::SignalDelivered { signum, ... }`
  carries one signum at a time, and the queue update remains
  internal to `thread_pending` / `group_pending`. Schedule a
  follow-up ADR when the queue lands.
- **`signalfd(2)`** — Option C's add-on (per-process
  `signal_event_source` + disposition variant
  `ConsumedBy(SignalSubscriptionId)`). Defer to its own ADR;
  Phase D9-C records the design as a follow-up file but does
  not implement it.
- **`SigInfo` payload** (`si_code`, `si_pid`, `si_value`). Day-1
  `Signum` carries only the bit; `MailboxEvent::SignalDelivered`
  carries only the signum + routing. When `siginfo` lands, the
  payload either rides in `MailboxEvent::SignalDelivered`
  (preferred — keep the event self-contained) or rides
  alongside it in a per-thread siginfo slot drained by
  `ast_check`. Pick during the realtime-queue ADR; both shapes
  are reachable from D9.
