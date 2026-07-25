# Part 8 — End to End: `read()` on an Empty Pipe

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 7 — Signals as Interruption](07_signals-as-interruption.md)

This capstone traces one operation — a reader blocking on an empty pipe, then woken
by a writer — through every layer the series built. Two threads are involved: **R**
(the reader) and **W** (the writer), each a `run_thread` future, both ordinary tasks
in the reactor's table. Follow the `a0` value and the task states and the whole
architecture snaps together.

## Cast

- **Reader task R** — `PerHartSlotted<run_thread>` for thread R, with payload `P_R`.
- **Writer task W** — same, payload `P_W`.
- **Pipe** — its read end has a `WaitSource` `S` (readiness) and a byte buffer.
- **Reactor** — one hart here, for clarity.

## Act 1 — R issues the syscall

```
[R in userspace]  read(fd, buf, 256)  ->  ecall

asm vector (boards/.../trap.rs:103)
  swap sscratch, save x1..x31 + CSRs into Rv64TrapFrame, classify -> Syscall(8)

on_syscall (trap.rs:35)
  req = translate_syscall: nr=READ, args=[fd, buf, 256, ...]   (SyscallRequest)
  try_direct_trap_syscall -> None   (read is not on the fast-path allow-list)
  hand_off_syscall(hart, view, req)                            (trap_handoff.rs:202)
    payload P_R = current_payload_for_hart(hart)               (the per-hart slot)
    ctx = view.capture_user_context();  ctx.pc += 4            (skip the ecall)
    P_R.store_saved_user_context(ctx)                          (R's registers now live here)
    P_R.userspace_slot.complete_interesting_trap(tok, Syscall(req))
        -> ActivePhase::Resolved(Syscall(req));  waker.wake()
  -> HandoffOutcome::Resolved -> TrapAction::Reschedule

apply_trap_action(Reschedule)
  tx_rv64_resume_kernel_after_reschedule(&KernelResumeCtx)     (trap.rs:485)
  -> restore sp/ra/s0..s11, ret
  -> unwinds the divergent enter_userspace_with_context call
  -> control pops back into run_thread(R) right after its Phase 2 dive
```

The syscall has **not run**. R's trap became *the resolution of a wait* (Part 3+4).

## Act 2 — R's future dispatches, then parks

```
run_thread(R) Phase 3:  entry_wait.await
  UserspaceRunWait::poll -> slot is Resolved(Syscall(req)) -> Poll::Ready(Syscall(req))   (userspace.rs:490)

run_thread(R) Phase 4:  syscall arm
  result = dispatch::<P>(req, &ctx_R).await                    (the async syscall)
    sys_read: pipe buffer is EMPTY
      register ActiveWait{gen, source=S, interest=READABLE} on WaitSource S   (wait_source.rs:118)
      mailbox_R.register_waker(cx.waker())                                    (mailbox.rs:352)
      no data, no signal -> Poll::Pending
  -> dispatch(...).await is Pending
  -> run_thread(R) returns Pending
  -> PerHartSlotted clears the per-hart slot, returns Pending

reactor (runtime.rs:786) finish_polled_pending(R) -> Parked
```

State of the world now:

| | |
|---|---|
| Task R | `Parked` — on no run-queue |
| R's registers | in `P_R.saved_user_context` (heap) |
| R's resume point | the `dispatch(...).await` inside `run_thread(R)`'s boxed state machine |
| R's wait entry | one `Subscriber` row in `WaitSource S`, holding `Weak<mailbox_R>` |
| Kernel stacks held by R | **zero** |

The hart moves on. `pick_next_or_steal_local` returns task **W**. No stack was
parked — this is the Part 5 payoff, concretely.

## Act 3 — W writes and fires the readiness source

W runs its own round-trip (Acts 1–2 shape) and reaches its `write` dispatch:

```
run_thread(W) Phase 4:  sys_write
  copy bytes into the pipe buffer
  pipe now READABLE -> fire WaitSource S
     for each Subscriber whose interest overlaps READABLE and gen matches:
        post MailboxEvent::SourceFired{gen, S, READABLE} to mailbox_R
        mailbox_R's Waker -> TaskWakeState::wake():                 (waker.rs:31)
              wake_requested = true;  global_wake_queue.push(R)
  sys_write -> SyscallResult::Return(n_written)
  P_W.store_pending_syscall_return(Ok(n_written))
```

W's `write` is the `wake_up()` for R's wait-queue. It enqueued R's `TaskId`; it did
not run R.

## Act 4 — the reactor re-runs R

```
reactor loop, next iteration:
  drain_wakes_for_hart(hart)                                   (runtime.rs:704)
    pop R from global_wake_queue
    R: Parked -> Runnable;  scheduler.enqueue(R)               (routed by WakeHint)

  pick_next_or_steal_local -> R
  take R's future, build waker, poll:
    run_thread(R) resumes inside dispatch(...).await:
      sys_read::poll: mailbox_R.poll() -> SourceFired{gen, S, READABLE}
        ActiveWait::matches? gen ok, source S, READABLE overlaps -> YES
        unregister Subscriber from S;  clear_waker
        copy min(256, available) bytes into user buf
        -> Poll::Ready  =>  dispatch resolves SyscallResult::Return(n_read)
  run_thread(R) Phase 4 tail:
    P_R.store_pending_syscall_return(Ok(n_read))
  loop back to Phase 1
```

R is runnable again purely because its `Waker` was called and the reactor re-polled
it. The compiler-generated state machine resumed exactly at the `.await` (Part 0).

