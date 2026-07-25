# Chapter 12 — Signals across the ABI boundary

Signals are where the kernel synthesizes a function call *into* userspace. When a
process gets `SIGSEGV` or `SIGCHLD`, the kernel must arrange for the user's signal
handler to run — with the right arguments, on a frame the handler can return
through, restoring the interrupted context afterward. This is delicate ABI work,
and it's the consumer of the synchronous user-write primitive from Chapter 11.

`SignalFrameIf` is the HAL trait that owns the architecture-specific frame layout.
It is a **supertrait of `TrapIf`** (`crates/tx-hal/src/lib.rs:1007`) because
building a signal frame needs trap-context primitives.

## What signal delivery requires

To deliver a signal, the kernel must:

1. pick a user stack location below the current `sp` and lay out a **signal
   frame** there — saved registers, saved signal mask, a `siginfo`, a `ucontext`,
   and a **return trampoline**;
2. point the resumed user context at the handler: `pc = handler`, `sp =
   frame_addr`, `ra = trampoline`, and arguments `a0 = signo`, `a1 = &siginfo`,
   `a2 = &ucontext` (the RV64 / Linux `SA_SIGINFO` convention);
3. when the handler returns, it `ret`s into the trampoline, which executes
   `rt_sigreturn(2)`; the kernel then restores the saved context and mask so the
   interrupted code resumes as if nothing happened.

All of this is per-architecture. The trait surface (`crates/tx-hal/src/lib.rs:1007`):

```rust
pub trait SignalFrameIf: TrapIf {
    fn write_signal_frame(tf: TrapFrameMut<'_>, setup: SignalFrameWrite)
        -> Result<SignalFramePlacement, FaultInfo>;
    fn read_signal_frame(user_sp: UserPtr<u8>) -> Result<SavedSignalFrame, FaultInfo>;
    fn signal_frame_size() -> usize;
    fn decode_signal_frame_bytes(user_sp: UserPtr<u8>, bytes: &[u8])
        -> Result<SavedSignalFrame, FaultInfo>;
    fn restore_signal_frame(tf: TrapFrameMut<'_>, frame: &SavedSignalFrame);
    fn rewind_syscall_pc(tf: TrapFrameMut<'_>);
    fn prepare_signal_frame(ctx: &UserTrapContext, setup: &SignalFrameWrite)
        -> Result<(UserTrapContext, SignalFrameBytes), FaultInfo>;
}
```

The default impls all return an `ENOSYS`-shaped `FaultInfo`, so a board that
hasn't implemented signals links and fails cleanly; the RV64 board implements all
of them (`boards/tx-hal-riscv64-qemu-virt/src/signal_frame.rs:235`).

## The RV64 frame and its trampoline

The board's frame is `Rv64SignalFrame` (`signal_frame.rs`), a `#[repr(C)]` `Pod`
struct with a magic/version header, the signal number, saved FP context, saved
status, a `siginfo`, a Linux-compatible `ucontext` (`LinuxUcontextRv64` with
`uc_mcontext` holding `gregs`/`fpregs`), and last, the trampoline:

```rust
const RV64_RT_SIGRETURN_SYSCALL: u32 = 139;     // __NR_rt_sigreturn
// trampoline: two instructions, encoded as raw u32:
//   addi a7, zero, 139   ; ecall
const RV64_SIGRETURN_TRAMPOLINE: [u32; 2] = [ rv64_addi(17, 0, 139), rv64_ecall() ];
```

The trampoline lives *on the user stack as part of the frame*. The handler's `ra`
is set to its address; when the handler returns, the CPU executes those two
instructions — `a7 = 139; ecall` — invoking `rt_sigreturn`. The kernel catches
that syscall and restores the saved context.

## `prepare_signal_frame` — building the frame without a live frame

The interesting method is `prepare_signal_frame` (`signal_frame.rs:335`). Recall
from Chapter 10 that the thread runtime's AST checkpoint runs *before*
`enter_userspace_with_context` — before there is a live `TrapFrameMut`. So signal
setup can't use the writeback vtable; it has to build everything from a plain
`UserTrapContext` snapshot:

```rust
fn prepare_signal_frame(ctx: &UserTrapContext, setup: &SignalFrameWrite)
    -> Result<(UserTrapContext, SignalFrameBytes), FaultInfo>
{
    let frame_size = size_of::<Rv64SignalFrame>();
    let user_sp = UserPtr::<u8>::new(ctx.regs[2]);               // sp = x2
    let unrounded = user_sp.addr().checked_sub(frame_size)?;     // grow stack down
    let frame_addr = align_down(unrounded, RV64_SIGFRAME_ALIGN);

    let frame = Rv64SignalFrame {
        magic: …, version: …, frame_size: frame_size as u32,
        sig_no: setup.sig_no,
        saved_fp: ctx.fp, saved_status: ctx.status as u64,
        siginfo: setup.siginfo,
        ucontext: LinuxUcontextRv64::from_user_context(ctx, setup.old_mask.bits),
        trampoline: RV64_SIGRETURN_TRAMPOLINE,
    };
    let frame_bytes = /* &frame as bytes */;

    // Build the handler-entry context:
    let mut handler_ctx = *ctx;
    handler_ctx.pc       = setup.handler_pc.addr();
    handler_ctx.regs[2]  = frame_addr;                                   // sp
    handler_ctx.regs[1]  = frame_addr + offset_of!(Rv64SignalFrame, trampoline);  // ra
    handler_ctx.regs[10] = setup.sig_no as usize;                        // a0 = signo
    handler_ctx.regs[11] = frame_addr + offset_of!(Rv64SignalFrame, siginfo);   // a1
    handler_ctx.regs[12] = frame_addr + offset_of!(Rv64SignalFrame, ucontext);  // a2
    Ok((handler_ctx, SignalFrameBytes::from_slice(frame_bytes)))
}
```

