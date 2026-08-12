# Part 2 — A Thread as a Future

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 1 — The Reactor as a Scheduler](01_reactor-as-scheduler.md) · **Next:** [Part 3 — Traps and the Handoff](03_traps-and-handoff.md)

In Part 1 the reactor polled opaque tasks. Now we open one up. A userspace thread
is the future returned by `run_thread`. Its `async` body is an infinite loop, and
each iteration is exactly one userspace round-trip: enter userspace, take a trap,
service it, repeat. The thread *ends* when its body returns — on `exit`/`exit_group`
or an unrecoverable fault.

## A traditional thread vs. this future

A classical kernel thread spends its life bouncing between two modes:

```
user mode:    runs instructions until a trap (syscall / fault / IRQ)
kernel mode:  trap handler services it, then returns to user
```

The state that survives a trip through the kernel lives in the thread's **kernel
stack** and its **trap frame**. `run_thread` keeps the same two-mode life, but the
surviving state lives in two places you can name and inspect:

- the **future's own state machine** (its locals across `.await`), and
- a **`ThreadPayload`** (the registers and the pending syscall result).

## The top-level future

```rust
// crates/tx-kernel/src/thread_future.rs:256
async fn run_thread<P: TxPlatform>(
    thread:  Cap<ThreadIdentity>,        // who this thread is (identity cap)
    payload: PayloadCap<ThreadPayload>,  // its mutable execution state
) {
    loop {
        // (1) open the entry wait; run the AST checkpoint (signals, stop-state)
        // (2) merge saved regs + pending syscall result; enter userspace
        // (3) .await the trap that the trap shell will resolve
        // (4) dispatch the trap: syscall / page-fault / timer / fatal
        // (5) loop
    }
    // falls out of the loop -> thread terminated
}
```

Two capabilities are held across every `.await` here, deliberately:

- `Cap<ThreadIdentity>` — the thread's identity, an epoch-managed capability that
  is safe to hold across yields (`txdoc:THREAD-4-2-OWNERSHIP`).
- `PayloadCap<ThreadPayload>` — the execution state. *This* is where the registers
  live between polls.

## Where the "saved context" lives: `ThreadPayload`

This is the type that replaces "the kernel stack + trap frame" as the home of
suspended thread state. The fields below are accurate (eliding the signal-internal
ones until Part 7):

```rust
// crates/tx-subsystems/src/thread_runtime/structure.rs:140
struct ThreadPayload {
    task: SpinMutex<Option<TaskKey>>,           // which reactor task drives this

    userspace_slot: UserspaceRunSlot,           // the wait the trap shell resolves
    active_request: SpinMutex<Option<UserspaceRunRequest>>,  // in-flight wait token

    saved_user_context:   SpinMutex<Option<UserTrapContext>>,// the user registers
    pending_syscall_return: SpinMutex<Option<Result<i64, i32>>>, // result -> a0

    mailbox: SpinMutex<Option<Weak<TaskMailbox>>>,  // wake channel (signals/readiness)
    stopped: AtomicBool,                            // SIGSTOP parked?

    signal_mask: AtomicU64,                         // (Part 7)
    thread_pending: PendingSignalQueue,             // (Part 7)
    signal_summary: AtomicU8,                        // (Part 7)
    // saved_signal_context / saved_signal_mask / alt_stack (Part 7)
    // clear_child_tid / robust_list_* (futex teardown)
    proc_sleeping: AtomicBool,                       // procfs "is sleeping" hint
}
```

The register set itself is the HAL type:

```rust
// crates/tx-hal/src/trap.rs:169
struct UserTrapContext {
    regs: [usize; 32],   // x0..x31 (RV64); a0 = regs[10]
    pc:   usize,
    status: usize,       // sstatus
    fp:   UserFpContext, // f0..f31 + fcsr
}
```

So when a thread is suspended mid-syscall, "its registers" are
`payload.saved_user_context` — a plain struct in a heap-managed payload, not bytes
on a parked stack. That is the concrete payoff of Part 0's "context is a value."

## The per-hart slot wrapper: `PerHartSlotted`

There is a problem to solve before the loop can work. The trap shell (Part 3) runs
in a *synchronous* context — a hardware trap, no future, no `cx`. When a trap
fires, it must answer: *which thread is running on this hart right now?* It cannot
ask the reactor; it just needs a pointer.

`PerHartSlotted` is the bridge. It wraps the `run_thread` future and, on every
poll, installs this thread's identity and payload into a **per-hart slot**, then
clears them when the poll returns:

