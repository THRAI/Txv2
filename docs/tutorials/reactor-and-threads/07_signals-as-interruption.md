# Part 7 — Signals as Wait Interruption

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 6 — Interrupts and Wakes](06_interrupts-and-wakes.md) · **Next:** [Part 8 — End to End](08_end-to-end.md)

Signals touch the future model in two places: they must **interrupt** a thread
parked in a blocking syscall (the source of `EINTR`), and they must be **delivered**
— turned into a handler frame on the user stack — at a safe point. This chapter
shows both: the lost-wake-safe interruption path, and the AST checkpoint where
delivery happens. Throughout, the mapping is to the traditional "`signal_pending()`
in interruptible sleep" + "deliver signals on return to user."

## The traditional model, briefly

```
# while sleeping interruptibly:
if signal_pending(current) and state == TASK_INTERRUPTIBLE:
    remove from wait-queue; return -ERESTARTSYS / -EINTR

# on the path back to userspace:
if signal_pending(current):
    setup_signal_frame()   # push handler context onto user stack, redirect PC
```

Two jobs: wake an interruptible sleeper, and deliver at the return-to-user
boundary. The future model keeps both jobs and both boundaries.

## Signal state lives in the `ThreadPayload`

The signal-related fields of `ThreadPayload` (Part 2, deferred until now):

```rust
// crates/tx-subsystems/src/thread_runtime/structure.rs:140 (signal subset)
struct ThreadPayload {
    signal_mask:    AtomicU64,          // blocked set (sigprocmask)
    thread_pending: PendingSignalQueue, // per-thread pending bitset
    group_pending_summary: AtomicU64,   // conservative process-group hint
    signal_summary: AtomicU8,           // InterruptSummary, packed (the fast check)

    saved_signal_context: SpinMutex<Option<UserTrapContext>>, // pre-handler regs
    saved_signal_mask:    SpinMutex<Option<SignalMask>>,      // pre-handler mask
    alt_stack:            SpinMutex<Option<(usize, usize)>>,  // sigaltstack
    mailbox: SpinMutex<Option<Weak<TaskMailbox>>>,            // the wake channel
    // ...
}
```

The cheap, hot field is `signal_summary` — an `InterruptSummary` packed into a byte:

```rust
// crates/tx-subsystems/src/signal/mod.rs:442
struct InterruptSummary {
    deliverable_signal: bool,   // a signal is pending & unblocked
    termination: bool,          // SIGKILL / fatal already escalated
    stop_requested: bool,       // SIGSTOP-family wants us parked
}
```

A parked future can check "do I need to wake for a signal?" by reading one atomic
byte — no lock, no scan. That is the futures analogue of `signal_pending()`.

## Interruption: the lost-wake-safe path

The hard part of signal interruption is the **lost wakeup**: a signal posted in the
window between "check condition" and "park" must not be missed. A traditional kernel
solves this with the wait-queue lock and `set_current_state` ordering. The future
model solves it with the mailbox.

When a signal is sent (`step_kill_process`, `signal/mod.rs:1247`), the routing path
does **two** things per affected thread:

```rust
// signal/mod.rs:739, 1344 (pseudocode)
// 1. Update the authoritative state: set the pending bit + refresh signal_summary.
thread_payload.signal_summary.set(deliverable_signal = true);
// 2. Post a wake-hint to the thread's mailbox.
post_signal_mailbox(&thread_payload, sig, SignalRouting::ProcessDirected);
```

`post_signal_mailbox` posts a `MailboxEvent::SignalDelivered` and, crucially, fires
the registered `Waker`:

```rust
// crates/tx-substrate/src/wake/mailbox.rs:187
enum MailboxEvent {
    SourceFired { generation, source, interests },
    SignalDelivered { signum: u32, routing: SignalRouting },   // <- the wake-hint
    // ...
}
```

