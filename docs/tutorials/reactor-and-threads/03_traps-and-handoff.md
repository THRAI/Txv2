# Part 3 — Traps and the Handoff

> **Series:** [The Reactor & Userspace Threads as Futures](README.md)
> **Prev:** [Part 2 — A Thread as a Future](02_thread-as-a-future.md) · **Next:** [Part 4 — The Suspension Point](04_the-suspension-point.md)

This is the crux of the series. A hardware trap is unavoidably **synchronous**: the
CPU jumps to a fixed vector with no future, no `Context`, no `Waker`. Yet our thread
is a future that wants to `.await` the trap. This chapter shows the seam that joins
the two worlds — the **trap handoff** — and shows that almost everything *around*
that seam is identical to a textbook kernel.

## What is exactly like every other kernel

Be clear about what is *not* novel, so you can spot what is.

### The asm trap vector

```
; boards/tx-hal-riscv64-qemu-virt/src/trap.rs:103  (tx_rv64_qemu_minimal_trap_vector)
swap sscratch <-> sp          ; switch to the per-CPU trap stack
save x1..x31 into trap frame  ; the integer registers
read  scause, sepc, stval, sstatus  ; the CSRs
(save f0..f31 + fcsr if FP dirty)
call tx_rv64_qemu_kernel_trap_entry(&mut frame)
```

The frame it builds is the ordinary RISC-V trap frame:

```rust
// boards/tx-hal-riscv64-qemu-virt/src/trap.rs:530
struct Rv64TrapFrame {
    x:       [u64; 32],   // x0..x31
    scause:  u64,         // why we trapped
    sepc:    u64,         // where (PC at trap)
    stval:   u64,         // fault address / aux value
    sstatus: u64,
    f:       [u64; 32],   // FP regs
    fcsr:    u64,
}
```

### Cause classification

```rust
// boards/tx-hal-riscv64-qemu-virt/src/trap.rs:927  (classify_rv64_trap)
match scause {
    // synchronous
    8  => Syscall,            // ECALL from user
    12 => InstructionPageFault,
    13 => LoadPageFault,
    15 => StorePageFault,
    2  => IllegalInstruction,
    // interrupts (high bit set)
    INT|5 => TimerInterrupt,
    INT|9 => ExternalInterrupt,
    INT|1 => SoftwareInterrupt, // IPI
    _  => /* ... */
}
```

None of this is special. A traditional kernel has the same vector, the same frame,
the same decode. The difference is entirely in **what the handler decides to do**.

## The trap shell: decide, don't execute

The classified trap reaches a platform-independent dispatcher implementing
`KernelTrapSink`. Its handlers return a `TrapAction` — they decide a *policy*, they
do not run the syscall to completion:

```rust
// crates/tx-hal/src/trap.rs:304
enum TrapAction { Resume, Reschedule, DeliverSignal, Terminate }

// crates/tx-hal/src/trap.rs:311
trait KernelTrapSink<P> {
    fn on_page_fault(view: TrapFrameMut, fault: FaultInfo) -> TrapAction;
    fn on_syscall(view: TrapFrameMut) -> TrapAction;
    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut) -> TrapAction;
    fn on_external_irq(...) -> TrapAction;
    // ...
}
```

The four actions map to four fates for the trapped thread:

| `TrapAction` | Meaning |
|---|---|
| `Resume` | Return straight to userspace via `sret` (handled inline, fast path) |
| `Reschedule` | Longjmp back into the reactor; the thread future will service the trap |
| `DeliverSignal` | Return to user but into a signal handler frame (Part 7) |
| `Terminate` | Fatal — no valid thread context to hand off to |

## `on_syscall`: capture and resolve, then leave

Here is the actual decision for a syscall (`trap.rs:35`), pseudocoded:

```rust
fn on_syscall(view: TrapFrameMut) -> TrapAction {
    let req = translate_syscall(&view);     // a7 -> nr, a0..a5 -> args

    // Fast path: a tiny allow-list handled synchronously, right here.
    if let Some(action) = try_direct_trap_syscall(&mut view, &req) {
        return action;                       // usually TrapAction::Resume
    }

    // Slow path: hand the syscall off to the thread future.
    let hart = current_cpu_id();
    let outcome = hand_off_syscall(hart, &view, req);
    outcome_to_trap_action(&outcome)         // Resolved -> Reschedule
}
```