## Act 5 — R returns to userspace with the result

```
run_thread(R) Phase 1:  start_request -> new UserspaceRunWait (next generation)
                        ast_checkpoint: no pending signal -> Continue           (Part 7)
run_thread(R) Phase 2:  prepare_userspace_entry_payload_into(P_R, &mut ctx)     (execution.rs:775)
                          ctx = P_R.saved_user_context
                          drain pending_syscall_return = Ok(n_read)
                          ctx.regs[A0] = n_read         <-- the ONLY a0 writeback (Plan B)
                        enter_userspace_with_context(&ctx, root)   (divergent)
                          -> sret: user resumes at the instruction after `ecall`,
                             with a0 = n_read
[R in userspace]  read() returns n_read.  Done.
```

The return value lived in `pending_syscall_return` across the entire park/wake/resume
cycle and was applied to a *fresh* trap frame at exactly one site — the Plan B
discipline from Part 3, now visibly necessary: the original trap frame was gone many
polls ago.

## The whole thing as one picture

```
   userspace R            trap shell (sync)         reactor (async executor)
   ───────────            ─────────────────         ────────────────────────
   read() ecall  ───────► capture regs -> P_R
                          resolve wait (Syscall)
                          wake()         ┐
                          Reschedule ────┘ longjmp ──► run_thread(R) Phase3 .await = Ready
                                                       Phase4 dispatch(read).await
                                                         pipe empty -> register on S
                                                         -> Pending  ──► R PARKED
                                                       (hart polls W instead)

   userspace W
   ───────────
   write() ecall ───────► ... -> run_thread(W) dispatch(write):
                                  buffer += bytes; fire WaitSource S
                                    -> post SourceFired to mailbox_R
                                    -> Waker(R).wake(): enqueue R
                                  return n_written

                                                       drain_wakes: R Parked->Runnable
                                                       poll R: dispatch(read) resumes
                                                         mailbox event matches -> copy bytes
                                                         -> Ready(n_read)
                                                       store pending_syscall_return
   read()=n_read ◄──────── sret (a0=n_read) ◄───────── Phase2 merge a0=n_read; enter_userspace
```

## The mapping table, now fully populated

The table from the [README](README.md), with the act that demonstrated each row:

| Traditional kernel | txKernel future model | Shown in |
|---|---|---|
| Thread with a kernel stack | `run_thread` future in a boxed task | Cast / Act 2 |
| Scheduler run-queue + `schedule()` | reactor poll loop + run-queues | Acts 2, 4 |
| Context switch (save/restore regs) | `poll()` returns; state machine *is* the context | Act 2 |
| Trap entry (asm vector) | asm vector → `Rv64TrapFrame` | Act 1 |
| Handler runs syscall to completion | trap shell captures + resolves a wait, longjmps | Act 1 |
| Sleep on a wait-queue | register `ActiveWait` on `WaitSource S`, return `Pending` | Act 2 |
| `wake_up()` | fire `S` → `SourceFired` → `Waker::wake()` | Act 3 |
| Woken thread re-checks & completes | re-poll resumes `dispatch(read).await` | Act 4 |
| `iret`/`sret` back to user | `enter_userspace_with_context` (a0 via Plan B) | Act 5 |
| Blocking costs a parked kernel stack | blocking costs one subscriber row + a boxed future | Act 2 |

## Where to go from here

- **Preemption** of a CPU-bound R: the timer-IRQ path (Part 6) injects
  `TimerPreempt` instead of `Syscall`, R `yield_now().await`s, and Act 4-style
  re-scheduling runs W — no `poll` ever interrupted.
- **Interruption** of the parked R by a signal: Part 7 — `SignalDelivered` forces a
  re-poll, R's `Interruptible` wait resolves `Interrupted`, and the `read` returns
  `EINTR` instead of bytes.
- **Page faults**: the same Act-1 handoff with `UserspaceTrapInfo::PageFault` and PC
  unchanged; Phase 4 runs `aspace.fault_script(...).await` and re-enters without an
  `a0` write (Parts 2–3).
- **SMP**: tasks R and W can sit on different harts; the global wake queue and
  per-hart run-queues (Part 1) already account for cross-hart wakes. The per-hart
  slot race noted in Part 2 is the seam that design is built around.

That is the whole model. A thread is a future, the reactor is its executor, a trap
is a wait, a wakeup is a `Waker`, and "blocking" is `Poll::Pending` — and none of it
parks a kernel stack.

---

**Anchors (consolidated):**
- asm vector / frame / resume: `boards/tx-hal-riscv64-qemu-virt/src/trap.rs:103,485,530`
- trap shell: `crates/tx-kernel/src/trap.rs:35`; handoff `crates/tx-kernel/src/trap_handoff.rs:202`
- thread future: `crates/tx-kernel/src/thread_future.rs:256`; slot `:192`
- suspension point: `crates/tx-reactor/src/userspace.rs:341,490`
- reactor loop / wakes: `crates/tx-reactor/src/runtime.rs:786,704`; waker `crates/tx-reactor/src/waker.rs:31`
- wait machinery: `crates/tx-substrate/src/wake/{wait_source.rs:83,mailbox.rs:127,258,517}`
- syscall handlers: `crates/tx-shims/src/linux_syscall/{io.rs:875,time.rs:825,vm.rs:1192}`
- Plan B writeback: `crates/tx-subsystems/src/thread_runtime/execution.rs:775`
