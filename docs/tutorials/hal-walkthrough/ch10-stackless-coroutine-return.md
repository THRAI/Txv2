# Chapter 10 — Why divergent-into-userspace returns: the stackless-coroutine trick

This is the most original chapter in the tutorial, and the hardest. It answers a
question that sounds impossible:

> The kernel runs threads as **stackless `async` futures** polled on the current
> hart's stack (Chapter 4 — "tasks remain stackless futures"). Entering userspace
> is `sret`, which *diverges* — it does not return. How can a future `poll` enter
> userspace, have the user trap back in, run a syscall, and *resume the same
> future* — when the thing it called never returns?

The answer is a controlled longjmp: `enter_userspace_with_context` is written so
that it appears to *return* to its caller when the trap shell decides
`TrapAction::Reschedule`. The divergent `sret` is brought back through normal
function-return semantics. This is what makes a stackless-coroutine thread model
composable with hardware userspace entry.

## The shape of the problem

A kernel thread future, when polled, eventually wants to run userspace. In a
stackful design it would just `sret` and let the next trap re-enter the kernel on
a fresh kernel stack, find the thread, and continue. But txKernel threads have no
private kernel stack — they're futures on the hart's stack. If `sret` simply
diverged, the future's `poll` frame would be stranded: the next trap would build a
new call chain that has no way back into that `poll`.

So `enter_userspace_with_context` must do something stackful-looking on a
stackless substrate: stash *enough kernel CPU state to return to this exact call
site*, then `sret`. When the trap fires and the sink says "reschedule," the shell
restores that stashed state and `ret`s — landing back inside
`enter_userspace_with_context`, which returns to the future's `poll`, which awaits
the now-resolved userspace-run and dispatches the trap it just took.

## `KernelResumeCtx` — the stash

The state needed to return to a call site on RISC-V is the stack pointer, the
return address, and the callee-saved registers. The board defines exactly that,
per hart (`boards/tx-hal-riscv64-qemu-virt/src/lib.rs:143`):

```rust
#[repr(C, align(8))]
pub struct KernelResumeCtx {
    pub sp: usize,      // offset 0
    pub ra: usize,      // offset 8
    pub s: [usize; 12], // offset 16..112   (s0..s11)
}
```

It is stored one-per-hart in a `PerHartCell` array (`lib.rs:180`), and its field
offsets are asserted at compile time (`lib.rs:157`) because the assembly
references them by literal byte offset. There is no `ra`/`sp` for the *whole*
kernel here — just the snapshot taken at the moment of the most recent userspace
entry. The doc references this as the "Plan B writeback" two-site discipline
(`txdoc:THREAD-5-4-THE-TWO-SITE-DISCIPLINE`).

## Site 1: `enter_userspace_with_context`

This is the `TrapIf` method the board overrides (`board trap.rs:813`). Read it as
"materialize a fresh frame, switch the address space, stash resume state, sret":

```rust
fn enter_userspace_with_context(ctx: &UserTrapContext, root: &PmapRoot) {
    let mut frame = Rv64TrapFrame { /* zeroed */ };
    frame.restore_user_context(ctx);      // copies regs/pc/sstatus; clears SPP, sets SPIE
    <Platform as PmapIf>::activate_user_pmap(root);   // satp → process root (Chapter 8)

    unsafe {
        let resume_ctx = current_kernel_resume_ctx_ptr();
        let stack_top  = trap_stack_top_for_cpu(<Platform as SmpIf>::current_cpu_id());
        tx_rv64_enter_userspace_save_resume(resume_ctx, &frame, stack_top);
    }
}
```

Note the signature: `fn enter_userspace_with_context(...)` returns `()`. The doc
explains the apparent contradiction (`txdoc:HAL-TRAPIF-…` and the trait comment at
`crates/tx-hal/src/trap.rs:342`): despite the `()` return type, the body "runs to
a trap and back." On `Reschedule` it returns normally to its caller; on
`Resume`/`DeliverSignal` it does *not* return through here at all (the trap-vector
epilogue `sret`s straight back to userspace). The `()` type is what lets it
compose with the future-driven runtime — the caller just sees a function that
returns when there's a kernel-side decision to make.

The asm helper `tx_rv64_enter_userspace_save_resume(a0=resume_ctx, a1=&frame,
a2=stack_top)` (`board trap.rs:376`):

1. stores the *current* `sp`, `ra`, and `s0..s11` into `*resume_ctx` — this is the
   bookmark back into `enter_userspace_with_context`'s caller;
2. primes `sscratch` with `stack_top` (so the next user trap lands on the trap
   stack, Chapter 9);
3. loads the user register file from `frame`, sets `sepc`, and `sret`s into user
   mode.

After the `sret`, this hart is running userspace. The kernel call stack below
`enter_userspace_with_context` is *intact and parked* — its continuation lives in
`KernelResumeCtx`.

## The user traps back in

The user executes until it faults, makes a syscall, or the timer fires. The
Chapter 9 vector saves a fresh `Rv64TrapFrame` on the trap stack, classifies, and
calls the sink. Consider a syscall: `KernelTrapDispatcher::on_syscall` runs. Per
the **Plan B writeback discipline**, it does *not* write the result into this
fresh frame. Instead `trap_handoff::hand_off_syscall` (`trap_handoff.rs`):

- calls `view.capture_user_context()` to snapshot the user registers into the
  thread's payload as `saved_user_context`;
- bumps the saved PC past the 4-byte `ecall` so the eventual re-entry resumes at
  the next instruction;
