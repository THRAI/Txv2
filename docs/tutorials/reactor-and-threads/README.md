# The Reactor & Userspace Threads as Futures

A tutorial series on how txKernel re-expresses the classical kernel mechanisms —
scheduling, context switch, trap handling, syscalls, blocking, interrupts, and
signals — in terms of Rust `Future`s and an executor (the *reactor*).

## Who this is for

Kernel developers who know the traditional synchronous model (trap vectors,
run-queues, `schedule()`, wait-queues, `wake_up()`) but have not yet seen how
that model maps onto `async`/`await`. A short async primer is included so you do
not need prior Rust-async fluency.

## The one-sentence thesis

**A suspended thread is a parked future, not a parked kernel stack.** A userspace
thread *is* a `Future`; the reactor is the executor that polls it; "blocking"
becomes `Poll::Pending` plus a `Waker`. Nothing busy-waits, and no kernel stack
sits idle on a wait-queue.

## How to read the code in this series

Code blocks are **pseudocode** — simplified control flow, eliding error paths and
some generics for clarity. But **data-structure types are accurate**: struct and
enum names, field names, and their types match the real source. Every chapter
ends with `file:line` anchors so you can read the real thing.

## The series

| # | File | Topic |
|---|------|-------|
| 0 | [00_async-primer.md](00_async-primer.md) | Rust futures as state machines: `poll`, `Waker`, the pull model |
| 1 | [01_reactor-as-scheduler.md](01_reactor-as-scheduler.md) | The reactor *is* a scheduler — run-queues, the poll loop, preemption |
| 2 | [02_thread-as-a-future.md](02_thread-as-a-future.md) | `run_thread`: a userspace thread opened up as a state machine |
| 3 | [03_traps-and-handoff.md](03_traps-and-handoff.md) | Traps: where synchronous hardware meets the async kernel |
| 4 | [04_the-suspension-point.md](04_the-suspension-point.md) | `UserspaceRunWait`: the single await that links trap shell and future |
| 5 | [05_blocking-syscalls-as-futures.md](05_blocking-syscalls-as-futures.md) | `read`, `nanosleep`, `futex`: blocking without parking a stack |
| 6 | [06_interrupts-and-wakes.md](06_interrupts-and-wakes.md) | IRQs and timers: who calls `wake()` |
| 7 | [07_signals-as-interruption.md](07_signals-as-interruption.md) | Signals interrupting parked waits; EINTR; AST delivery |
| 8 | [08_end-to-end.md](08_end-to-end.md) | Capstone: `read()` on an empty pipe, traced through every layer |

A single-file consolidated **[TECHNICAL_REPORT.md](TECHNICAL_REPORT.md)** covers the
same material in formal report form (abstract, numbered sections, design discussion,
type reference, and a consolidated source-anchor index) for readers who prefer one
long document over the chaptered series.

## The mapping table

Every chapter returns to this. The whole series is an expansion of it.

| Traditional kernel | txKernel future model | Anchor |
|---|---|---|
| Thread with a kernel stack | A `Future` (`run_thread`) in a boxed task | `thread_future.rs:256` |
| Scheduler run-queue + `schedule()` | Reactor poll loop + 4 priority run-queues | `runtime.rs:786`, `scheduler.rs:357` |
| Context switch (save/restore regs) | `poll()` returns; the future's state machine *is* the saved context | `runtime.rs:849` |
| Trap entry (asm vector) | Still asm — saves regs into a trap frame | `boards/.../trap.rs:103` |
| Handler runs syscall to completion | Trap shell *captures* + *resolves a wait*, then longjmps out | `trap_handoff.rs:202` |
| Sleep on a wait-queue | `Poll::Pending` + register a `Waker` on a `WaitSource`/mailbox | `userspace.rs:490`, `wait_source.rs` |
| `wake_up()` on the wait-queue | `Waker::wake()` → task re-queued for poll | `waker.rs:31` |
| `iret`/`sret` back to user | `enter_userspace_with_context()` (divergent) | `thread_future.rs` |
| Timer IRQ → preempt → reschedule | Timer trap → `TimerPreempt` → `yield_now().await` | `trap_handoff.rs:356` |

## What is genuinely identical to a normal kernel

So you do not over-attribute novelty: the asm trap vector, the trap frame layout,
the cause-classification, and even a fast-path set of synchronous syscalls are
exactly what you would find in any kernel. The novelty is concentrated at **one
seam** — the trap shell resolves a wait and longjmps back into the executor
instead of running the handler to completion. Chapter 3 is where that seam lives.