```rust
// crates/tx-kernel/src/thread_future.rs:192
struct PerHartSlotted<P, F> {
    thread:  Cap<ThreadIdentity>,
    payload: PayloadCap<ThreadPayload>,
    inner:   F,                          // the run_thread future
}

impl<P, F: Future> Future for PerHartSlotted<P, F> {
    type Output = F::Output;
    fn poll(self, cx) -> Poll<F::Output> {
        let hart = current_cpu_id();

        // Publish "this thread is live on this hart" for the trap shell.
        set_current_thread_identity(hart, self.thread.clone());
        set_current_thread_payload(hart, self.payload.clone());
        if let Some(mailbox) = current_task_mailbox(hart) {
            self.payload.bind_mailbox(downgrade(mailbox));  // wire wakes -> this task
        }

        let out = self.inner.poll(cx);   // <- run_thread runs here

        clear_current_thread_payload(hart);
        clear_current_thread_identity(hart);
        out
    }
}
```

The slot is cleared on **both** `Ready` and `Pending`. There is an intentional
race: between the wrapper's poll exit and the reactor's next decision, the slot is
briefly `None`. A trap arriving in that window finds no payload and falls back to a
terminate policy. The comment at `thread_future.rs:184` calls this out as
deliberate — leaving the slot set across yields would deny the slot to other
futures under SMP. Note this now; it is exactly the async/sync seam we examine in
Part 3.

Also note `bind_mailbox`: the wrapper connects the thread's `ThreadPayload.mailbox`
to the reactor *task's* mailbox. That is how a signal or a readiness event later
reaches the right `Waker` ([Parts 6](06_interrupts-and-wakes.md)–[7](07_signals-as-interruption.md)).

## The loop body, phase by phase

Now the five phases inside `run_thread`'s loop. Pseudocode condensed from
`thread_future.rs:263`–`~968`.

### Phase 1 — open the entry wait, run the AST checkpoint

```rust
// Open the userspace-run wait BEFORE entering userspace. This is the
// token the trap shell fills in when the next trap fires.
let entry_wait  = payload.userspace_slot().start_request().expect("slot free");
let entry_token = entry_wait.request();
payload.set_active_userspace_request(Some(entry_token));

// Stop-state: a SIGSTOP'd thread must not enter userspace.
while payload.is_stopped() { /* park until SIGCONT clears it */ }

// AST checkpoint: turn pending signals into handler frames, etc. (Part 7)
ast_checkpoint(&payload);
```

`start_request` hands back a `UserspaceRunWait` future and a generation-checked
`UserspaceRunRequest` token. Stashing the token in the payload is what lets the
(synchronous) trap shell later resolve *this specific* wait. The wait object itself
is the subject of [Part 4](04_the-suspension-point.md).

### Phase 2 — merge state and dive into userspace

```rust
let mut ctx = UserTrapContext::empty();
prepare_userspace_entry_payload_into(&payload, &mut ctx);  // execution.rs:775
// ^ folds saved_user_context + pending_syscall_return (-> a0) into `ctx`

payload.set_active_userspace_request(Some(entry_token));   // re-arm after prepare
<P as TrapIf>::enter_userspace_with_context(&ctx, root);   // DIVERGENT
```

`enter_userspace_with_context` is the `sret`/`iret` analogue. On real hardware it
**does not return** — control leaves the kernel and resumes userspace at `ctx.pc`
with `ctx.regs`. The kernel only regains control via a trap, which longjmps back
*through* this call site (Part 3). The merge step is the *only* place a syscall
result is written into `a0` — the "Plan B" discipline detailed in Part 3.

### Phase 3 — await the trap

```rust
let trap: UserspaceTrapInfo = entry_wait.await;   // thread_future.rs:624
```

This is the thread's single suspension point per round-trip. On real hardware the
trap shell has *already* resolved the wait by the time we are re-polled, so this
`.await` returns `Ready` immediately. On the host test platform there is no real
trap, so it genuinely returns `Pending` and the test driver resolves it later —
which is what makes the state machine observable in tests (Part 4).

### Phase 4 — dispatch the resolved trap