- resolves the thread's *userspace-run wait* with the syscall request;
- returns an outcome that maps to `TrapAction::Reschedule`.

Why snapshot into the payload instead of editing the live frame? Because the live
frame is on the trap stack and is about to be *discarded* — the longjmp doesn't go
through the trap epilogue that would `sret` from it. The durable record is
`saved_user_context` in the thread payload. When the future is polled again and
decides to re-enter userspace, `enter_userspace_with_context` builds a brand-new
frame from that saved context. The "two sites" are: (1) `enter_userspace_with_context`
materializes the frame, and (2) the userspace-entry shim writes the pending
syscall return into that fresh frame just before `sret`. The trap handler is never
a writeback site.

## Site 2: applying the action — the longjmp

Back in the shell, `apply_trap_action` (`board trap.rs:1018`) consumes the
`TrapAction`:

```rust
fn apply_trap_action(frame: &Rv64TrapFrame, action: TrapAction) {
    let from_user = frame.previous_mode() == TrapPreviousMode::User;
    match action {
        TrapAction::Resume | TrapAction::DeliverSignal => {}   // fall to epilogue → sret
        TrapAction::Reschedule => {
            if from_user {
                let stack_top = trap_stack_top_for_cpu(current_cpu_id());
                crate::clear_current_asid_residency();
                unsafe {
                    core::arch::asm!("csrw sscratch, {top}", top = in(reg) stack_top);
                    let ctx = current_kernel_resume_ctx_ptr();
                    tx_rv64_resume_kernel_after_reschedule(ctx);   // diverges
                }
            }
            // from-kernel Reschedule: fall through to epilogue (no longjmp)
        }
        TrapAction::Terminate => tx_rv64_qemu_trap_panic(frame),
    }
}
```

`tx_rv64_resume_kernel_after_reschedule(a0=resume_ctx)` (`board trap.rs:485`) is
the longjmp: it loads `sp`, `ra`, and `s0..s11` *back* from `*resume_ctx` and
`ret`s. Control resumes inside `enter_userspace_with_context` (Site 1) as if the
call to `tx_rv64_enter_userspace_save_resume` had simply returned — which unwinds
into the future's `run_thread` body, which awaits the just-resolved userspace-run
wait and dispatches the syscall it captured.

Two correctness details the code is careful about:

- **Re-prime `sscratch` before the longjmp.** The longjmp skips the trap epilogue
  (which normally restores `sscratch`), so the code explicitly writes `stack_top`
  back into `sscratch`. Forget this and the *next* user trap would land on the
  user stack — silent corruption. The comment at `board trap.rs:1028` spells this
  out.
- **Only longjmp for from-*user* traps.** `KernelResumeCtx` is written only by
  `enter_userspace_with_context`. A from-*kernel* trap that asks for `Reschedule`
  (e.g. an IRQ that fires while the BSP loop is in `wfi` and wakes another task)
  must *not* longjmp — the resume context holds a stale snapshot from the last
  userspace entry, and jumping to it would time-warp into a dead frame. For those,
  `apply_trap_action` falls through to the epilogue and the woken task is picked up
  on the next reactor poll. This is exactly the subtle bug the comment at
  `board trap.rs:1039` was written to prevent.

## The four `TrapAction`s, by what they do to control flow

Putting Chapter 9's enum together with this chapter's mechanics:

| Action | From user | From kernel |
|---|---|---|
| `Resume` | trap epilogue pop + `sret` back to userspace | epilogue `sret` back to S-mode PC |
| `DeliverSignal` | resume-shaped (`sret`); the AST checkpoint redirects entry to the handler (Chapter 12) | — |
| `Reschedule` | **longjmp** via `KernelResumeCtx` back into the future's `poll` | fall through to epilogue; woken task runs on next poll |
| `Terminate` | panic / fault path | panic — kernel bug stops the world |

`Reschedule`-from-user is the linchpin. It is the single mechanism that lets a
syscall, a page fault, a timer preemption, or a wake-causing IRQ all hand control
back to the async runtime through a normal return, so the thread future can make
its scheduling decision in ordinary Rust instead of in trap context.

## Why this is worth it

A stackful kernel gets this for free — each thread has a kernel stack to return
onto. txKernel deliberately gave that up to make threads cheap, poll-able futures
with no per-thread kernel stack, which is what lets it run a fully `async` kernel
on a single hart stack. The price is this one piece of machinery: a per-hart
resume context and a pair of asm helpers that bracket the `sret`/trap round-trip
and turn it into a function call. Once it exists, *every* userspace interaction —
syscalls, faults, signals, preemption — composes with `.await` for free. The
complexity is concentrated in two assembly helpers and one `apply_trap_action`
match arm; everything above it is ordinary async Rust.

## What you should take away

- Entering userspace is divergent (`sret`), but `enter_userspace_with_context` is
  written to *return* to its caller on `Reschedule`, by bracketing the round-trip
  with a per-hart `KernelResumeCtx` stash/restore — a controlled longjmp.
- Plan B writeback: the trap handler never edits the live (soon-discarded) frame;
  it snapshots `saved_user_context` into the thread payload, and a fresh frame is
  rebuilt on the next entry.
- `apply_trap_action` longjmps only for from-user `Reschedule`; from-kernel
  `Reschedule` falls through to the epilogue to avoid resuming a stale context.
- This is the mechanism that makes a stackless-coroutine thread model composable
  with hardware userspace entry — the whole async kernel rests on it.

Next: [Chapter 11 — User-memory access: eager walk and the surviving fixup
exception](ch11-user-access-fixup.md).
</content>
