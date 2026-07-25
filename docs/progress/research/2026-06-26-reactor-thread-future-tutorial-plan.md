# Tutorial Plan: The Reactor & Userspace Threads as Futures

**Date:** 2026-06-26
**Status:** Plan / outline (not yet written)
**Audience:** Kernel developers who know traditional OS concepts (trap handlers,
context switch, scheduler run-queues, blocking syscalls) but are new to how
txKernel re-expresses them in Rust `async`/`Future` terms.

## Thesis

A traditional kernel handles a syscall *synchronously inside the trap handler*:
the trap entry saves registers, the handler runs to completion (blocking the CPU
on a wait-queue if it must sleep), then `iret`/`sret` returns to userspace.
txKernel keeps the *same conceptual stages* but cuts the timeline at the blocking
point: a userspace thread is a `Future`, the reactor is the executor that polls
it, and "blocking" becomes `Poll::Pending` + a `Waker`. Nothing busy-waits and no
kernel stack is parked on a wait-queue — the suspended thread is just a future
sitting in a task table.

The tutorial's job: for each classical mechanism, show the textbook version, then
the txKernel future-shaped version, grounded in real code.

## Core mental-model mapping (the table the whole tutorial builds toward)

| Traditional kernel | txKernel future model | Code anchor |
|---|---|---|
| Process/thread with a kernel stack | A `Future` (`run_thread`) in a boxed task | `thread_future.rs:256` |
| Scheduler run-queue + `schedule()` | Reactor poll loop + 4 priority run-queues | `tx-reactor/src/runtime.rs:786`, `scheduler.rs:357` |
| Context switch (save/restore regs) | `poll()` returns; future's state machine *is* the saved context | `runtime.rs:849` |
| Trap entry (asm vector) | Still asm — saves regs into a trap frame | `boards/.../trap.rs:103` |
| Trap handler runs syscall to completion | Trap shell only *captures* + *resolves a wait*, then longjmps out | `trap_handoff.rs:202` |
| Blocking on a wait-queue (sleep) | `Poll::Pending` + register `Waker` on a `WaitSource`/mailbox | `wait_source.rs`, `userspace.rs:490` |
| `wake_up()` on the wait-queue | `Waker::wake()` → task re-queued for poll | `waker.rs:31` |
| `iret`/`sret` back to user | `enter_userspace_with_context()` (divergent call) | `thread_future.rs:~590` |
| Timer interrupt → preempt → reschedule | Timer trap → `TimerPreempt` → `yield_now().await` | `trap_handoff.rs:356`, `thread_future.rs` TimerPreempt arm |

## Proposed chapter structure

### Part 0 — Prerequisites & framing (short)
- Quick refresher on Rust futures *as state machines*: `poll(&mut Context) ->
  Poll<T>`, `Waker`, "a future does nothing until polled", lazy/pull model.
- The one-sentence claim: **a suspended thread is a parked future, not a parked
  stack.** Set expectation that we'll keep returning to the mapping table.
- `no_std` reality: uses real `core::future::Future` / `core::task` — not a
  bespoke future trait. The novelty is the *executor*, not the future contract.

### Part 1 — The reactor as a scheduler
Goal: convince the reader the reactor *is* a scheduler, just pull-based.
- `Reactor` / `ReactorShared` structure; per-hart locals (`runtime.rs:182,262`).
- The four run-queues (kernel / boosted / new / preempted) and pick order
  (`scheduler.rs:357`, `pick_next_from_local` at `scheduler.rs:1271`) — compare
  to a classic MLFQ / priority run-queue.
- The poll loop walkthrough (`runtime.rs:786`): drain wakes → pick task → take
  future → build `Waker` → `future.poll(cx)` → handle `Ready`/`Pending`. Map each
  step to `schedule()` / `switch_to()` / `need_resched`.
- Tasks vs threads: a `Task` holds `Pin<Box<dyn Future>>` (`task.rs:113,21`);
  a userspace *thread* is one specific future. Tasks ≠ OS threads; they
  multiplex on one hart.