Now the subtle, important design choice: **`SignalDelivered` does not "match" the
wait.** Recall `ActiveWait::matches` from Part 5 — it returns `false` for
`SignalDelivered`. So the event does not satisfy the wait's *condition*. Instead it
forces a **re-poll**, and on re-poll the future consults the authoritative
`signal_summary` (via `WaitProtocol::classify_interrupt`) to decide whether it was
interrupted. The comment in the source puts it well: *the `SignalDelivered` event is
a wake-hint; the truth lives in `InterruptSummary`.*

This split is what makes it lost-wake-safe:

- The **summary bit** is the durable truth. If it is set before the future's next
  poll, the future sees it — no matter when the post happened.
- The **mailbox post + `wake()`** is only a nudge to *cause* that next poll. If it
  races and arrives "too early," the re-poll still reads the summary correctly; if
  it arrives "late," the summary was already set, so a subsequent poll catches it.

The waits classify themselves by protocol:

```rust
// crates/tx-substrate/src/step/wait_protocol.rs:15
enum WaitProtocol {
    Uninterruptible,   // no signal disturbs it
    Interruptible,     // any signal -> Interrupted
    Killable,          // only SIGKILL disturbs it
}

// crates/tx-substrate/src/step/wait_protocol.rs:57
enum WaitOutcome { Ready, Interrupted, Killed, TimedOut }
```

A blocking syscall (Part 5) that parked with `Interruptible` therefore resolves as
`WaitOutcome::Interrupted` when a signal's summary bit is set, and the handler maps
that to `SyscallResult::Error(EINTR)`. The reactor `wait.rs` variant adds timeout
flavors (`InterruptibleTimeout(deadline)`, `KillableTimeout(deadline)`).

### The regression test that pins it