It returns *two* things: the modified `UserTrapContext` (which becomes the
thread's `saved_user_context`, so the next `enter_userspace_with_context` lands in
the handler) and the raw `SignalFrameBytes` to write to the user stack. The caller
writes those bytes via the synchronous Chapter 11 primitive, then stores the
context. Note all the handler register wiring uses `offset_of!` against the frame
struct — the kernel computes the user addresses of `siginfo`, `ucontext`, and the
trampoline relative to `frame_addr`, so the layout is single-sourced.

The return trip: when the handler `ret`s to the trampoline, `rt_sigreturn` lands
in `on_syscall`, which routes to `restore_signal_frame` /
`decode_signal_frame_bytes` (`signal_frame.rs:303`) to rebuild the saved context
and mask, restoring the interrupted user state.

## The 2048-byte buffer: an ABI war story

`SignalFrameBytes` (`crates/tx-hal/src/lib.rs:973`) is the fixed-size carrier that
moves frame bytes from `prepare_signal_frame` to the user-stack write:

```rust
pub struct SignalFrameBytes { pub data: [u8; Self::CAPACITY], pub len: usize }
impl SignalFrameBytes { pub const CAPACITY: usize = 2048; }
```

The doc comment on this type (lines 958–972) is one of the most instructive bug
post-mortems in the tree, and it's worth reading as a cautionary tale. The buffer
*used* to be 512 bytes. The musl-compatible RV64 `ucontext_t` includes the full
floating-point union, which pushed the real frame — including the on-stack
trampoline at `offset_of!(SignalFrame, trampoline) = 712` — past 512 bytes. The
`from_slice` copy silently truncated, dropping the trailing fields, most
catastrophically the trampoline. So a signal handler would return through
`ra = frame_addr + 712`, the CPU would fetch *uninitialized user stack bytes*
instead of the `a7=139; ecall` trampoline, and execution would wander off into
garbage. The fix was to size the carrier (2048) safely above the board frame, and
`from_slice` now `assert!`s on overflow instead of truncating:

```rust
pub fn from_slice(bytes: &[u8]) -> Self {
    let len = bytes.len();
    assert!(len <= Self::CAPACITY, "signal frame layout ({len} bytes) exceeds buffer ({})", Self::CAPACITY);
    let mut data = [0u8; Self::CAPACITY];
    data[..len].copy_from_slice(bytes);
    Self { data, len }
}
```

The lesson for HAL design: fixed-size carriers that cross the ABI boundary must be
sized for the *worst-case real layout* and must fail loud on overflow, never
truncate. A silent truncation of an ABI structure is nearly impossible to debug
from the symptom (a handler returning into noise).

## `rewind_syscall_pc` and `SA_RESTART`

`rewind_syscall_pc` (`signal_frame.rs:331`) backs the PC up over the `ecall` so a
syscall interrupted by a signal can be *restarted* after the handler returns
(`SA_RESTART` semantics). The default impl (`crates/tx-hal/src/lib.rs:1048`) is
`tf.rewind_pc(4)` — rewind one RV64 instruction. It's small, but it's the kind of
arch detail (instruction width) that has to live behind the HAL so the signal
subsystem doesn't hard-code "4".

## `UserFpContext` and the FP-save contract

The saved context includes floating-point state via `UserFpContext`
(`crates/tx-hal/src/trap.rs:131`), a `repr(C)` `[u64; 32]` + `fcsr` + flags, with
`FLAG_VALID`/`FLAG_DIRTY` bits. The doc's `FpSimdIf` section
(`txdoc:HAL-FPSIMDIF-OPTIONAL-WHERE-FPU-STATE-LIVES-1`) requires that a board with
FP support capture FP state into the signal frame and restore it on `sigreturn`,
so handler delivery doesn't clobber the interrupted code's float registers. The
RV64 frame carries `saved_fp: ctx.fp` precisely for this — even ahead of a full
lazy-FPU ownership policy, signal save/restore round-trips the FP context.

## What you should take away

- `SignalFrameIf` (a `TrapIf` supertrait) owns the arch-specific signal-frame
  layout: frame struct, on-stack `rt_sigreturn` trampoline, handler register
  wiring, and `sigreturn` restore.
- `prepare_signal_frame` builds the frame and the handler-entry context from a
  `UserTrapContext` snapshot (no live `TrapFrameMut`), returning bytes the caller
  writes via the Chapter 11 synchronous primitive — fitting the
  AST-checkpoint-before-entry order.
- Fixed-size ABI carriers (`SignalFrameBytes`, 2048 B) must be sized for the
  worst-case real layout and assert on overflow; the 512→2048 truncation bug is
  the reason.
- FP state round-trips through the frame (`UserFpContext`) so handlers don't
  clobber interrupted float registers.

This closes Part IV. Part V covers the remaining device-facing axes:
[Chapter 13 — Interrupts: explicit registration, not link-time magic](ch13-irq-explicit-registration.md).
</content>