- Time-slicing & preemption points (`SliceConfig`, `PreemptionPoint`,
  `preempt.rs:92`) — cooperative vs preemptive, where the "tick" maps in.

### Part 2 — What a userspace thread future actually is
Goal: open up `run_thread` and show the state machine.
- `run_thread<P>(thread, payload)` (`thread_future.rs:256`) — the `loop {}` is the
  thread's whole life; each iteration = one userspace round-trip.
- `PerHartSlotted` wrapper (`thread_future.rs:192-244`): why every poll installs
  the payload into a per-hart slot and clears it on exit — this is the bridge the
  (synchronous) trap shell uses to find "who is running here." Include the
  slot-cleared-on-Pending race note (`thread_future.rs:184-191`) as a teaching
  moment about the async/sync seam.
- The five phases inside the loop, annotated:
  1. open the entry wait + AST checkpoint (signals/stop-state) — `:287`
  2. merge saved context + pending syscall return; dive to userspace — `:550`
  3. `entry_wait.await` — the suspension point — `:624`
  4. dispatch the resolved trap (syscall / page-fault / timer / fatal) — `:639`
  5. loop back
- Where the "context" lives: `ThreadPayload` (`thread_runtime/structure.rs:140`),
  `saved_user_context`, `pending_syscall_return`. Contrast with a traditional
  kernel stack + trap frame. Emphasize: across `.await`, the registers live in the
  payload (epoch-managed `Cap`), and the future's own locals live in the boxed
  state machine — *that* is the "saved context."

### Part 3 — Traps: where synchronous hardware meets the async kernel
Goal: the crux. Hardware traps are unavoidably synchronous; show the handoff.
- Low-level RV64 vector (`boards/.../trap.rs:103`): sscratch swap, save 32 GPRs +
  CSRs into `Rv64TrapFrame` (`:530`), `classify_rv64_trap` (`:927`). This part is
  identical to any textbook kernel — call that out.
- The trap shell decision (`KernelTrapSink` / `KernelTrapDispatcher`,
  `trap.rs:10`): `on_syscall`, `on_page_fault`, `on_timer_interrupt`,
  `on_external_irq` → return a `TrapAction` (`Resume` / `Reschedule` / ...).
- **The key idea — "trap handoff":** the trap shell *cannot* `.await` (it's
  running on the trap stack with no future context). So it does the minimum:
  - `hand_off_syscall` (`trap_handoff.rs:202`): capture user context into the
    payload, bump PC past `ecall`, then `complete_interesting_trap(req,
    Syscall(req))` resolving the in-flight wait, return `TrapAction::Reschedule`.
  - `Reschedule` → `tx_rv64_resume_kernel_after_reschedule` (`trap.rs:485`)
    longjmps back into the kernel poll context, unwinding the divergent
    `enter_userspace_with_context` call so `run_thread` resumes at
    `entry_wait.await`.
- Fast path / direct syscalls (`try_direct_trap_syscall`, `trap.rs:122`): a small
  allow-list (getpid, clock_gettime, rt_sigprocmask, ...) handled *synchronously
  in the trap shell* with `TrapAction::Resume` — the traditional model, kept as an
  optimization. Good contrast: not everything needs to round-trip the reactor.
- Plan B writeback discipline (two-site): trap shell snapshots but does NOT write
  the return value; the only writeback site is
  `prepare_userspace_entry_payload_into` (`thread_runtime/execution.rs:775`) which
  folds `pending_syscall_return` into `a0` at re-entry. Explain *why*: lets async
  dispatch run between handoff and re-entry without the trap-restore path
  trampling the result, and lets signal-frame delivery override `a0` cleanly.

### Part 4 — The suspension point: `UserspaceRunWait`
Goal: zoom into the single await that links trap shell ↔ future.
- `UserspaceRunSlot` / `UserspaceRunWait` (`userspace.rs:190-201`),
  `UserspaceTrapInfo` enum (`:83`).