```rust
enum UserspaceTrapInfo {        // crates/tx-reactor/src/userspace.rs:83
    Syscall(SyscallRequest),
    PageFault(PageFaultInfo),
    TimerPreempt,
    Fatal(FatalTrapInfo),
}

match trap {
    UserspaceTrapInfo::Syscall(req) => {
        // async dispatch — may .await on a blocking operation (Part 5)
        let result: SyscallResult = dispatch::<P>(req, &ctx).await;
        match result {
            NoReturn          => return,                       // exit/exit_group
            ExecCommitted     => { /* don't write a0; new image */ }
            other             => payload.store_pending_syscall_return(other.into()),
        }
    }
    UserspaceTrapInfo::PageFault(info) => {
        match aspace.fault_script(info.into()).await {         // async VM fault
            Ok(())  => { /* mapping published; re-run faulting insn, no a0 write */ }
            Err(_)  => { deliver_synchronous_fault(SIGSEGV); return; }
        }
    }
    UserspaceTrapInfo::TimerPreempt => {
        yield_now().await;   // voluntarily give the hart up; re-enter from saved ctx
    }
    UserspaceTrapInfo::Fatal(_) => {
        deliver_synchronous_fault(SIGSEGV); return;
    }
}
// fall through to top of loop -> Phase 1 again
```

The relevant result type is accurate here:

```rust
// crates/tx-shims/src/linux_syscall/result.rs:22
enum SyscallResult {
    Return(i64),                                  // -> a0 = value
    CloneReturn { value: i64, child_submit: SubmitChildThreadStatus },
    Error(i32),                                   // -> a0 = -errno
    NoReturn,                                     // exit/exit_group: future returns
    ExecCommitted,                                // execve replaced the image
    // ...
}
```

Notice the asymmetry that the writeback discipline (Part 3) exists to manage:
the **syscall** arm produces a result that must land in `a0` on the *next* entry;
the **page-fault** arm produces *no* `a0` write because the faulting instruction is
simply re-executed; `TimerPreempt` produces no architectural change at all, it just
yields. One loop, several very different "returns to user."

## How the thread future is born (boot)

At boot the init thread's future is wrapped in `PerHartSlotted` and submitted to
the reactor exactly like any other task (`init/exec.rs:647`):

```rust
let (task_key, _) = reactor.submit_task_with_meta_from_hart(
    PerHartSlotted::<P, _>::new(
        thread.clone(),
        payload.clone(),
        run_thread::<P>(thread, payload),    // the thread's whole life
    ),
    userspace_thread_sched_meta(),
    current_hart,
    &mut signal,
);
register_thread_reactor_task(tid, task_key);
P::enable_timer_wakeups();
```

From the reactor's perspective (Part 1) this is just another `Pin<Box<dyn
Future>>` in the task table. Later threads (from `clone`) are submitted the same
way through the `reactor_submit` seam (`init/reactor_submit.rs:305`), which is how
`tx-shims` — a library that cannot depend on the kernel's reactor — gets a child
thread onto the run-queue ([Part 5](05_blocking-syscalls-as-futures.md) touches the seam).

## Summary

- A userspace thread *is* the `run_thread` future: an infinite `async` loop where
  each iteration is one userspace round-trip.
- Suspended thread state lives in a **`ThreadPayload`** (registers in
  `saved_user_context`, the result destined for `a0` in `pending_syscall_return`)
  plus the future's own state machine — not on a kernel stack.
- `PerHartSlotted` publishes the running thread's payload into a **per-hart slot**
  on each poll so the synchronous trap shell can find it, and binds the thread's
  mailbox to the reactor task's `Waker`.
- The loop's four trap arms (syscall / page-fault / timer / fatal) all re-enter
  userspace differently — the reason the syscall-result writeback is carefully
  staged (Part 3).
- A thread is submitted to the reactor as an ordinary boxed future at boot, and
  child threads via the `reactor_submit` seam.

---

**Anchors:**
- `run_thread`: `crates/tx-kernel/src/thread_future.rs:256`; loop top `:263`; entry await `:624`
- `PerHartSlotted`: `crates/tx-kernel/src/thread_future.rs:192`; slot-race note `:184`
- `ThreadPayload`: `crates/tx-subsystems/src/thread_runtime/structure.rs:140`
- `UserTrapContext`: `crates/tx-hal/src/trap.rs:169`
- `UserspaceTrapInfo`: `crates/tx-reactor/src/userspace.rs:83`
- `SyscallResult`: `crates/tx-shims/src/linux_syscall/result.rs:22`
- entry merge `prepare_userspace_entry_payload_into`: `crates/tx-subsystems/src/thread_runtime/execution.rs:775`
- boot submit: `crates/tx-kernel/src/init/exec.rs:647`; child submit: `crates/tx-kernel/src/init/reactor_submit.rs:305`

**Next:** [Part 3 — Traps and the Handoff](03_traps-and-handoff.md)