`translate_syscall` builds the request type you saw in Part 2:

```rust
// crates/tx-reactor/src/userspace.rs:37
struct SyscallRequest { nr: u64, args: [u64; 6] }
```

### The handoff itself

`hand_off_syscall` is the seam. It runs in the synchronous trap context but does
only three things — none of which is "execute the syscall":

```rust
// crates/tx-kernel/src/trap_handoff.rs:202
fn hand_off_syscall(hart, view, req) -> HandoffOutcome {
    // 1. Find the thread running on this hart (the per-hart slot from Part 2).
    let payload = current_payload_for_hart(hart)?; // None -> NoActivePayload
    let active  = payload.active_userspace_request()?; // the wait token from Phase 1

    // 2. Snapshot the user registers into the payload, and advance PC past ecall.
    let mut ctx = view.capture_user_context();
    ctx.pc = ctx.pc + RV64_ECALL_INSN_BYTES;   // 4: don't re-run the ecall
    payload.store_saved_user_context(Some(ctx));

    // 3. Resolve the in-flight userspace-run wait with the trap info.
    let slot = payload.userspace_slot().clone();
    slot.complete_interesting_trap(active, UserspaceTrapInfo::Syscall(req))?;
    HandoffOutcome::Resolved
}
```

```rust
// crates/tx-kernel/src/trap_handoff.rs:145
enum HandoffOutcome {
    Resolved,          // wait resolved -> caller returns TrapAction::Reschedule
    NoActivePayload,   // trap from kernel code, or slot momentarily empty -> Terminate
    NoActiveRequest,   // payload present but no in-flight wait -> Terminate
    SlotError(_),
}
```

Step 2 is where "the registers" land in the `ThreadPayload` (Part 2). Step 3 calls
`complete_interesting_trap`, which flips the `UserspaceRunWait` to *resolved* and
fires its `Waker` — the mechanics are [Part 4](04_the-suspension-point.md). The key
fact: **the syscall has not run yet.** All that happened is the trap was *recorded*
as the resolution of a wait the thread future is sitting on.

### Why advance PC here but not on a page fault

Note `ctx.pc += 4` happens for syscalls only. A syscall should resume *after* the
`ecall`. A page fault must *re-execute* the faulting instruction once the mapping
exists, so `hand_off_user_pf` (`trap_handoff.rs:279`) snapshots the context with
PC **unchanged**. Same seam, one detail different — and the reason the syscall arm
and the page-fault arm of `run_thread` (Part 2) write back differently.

## The longjmp: getting back into the reactor

`on_syscall` returned `Reschedule`. But the CPU is on the trap stack, deep inside
the divergent `enter_userspace_with_context` call the thread future made in Part 2,
Phase 2. We need to get back to the reactor's poll loop. That is a longjmp:

```
; boards/tx-hal-riscv64-qemu-virt/src/trap.rs  apply_trap_action(Reschedule):
re-prime sscratch = trap_stack_top
call tx_rv64_resume_kernel_after_reschedule(&KernelResumeCtx)   ; trap.rs:485
```

```
; tx_rv64_resume_kernel_after_reschedule:  restore (sp, ra, s0..s11) and `ret`
```

`KernelResumeCtx` is the kernel-side callee-saved register snapshot taken right
before the thread future dove into userspace. Restoring `sp`/`ra`/`s0..s11` and
`ret`-ing **unwinds the divergent call** — control pops back out of
`enter_userspace_with_context` as if it had returned normally, landing the thread
future right after its Phase 2 dive, at the `entry_wait.await` of Phase 3.

This is the futures equivalent of a context switch *into the scheduler*, except the
"scheduler" is just the reactor poll loop continuing. Compare the two worlds:

| Traditional | txKernel |
|---|---|
| Trap → handler runs syscall on the kernel stack | Trap → shell records the trap, longjmps to reactor |
| Handler blocks → `schedule()` switches stacks | Future `.await`s → reactor polls another task |
| Handler finishes → `sret` to user | Future loops → `enter_userspace_with_context` → `sret` |

## The fast path: when the traditional model wins