- The future impl (`userspace.rs:490`): first poll sees phase `Pending/Running`,
  stashes `cx.waker()`, returns `Poll::Pending`. `complete_interesting_trap`
  (`:341`) flips phase to `Resolved` and calls the stored waker. Next poll →
  `Poll::Ready(trap)`.
- This is the textbook **wait-queue + wake_up**, reduced to one slot and one
  waker. Diagram: userspace → trap → shell resolves slot → reschedule longjmp →
  reactor re-polls future → `await` returns the trap info.
- The host-test asymmetry (great for understanding): on real hardware the wait is
  *already resolved* by the time the future re-polls (longjmp happens inside the
  trap); on the host test platform there is no real trap, so the test driver calls
  `complete_interesting_trap` and the first `await` genuinely returns `Pending`.
  Walk the test (`init/tests.rs:884` and `thread_future/tests.rs`) to make the
  state machine observable.

### Part 5 — Blocking syscalls become futures (the payoff)
Goal: show a syscall that *sleeps* and how no CPU/stack is consumed.
- Once `run_thread` has the `Syscall(req)`, dispatch is `async fn`:
  `Box::pin(tx_shims::linux_syscall::dispatch::<P>(req, &ctx)).await`
  (`thread_future.rs` syscall arm).
- Concrete examples, textbook-vs-future side by side:
  - `sys_nanosleep` (`tx-shims/.../time.rs:825`) → `sleep_until_deadline().await`,
    woken by the timer wheel. Compare to `schedule_timeout()`.
  - `sys_futex` wait (`tx-shims/.../vm.rs:1192`) → parks on a futex-bucket
    `WaitSource`, woken by `step_futex_wake`. Compare to a kernel futex hash-bucket
    wait-queue.
  - A pipe/tty `read` on empty buffer (`io.rs`, `wait_for_tty_readable`) → parks
    on a readiness `WaitSource`, woken by the device IRQ path.
- The wait machinery: `WaitSource` / `WaitToken` / `TaskMailbox` /
  `RegisteredWaitFuture` (`wait_source.rs`); poll = register waker on mailbox,
  subscribe with a generation, re-check condition, else `Pending`
  (`userspace.rs`/`wait_source.rs:197-246`). Map: mailbox = the thread's
  wait-entry; generation = ABA/lost-wake guard; `WaitSource::notify` = `wake_up`.
- Punchline: the suspended `read` is now a future in the task table. The hart is
  free to poll other threads. No kernel stack is pinned. "Blocking" cost = one
  parked future + one waker registration.

