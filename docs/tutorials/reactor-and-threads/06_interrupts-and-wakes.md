# Part 6 — Interrupts and Wakes: Who Calls `wake()`

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 5 — Blocking Syscalls as Futures](05_blocking-syscalls-as-futures.md) · **Next:** [Part 7 — Signals as Interruption](07_signals-as-interruption.md)

Part 5 left a parked `read` future waiting on a `WaitSource`, and a question: who
makes the condition true and fires the waker? For I/O, the answer is a device
interrupt. This chapter follows a hardware IRQ from the trap vector to
`Waker::wake()`, and then handles the special case of the *timer* interrupt — the
mechanism that lets a cooperative poll loop still preempt a CPU-bound userspace
thread.

## Two kinds of interrupt, two jobs

- A **device IRQ** (UART, virtio) signals that an external event happened. Its job
  is to make some `WaitSource` ready and wake whichever task was parked on it.
- A **timer IRQ** signals that a time-slice elapsed. Its job is to force the
  *currently running userspace thread* to yield the hart so the scheduler can run
  someone else.

Both arrive through the same asm vector and `classify_rv64_trap` (Part 3), but they
take different `TrapAction` paths.

## Device IRQ: from wire to `wake()`

### The trap-shell side

```rust
// KernelTrapSink::on_external_irq  (trap.rs:67, pseudocode)
fn on_external_irq(...) -> TrapAction {
    let irq = P::claim();                       // PLIC claim
    let handled = P::dispatch_irq(irq);         // run the registered handler
    P::complete(irq);                           // PLIC complete (EOI)
    match handled {
        IrqHandled::Wake    => TrapAction::Reschedule,  // a task may now be runnable
        IrqHandled::Done    => TrapAction::Resume,       // nothing to reschedule
        IrqHandled::NotMine => TrapAction::Resume,
    }
}
```

```rust
// crates/tx-hal/src/lib.rs:1180
enum IrqHandled { Done, Wake, NotMine }
```

Handlers are registered in a dispatch table at boot:

```rust
// crates/tx-kernel/src/irq.rs
static IRQ_DISPATCH_TABLE: ... ;                       // irq.rs:45
fn register_irq_handler(irq, handler: IrqHandlerFn);   // irq.rs:85
fn install_irq_handlers::<P>();                        // irq.rs:159
```

### The IRQ context restriction (a real constraint)

An IRQ handler runs in interrupt context. In this kernel that means it **cannot
create an epoch (EBR) guard** — so it cannot touch capability-managed structures
like the TTY line discipline. This forces a two-phase design that is worth seeing,
because it is where "interrupt context vs. process context" shows up concretely.

**Phase A — in IRQ context, do the minimum:**

```rust
// crates/tx-kernel/src/irq.rs:182
fn uart_rx_irq_handler<P: ConsoleIf>(_irq: u32) -> IrqHandled {
    let mut buf = [0u8; UART_RX_DRAIN_MAX];
    let n = P::read_bytes(&mut buf);            // pull bytes out of the UART FIFO
    if n == 0 { return IrqHandled::Done; }      // spurious
    if console_tty().is_none() { return IrqHandled::NotMine; } // pre-boot race

    // Stash bytes in a plain ring buffer — no epoch guard, no TTY touch.
    let mut pending = UART_RX_PENDING.lock();
    pending.push(&buf[..n]);                    // overflow silently dropped
    IrqHandled::Wake                            // ask for a reschedule
}
```

**Phase B — back in the reactor (non-IRQ context), do the real work:**

```rust
// crates/tx-kernel/src/irq.rs:222 — called by the reactor loop after each poll
fn drain_uart_rx_pending() -> usize {
    let (bytes, n) = UART_RX_PENDING.lock().take_snapshot();   // drain + clear
    if n == 0 { return 0; }
    let tty = console_tty()?;
    let guard = guard();                        // NOW legal: process context
    tty.step_ingest(&bytes, &guard);            // push into the line discipline
    // step_ingest makes the TTY's WaitSource ready and fires subscribers' wakers
    n
}
```

`step_ingest` is what finally fires the `WaitSource` the parked `read` subscribed
to in Part 5 — posting `SourceFired` to the reader's mailbox and calling its
`Waker::wake()`. From there the wake plumbing of Part 1 takes over.

### The complete wake chain

```
UART RX line asserts IRQ
  -> asm vector -> classify -> on_external_irq
       -> uart_rx_irq_handler: FIFO -> UART_RX_PENDING ring,  return Wake
  -> TrapAction::Reschedule  (longjmp back into the reactor)
reactor loop: poll a task ... then drain_uart_rx_pending()
  -> tty.step_ingest -> TTY WaitSource fires
       -> post SourceFired to the reader's TaskMailbox
       -> TaskWakeState::wake(): push TaskId onto the global wake queue   (waker.rs:31)
next loop iteration: drain_wakes_for_hart  (runtime.rs:704)
  -> reader task Parked -> Runnable -> enqueued
  -> reactor polls run_thread -> dispatch(read).await resumes -> copies data
  -> SyscallResult::Return(n) -> pending_syscall_return -> a0 on re-entry
```

Every arrow after `step_ingest` is the machinery from Parts 1, 4, and 5. The IRQ's
only novel contribution is *bridging interrupt context to process context* via the
`UART_RX_PENDING` ring and the `Wake` request.

## Timer IRQ: preempting a cooperative loop

Recall the tension from Part 1: `poll` is cooperative — the reactor cannot
interrupt a future mid-poll. So a userspace thread spinning in a tight loop never
returns to the reactor on its own. How is it preempted?