`crates/tx-subsystems/tests/v3_signal_interrupt_wake.rs` exists precisely to nail
this. Its setup (paraphrased from the test's own doc comment):

1. Bind a `TaskMailbox` to a one-thread process's `ThreadPayload`.
2. Submit a task awaiting a `Channel::wait_event(..., Interruptible, || false)` —
   the condition is *always false*, so the only way out is interruption. The
   underlying `Channel` is **never fired**.
3. Run the reactor once → the task parks.
4. `step_kill_process(proc, SIGTERM)` → sets `summary.deliverable_signal` **and**
   posts `SignalDelivered`, waking the task.
5. Run the reactor again → re-poll → `classify_interrupt` sees the summary →
   the future resolves `WaitOutcome::Interrupted`.

The asserted invariants: the outcome is `Interrupted` (not `Ready`, since the
condition was always false), and it happens in a bounded number of ticks (the real
path is exactly two: park, then post-wake re-poll). "The Channel was never fired" —
the *only* wake path was the mailbox. That test is the executable proof that signal
interruption is lost-wake-safe in the future model.

## Delivery: the AST checkpoint

Interruption gets the thread *out* of its wait and back into the `run_thread` loop.
But actually delivering the signal — building a handler frame — happens at a
controlled boundary: **Phase 1 of the loop, before re-entering userspace** (Part 2).
This is the "deliver on return to user" boundary, now an explicit checkpoint.

```rust
// run_thread Phase 1 (Part 2), expanded
ast_checkpoint(&payload):
    match ast_check(&thread) {                       // signal/mod.rs:836
        AstOutcome::Continue          => { /* nothing to do; enter userspace */ }
        AstOutcome::DeliverHandler { sig, action } => {
            // save current regs+mask, build the handler frame, redirect PC
            payload.store_saved_signal_context(payload.saved_user_context());
            payload.store_saved_signal_mask(current_mask());
            build_signal_frame(&payload, sig, action);   // pushes onto (alt) stack
        }
        AstOutcome::DefaultTerminate { sig } => { /* exit process group */ }
        AstOutcome::DefaultStop { sig }      => { payload.set_stopped(true); }
        AstOutcome::DefaultContinue { sig }  => { /* clear stop */ }
        AstOutcome::InitiateTermination      => { /* SIGKILL: exit, never enter user */ }
    }
```

```rust
// crates/tx-subsystems/src/signal/mod.rs:808
enum AstOutcome {
    InitiateTermination,                          // SIGKILL / fatal escalated
    DefaultTerminate { sig: Signum },             // Term/Core default
    DefaultStop      { sig: Signum },             // Stop default -> park
    DefaultContinue  { sig: Signum },             // Cont
    DeliverHandler   { sig: Signum, action: SigActionEntry }, // user handler
}
```

`ast_check` (`signal/mod.rs:836`) reads the summary, picks the lowest-numbered
deliverable signal, consults the disposition table, and returns the intent. The
`DeliverHandler` path is the interesting one: it saves the pre-handler
`UserTrapContext` and mask into `saved_signal_context` / `saved_signal_mask`, then
constructs the handler frame so that when the thread re-enters userspace it runs the
handler instead of resuming where it was.

### `rt_sigreturn`: the trip back

When the user handler finishes it calls `sigreturn`. That syscall is the inverse of
delivery: it drains `saved_signal_context` / `saved_signal_mask` back into
`saved_user_context` and the mask, so the *next* userspace entry resumes the
original execution. In the future model this is just another syscall arm in Phase 4
that writes the payload — no special machinery beyond the saved-context slots.

## The two boundaries, mapped

| Job | Traditional boundary | Future-model boundary |
|---|---|---|
| Wake an interruptible sleeper | `wake_up` + `signal_pending` in `schedule()` | `SignalDelivered` post + `Waker::wake()`; re-poll reads `signal_summary` → `Interrupted` → `EINTR` |
| Deliver (build handler frame) | `do_signal()` on return-to-user | `ast_check` in `run_thread` Phase 1, before `enter_userspace_with_context` |
| Return from handler | `sys_rt_sigreturn` restores sigframe | `sys_rt_sigreturn` arm drains `saved_signal_context` into the payload |
| `SIGSTOP` / `SIGCONT` | `TASK_STOPPED` state | `payload.stopped` flag checked at Phase 1 (Part 2) |

The boundaries are the *same two* a traditional kernel uses. The difference is that
each is now an explicit, testable step in an `async` state machine rather than an
implicit consequence of where the kernel stack happened to be.

## Summary

- Signal state lives in `ThreadPayload`; the hot path is `signal_summary`
  (`InterruptSummary` packed into a byte) — the futures `signal_pending()`.
- **Interruption is lost-wake-safe by construction:** the authoritative truth is the
  summary bit (set before any wake); `MailboxEvent::SignalDelivered` is only a
  *wake-hint* that forces a re-poll. `ActiveWait::matches` deliberately ignores it.
  An `Interruptible` wait then resolves `WaitOutcome::Interrupted` → `EINTR`.
  `v3_signal_interrupt_wake.rs` pins exactly this in ≤2 reactor ticks with the
  underlying channel never fired.
- **Delivery happens at the AST checkpoint** in `run_thread` Phase 1: `ast_check`
  returns an `AstOutcome`; `DeliverHandler` saves the pre-handler context/mask and
  builds the frame; `rt_sigreturn` restores it later. `SIGSTOP` sets
  `payload.stopped`, parking the thread before userspace entry.
- Both boundaries match the traditional model — they are just explicit steps in the
  async loop now.

---

**Anchors:**
- signal fields: `crates/tx-subsystems/src/thread_runtime/structure.rs:140`
- `InterruptSummary`: `crates/tx-subsystems/src/signal/mod.rs:442`
- `ast_check` / `select_next_signal` / `AstOutcome`: `crates/tx-subsystems/src/signal/mod.rs:836,575,808`
- signal send + mailbox post: `crates/tx-subsystems/src/signal/mod.rs:1247,1344,739`; `post_signal_mailbox` at `crates/tx-subsystems/src/thread_runtime/execution.rs:165`
- `MailboxEvent::SignalDelivered`: `crates/tx-substrate/src/wake/mailbox.rs:187`
- `WaitProtocol` / `WaitOutcome`: `crates/tx-substrate/src/step/wait_protocol.rs:15,57`; reactor variant `crates/tx-reactor/src/wait.rs:39,94`
- regression test: `crates/tx-subsystems/tests/v3_signal_interrupt_wake.rs`

**Next:** [Part 8 — End to End](08_end-to-end.md)