### Part 6 — Interrupts and wakes closing the loop
Goal: who calls `wake()`?
- IRQ path (`irq.rs:45,182`): `uart_rx_irq_handler` drains the FIFO into a ring
  buffer (can't touch epoch guards in IRQ context), returns `IrqHandled::Wake` →
  `TrapAction::Reschedule`. Non-IRQ context later drains (`drain_uart_rx_pending`,
  `irq.rs:222`) and pushes to the TTY discipline, which `notify`s the WaitSource,
  which wakes the parked reader's task.
- Timer interrupt → `hand_off_timer_preempt` (`trap_handoff.rs:356`) →
  `UserspaceTrapInfo::TimerPreempt` → `yield_now().await` in the thread future:
  voluntary reschedule that preserves saved context and re-enters from it. This is
  the future-model's "preemption."
- The wake plumbing end to end: `TaskWakeState::wake` (`waker.rs:31`) pushes the
  `TaskId` onto the global wake queue; `drain_wakes_for_hart` (`runtime.rs:704`)
  moves `Parked → Runnable`; scheduler re-queues. Tie back to Part 1's loop.

### Part 7 — Signals as wait interruption
Goal: how EINTR / signal delivery interacts with parked futures.
- Signal posts to the thread's mailbox as `MailboxEvent::SignalDelivered`
  (`signal/mod.rs`); a parked interruptible wait sees it on poll and returns
  `WaitOutcome::Interrupted` → syscall returns `EINTR` (test:
  `v3_signal_interrupt_wake.rs`). Map to traditional `signal_pending()` checks in
  interruptible sleep.
- AST checkpoint at loop top (Part 2 phase 1): where pending signals are turned
  into handler frames before re-entering userspace. Compare to "deliver signals on
  return-to-user" in a traditional kernel.

### Part 8 — Full end-to-end walkthrough (capstone)
Single narrative tracing `read()` on an empty pipe from `ecall` to resumption,
hitting every layer: asm vector → classify → trap shell `hand_off_syscall` →
reschedule longjmp → `run_thread` `entry_wait.await` returns `Syscall` → async
dispatch parks on WaitSource (`Pending`) → reactor polls other threads → writer's
`write` + device IRQ → `notify`/`wake` → reader task re-queued → dispatch resumes,
returns n → stash in `pending_syscall_return` → loop → `prepare_userspace_entry`
folds n into `a0` → `enter_userspace_with_context` → `sret`. Reuse the mapping
table from the intro as the closing summary, now fully populated.

## Pedagogical devices to use throughout
- **Per-chapter "Traditional vs txKernel" sidebar** so the analogy stays explicit.
- **One running example** (pipe/tty `read`) reused in Parts 3–8 so the reader
  isn't re-orienting each chapter.
- **Two diagrams minimum:** (a) the reactor poll loop as a scheduler; (b) the
  trap-handoff longjmp ↔ `entry_wait.await` round-trip (the single most important
  picture in the doc).
- **Call out what is genuinely identical to a normal kernel** (asm vector, trap
  frame, classify, fast-path syscalls) so readers don't over-attribute novelty.
  The novelty is concentrated at exactly one seam: the trap shell resolves a wait
  + longjmps instead of running the handler to completion.

## Key source anchors (verified during research)
- Reactor loop: `crates/tx-reactor/src/runtime.rs:786` (poll), `:704` (drain wakes)
- Scheduler queues: `crates/tx-reactor/src/scheduler.rs:357`, pick `:1271`
- Waker: `crates/tx-reactor/src/waker.rs:31,49`
- Task table / future type: `crates/tx-reactor/src/task.rs:113,21,166`
- Thread future: `crates/tx-kernel/src/thread_future.rs:256` (`run_thread`),
  `:192` (`PerHartSlotted`)
- Userspace wait: `crates/tx-reactor/src/userspace.rs:83,190,341,490`
- Trap shell: `crates/tx-kernel/src/trap.rs:10,35,122`
- Trap handoff: `crates/tx-kernel/src/trap_handoff.rs:202,279,356`
- Plan B writeback: `crates/tx-subsystems/src/thread_runtime/execution.rs:775`
- Asm vector / frame: `boards/tx-hal-riscv64-qemu-virt/src/trap.rs:103,485,530,927`
- IRQ: `crates/tx-kernel/src/irq.rs:45,182,222`
- Async syscalls: `crates/tx-shims/src/linux_syscall/{time.rs:825,vm.rs:1192,io.rs}`
- Wait sources: `crates/tx-subsystems/src/wait_source.rs`
- Boot submission: `crates/tx-kernel/src/init/exec.rs:647-658`
- Tests to mine for observable behavior: `crates/tx-kernel/src/thread_future/tests.rs`,
  `crates/tx-kernel/src/init/tests.rs:884,1014`

## Open questions for the author before writing
1. Target length / format — single long doc, or a numbered series under
   `docs/design/02_execution/`? (The reactor/thread material is execution-layer.)
2. Audience assumption: can we assume Rust async fluency, or include Part 0?
3. Should code excerpts be verbatim (kept in sync via `txdoc:` tags) or
   simplified/pseudocode for readability? Recommend simplified-in-prose +
   file:line pointers to the real thing.