Not every syscall deserves a round-trip through the reactor. `getpid`,
`clock_gettime`, `rt_sigprocmask` and a few others are pure, never block, and are
hot. `try_direct_trap_syscall` (`trap.rs:122`) handles those *synchronously in the
trap shell*, writes the result straight into the trap frame, and returns
`TrapAction::Resume` — a plain `sret`, no handoff, no poll. Preconditions: the
syscall is on the allow-list, no signal is pending, no interrupt summary is set.

This is worth dwelling on: the future model is not dogma. The kernel keeps a
classic synchronous path for the cases where it is strictly faster, and reserves
the handoff for syscalls that *might* block. Which ones might block is the whole
point of [Part 5](05_blocking-syscalls-as-futures.md).

## Plan B: the two-site writeback discipline

One subtlety threads through Parts 2–3: **where does the syscall's return value get
written into `a0`?** The naive answer — "in the trap handler" — is wrong here, and
the reason is instructive.

The discipline (called "Plan B" in the source) splits it into two sites:

1. **Trap shell (capture site).** `hand_off_syscall` snapshots the user context and
   resolves the wait. It does **not** write the return value. At this moment the
   value does not even exist — the syscall has not run.

2. **Userspace-entry shim (the only writeback site).**
   `prepare_userspace_entry_payload_into` (`execution.rs:775`), called in Phase 2
   of the *next* loop iteration, drains `pending_syscall_return` and folds it into
   `a0` of the fresh context just before `enter_userspace_with_context`:

```rust
// execution.rs:775 (pseudocode)
fn prepare_userspace_entry_payload_into(payload, out: &mut UserTrapContext) {
    let mut ctx = payload.saved_user_context().expect("captured at trap");
    if let Some(result) = payload.drain_pending_syscall_return() {
        ctx.regs[A0] = match result {       // A0 = regs[10]
            Ok(v)     => v as usize,
            Err(errno)=> (-(errno as i64)) as usize,  // -errno convention
        };
    }
    payload.set_active_userspace_request(None);
    *out = ctx;
}
```

Why bother with two sites?

- Between capture (1) and writeback (2), the thread future runs `dispatch(req).await`
  — which may suspend and resume many times (Part 5). The trap frame from the
  original trap is long gone; the value must live in `pending_syscall_return` and be
  applied to a *fresh* context at re-entry.
- It keeps the result-write off the trap-restore path, so nothing tramples it.
- It composes cleanly with signal delivery, which may overwrite `a0` with a handler
  frame instead (Part 7) — the page-fault and `ExecCommitted` arms skip the write
  entirely, and that is expressible only because the write is one explicit step.

## Summary

- The asm vector, trap frame, and cause classification are exactly a normal
  kernel's. The novelty is one decision: the handler *records* the trap instead of
  *executing* it.
- `on_syscall` either takes a synchronous fast path (`Resume`) or hands off:
  `hand_off_syscall` snapshots user registers into the `ThreadPayload`, advances PC
  past `ecall`, and **resolves the thread future's in-flight wait**, returning
  `Reschedule`.
- `Reschedule` longjmps via `tx_rv64_resume_kernel_after_reschedule`, unwinding the
  divergent `enter_userspace_with_context` so the thread future resumes at
  `entry_wait.await`.
- A page fault uses the same seam but leaves PC unchanged (re-execute the faulting
  instruction).
- "Plan B" puts the `a0` writeback at exactly one site — the userspace-entry merge
  — because the value is produced asynchronously, after the trap frame is gone.

The one thing we deferred: *how does resolving a wait turn into the future being
re-polled?* That is the suspension point itself.

---

**Anchors:**
- asm vector / frame / classify: `boards/tx-hal-riscv64-qemu-virt/src/trap.rs:103,530,927`
- reschedule longjmp: `boards/tx-hal-riscv64-qemu-virt/src/trap.rs:485`
- `TrapAction` / `KernelTrapSink`: `crates/tx-hal/src/trap.rs:304,311`
- `on_syscall` / fast path: `crates/tx-kernel/src/trap.rs:35,122`
- `hand_off_syscall` / `hand_off_user_pf` / `HandoffOutcome`: `crates/tx-kernel/src/trap_handoff.rs:202,279,145`
- `SyscallRequest`: `crates/tx-reactor/src/userspace.rs:37`
- Plan B writeback: `crates/tx-subsystems/src/thread_runtime/execution.rs:775`

**Next:** [Part 4 — The Suspension Point](04_the-suspension-point.md)