The answer: the timer interrupt does not fire *during a poll* — it fires while the
thread is **in userspace** (between `enter_userspace_with_context` and the next
trap). At that moment the kernel is not polling anything; it is waiting inside the
divergent userspace dive. The timer trap is the kernel regaining control.

```rust
// KernelTrapSink::on_timer_interrupt  (trap.rs:52, pseudocode)
fn on_timer_interrupt(cpu, view) -> TrapAction {
    P::cancel_deadline();
    if view.previous_mode == User {
        let outcome = hand_off_timer_preempt(hart, &view);   // trap_handoff.rs:356
        if matches!(outcome, Preempted) { mark_boot_reactor_userspace_preempt(cpu); }
        timer_preempt_outcome_to_trap_action(&outcome)        // Preempted -> Reschedule
    } else {
        // timer fired in kernel context: just account and Resume
    }
}
```

The handoff mirrors the syscall one (Part 3) but resolves the wait with a
*different* trap info:

```rust
// crates/tx-kernel/src/trap_handoff.rs:356
fn hand_off_timer_preempt(hart, view) -> TimerPreemptOutcome {
    let payload = current_payload_for_hart(hart)?;
    let active  = payload.active_userspace_request()?;
    payload.store_saved_user_context(Some(view.capture_user_context())); // PC unchanged!
    payload.userspace_slot().clone()
        .record_timer_preemption(active)   // -> ActivePhase::Resolved(TimerPreempt) + wake
        .map(|_| TimerPreemptOutcome::Preempted)
}
```

Two details distinguish it from a syscall handoff:

1. **PC is captured unchanged.** A preempted thread must resume the *exact*
   instruction it was on — there is nothing to skip (contrast the `+4` past `ecall`
   in Part 3).
2. The wait resolves with `UserspaceTrapInfo::TimerPreempt`
   (via `record_timer_preemption`, `userspace.rs:300`), and fires the waker.

Back in `run_thread`, Phase 4's `TimerPreempt` arm (Part 2) does the cooperative
thing:

```rust
UserspaceTrapInfo::TimerPreempt => {
    yield_now().await;   // return Pending once; reactor runs someone else
}
// loop back to Phase 1: re-enter userspace from the unchanged saved context
```

`yield_now()` is a future that returns `Pending` exactly once (re-queuing itself as
runnable), then `Ready`. So the thread releases the hart for one scheduling
decision and re-enters userspace from where it was. Net effect: a CPU-bound thread
is forced through the reactor's pick-next logic on every timer tick — the same
outcome as a traditional preemptive tick, achieved without ever interrupting a
`poll`.

> **Note on the upgrade path:** because timer preemption resolves the wait, a
> *real* trap (syscall/fault) arriving in the same window can be upgraded over a
> pending `TimerPreempt` resolution rather than being lost —
> `complete_interesting_trap` handles that replace case (`userspace.rs:341`). The
> generation check keeps it from crossing round-trips.

## The full wake plumbing, one more time

Both interrupt kinds ultimately feed the same two-stage wake from Part 1:

```rust
// stage 1: anyone, anywhere, requests a re-poll
TaskWakeState::wake():                       // waker.rs:31
    wake_requested = true
    global_wake_queue.push(task_id)

// stage 2: the reactor loop, next iteration
drain_wakes_for_hart(hart):                  // runtime.rs:704
    for task_id in drain(global_wake_queue):
        task: Parked -> Runnable
        scheduler.enqueue(task_id)           // routed by WakeHint to a run-queue
```

Device IRQs reach stage 1 via `step_ingest` → `WaitSource` → mailbox →
`Waker::wake()`. The timer reaches it via `record_timer_preemption` → the thread's
own `UserspaceRunWait` waker. Either way, the parked thread becomes runnable and the
loop polls it.

## Summary

- A device IRQ runs a registered handler that returns `IrqHandled::{Done, Wake,
  NotMine}`; `Wake` → `TrapAction::Reschedule`.
- IRQ context can't take epoch guards, so handlers do **Phase A** (drain FIFO into a
  plain ring, request `Wake`) and the reactor does **Phase B**
  (`drain_uart_rx_pending` → `step_ingest`) in process context, which fires the
  `WaitSource` and wakes the parked reader.
- The timer IRQ fires while the thread is *in userspace*, not during a poll. It
  hands off as `TimerPreempt` (PC unchanged), the thread future `yield_now().await`s
  once and re-enters from the saved context — preempting a cooperative loop without
  interrupting any `poll`.
- Both kinds converge on the same two-stage wake: `Waker::wake()` enqueues a
  `TaskId`; `drain_wakes_for_hart` makes it runnable.

---

**Anchors:**
- `IrqHandled`: `crates/tx-hal/src/lib.rs:1180`
- IRQ table / register / install: `crates/tx-kernel/src/irq.rs:45,85,159`
- `uart_rx_irq_handler` / `drain_uart_rx_pending`: `crates/tx-kernel/src/irq.rs:182,222`
- `on_external_irq` / `on_timer_interrupt`: `crates/tx-kernel/src/trap.rs:67,52`
- `hand_off_timer_preempt` / outcome→action: `crates/tx-kernel/src/trap_handoff.rs:356,395`
- `record_timer_preemption`: `crates/tx-reactor/src/userspace.rs:300`
- wake plumbing: `crates/tx-reactor/src/waker.rs:31`, `crates/tx-reactor/src/runtime.rs:704`

**Next:** [Part 7 — Signals as Interruption](07_signals-as-interruption.md)
