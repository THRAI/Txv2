# Chapter 9 — Trap entry and the shell-to-sink contract

`TrapIf` is the second load-bearing HAL trait. Where `PmapIf` owns "how memory is
mapped," `TrapIf` owns "what happens when the hardware interrupts the kernel" —
trap entry, register save, cause classification, and the contract by which the
architecture-specific *shell* calls into the architecture-neutral *sink*. It is
the seam through which every syscall, page fault, timer tick, and IRQ flows.

This is also the chapter where the doc and the code diverge most structurally, so
we'll build the picture from the code and call the doc's intent out as we go.

## The traditional picture

A classic trap handler is a chunk of assembly (the vector) that saves registers
into a frame, then a C function with a giant `switch (scause)` that decides what
to do, then assembly that restores and returns. The arch-specific and policy parts
are tangled: the `switch` knows both the RISC-V cause numbers *and* the kernel's
page-fault logic.

txKernel splits this into a **shell** and a **sink**:

- The **shell** is in the board crate. It owns the vector assembly, the frame
  layout, register save/restore, the `gp`/`tp` swap, cause classification, and
  applying the result. It speaks Sv39 and `scause`.
- The **sink** is in `tx-kernel`. It is `KernelTrapSink<P>`, a trait with one
  named method per trap class, implemented once, generically over `P`. It speaks
  page faults and syscalls, never `scause`.

The shell classifies and calls the right sink method; the sink returns a
`TrapAction`; the shell applies it. Neither knows the other's vocabulary.

## The concrete trap frame

The board defines a concrete `#[repr(C)]` trap frame, `Rv64TrapFrame`
(`boards/tx-hal-riscv64-qemu-virt/src/trap.rs:~532`), with a fixed 552-byte
layout whose offsets the assembly hard-codes:

```rust
#[repr(C)]
pub struct Rv64TrapFrame {
    pub x: [usize; 32],   // x0..x31     offset 0..255  (x[2]=sp, x[10]=a0)
    pub scause: usize,    //             offset 256
    pub sepc: usize,      //             offset 264
    pub stval: usize,     //             offset 272
    pub sstatus: usize,   //             offset 280
    pub f: [u64; 32],     // f0..f31     offset 288..543
    pub fcsr: u32,        //             offset 544
    pub _pad_fp: u32,     //             offset 548
}
```

This is exported publicly from the board crate (`lib.rs:20`). That is the first
divergence:

> **Divergence — concrete `Rv64TrapFrame`, no `type RawTrapFrame`.** The doc's
> `TrapIf` (`txdoc:HAL-TRAPIF-…-TRAIT-SURFACE-1`) has `type RawTrapFrame;` (an
> associated type, opaque to non-platform code) plus `classify` / `view` /
> `view_mut` methods on the trait. The shipped `TrapIf` (`crates/tx-hal/src/trap.rs:325`)
> has none of those — it carries only `install_minimal_trap_vector`,
> `install_kernel_trap_vector`, `install_user_trap_vector`, `classify_trap`,
> `snapshot_trap`, and `enter_userspace_with_context`. The raw frame is a *concrete
> board type*, and the read/write surface is the standalone `TrapFrameView` /
> `TrapFrameMut` (below), not trait-associated. The proof-object *idea* survived;
> the associated-type packaging did not.

## The vector assembly, in movements

The vector `tx_rv64_qemu_minimal_trap_vector` (board `trap.rs`) is one
`global_asm!` block. Conceptually:

1. **User/kernel stack discrimination.** On entry `sp` is the trapped context's
   `sp` (user `sp` for a user trap), and `sscratch` holds this hart's trap-stack
   top (primed once at boot). The vector checks the sign of `sp` (kernel VAs are
   high/"negative", user VAs are low/"positive") and, for a user trap, executes
   `csrrw sp, sscratch, sp` — atomically switching to the trap stack while saving
   the user `sp` into `sscratch`. A kernel-origin trap (already on a kernel stack)
   skips the swap. This is the RV64 `sscratch` discipline the doc requires
   (`txdoc:HAL-PER-ARCH-…-RISC-V-64-1`).
2. **Save the integer file** x0..x31 into the frame (with the user `sp` recovered
   from `sscratch`), then `scause`/`sepc`/`stval`/`sstatus`.
3. **Lazy FP save.** Read the FS field of `sstatus` (bits 14:13); only if FS ≥
   Clean does it save f0..f31 and `fcsr`. Integer-only threads pay nothing.
4. **Reinstall kernel `gp`/`tp`.** A user trap arrives with user `gp`/`tp`; the
   vector loads the kernel `gp` (`__global_pointer$`) and recovers the kernel `tp`
   from the trap-stack top, so Rust global access and per-CPU reads work.
5. **Call the Rust trap entry** with `a0` = `&mut Rv64TrapFrame`.
6. **Epilogue.** On return it restores `sstatus`/`sepc`, conditionally restores FP,
   re-stashes the trapped `sp` into `sscratch`, pops the integer file, and
   `csrrw sp, sscratch, sp` swaps back before `sret`.

## The dispatch function and the `#[no_mangle]` symbol

The Rust trap entry calls `dispatch_trap_frame::<K>` (board `trap.rs:~848`):

```rust
pub fn dispatch_trap_frame<K>(frame: &mut Rv64TrapFrame) -> TrapAction
where
    K: KernelTrapSink<Platform>,
{
    let class = classify_rv64_trap(frame.scause);
    let from_user = frame.previous_mode() == TrapPreviousMode::User;
    match class {
        TrapClass::PageFault { write, instruction } => {
            if !from_user {
                if let Some(recovery_pc) = user_access::fixup_lookup(frame.sepc) {
                    frame.sepc = recovery_pc;        // kernel-mode user-access fixup
                    frame.x[X_A0] = frame.stval;
                    return TrapAction::Resume;
                }
            }
            K::on_page_fault(frame.view_mut(), FaultInfo { /* … */ })
        }
        TrapClass::Syscall => K::on_syscall(frame.view_mut()),
        TrapClass::TimerInterrupt => {
            let _irq = crate::enter_irq_context();
            K::on_timer_interrupt(<Platform as SmpIf>::current_cpu_id(), frame.view_mut())
        }
        TrapClass::ExternalInterrupt => { /* enter_irq_context; */ K::on_external_irq(cpu) }
        TrapClass::InterprocessorInterrupt => { /* … */ K::on_ipi(cpu) }
        // illegal / breakpoint / alignment / unknown → on_illegal_or_sync_fault
        // (with a lazy-FP-enable shortcut for the first FP instruction)
    }
}
```

`classify_rv64_trap` is the `scause` decoder: it maps the interrupt bit + code to
`TrapClass` (timer=5, external=9, IPI=1; ecall=8 → `Syscall`; 12/13/15 →
instruction/load/store `PageFault`; 2 → `IllegalInstruction`; etc.).

Now the divergence you were promised in Chapter 1. The vector doesn't call
`dispatch_trap_frame` directly with a kernel type — `tx-hal` can't name
`tx-kernel`'s sink, and the *board crate* can't either (it's below `tx-kernel`).
So the **binary** exports the bridge symbol (`boards/tx-kernel-riscv64-qemu-virt/src/main.rs:22`):

```rust
#[no_mangle]
pub extern "C" fn tx_kernel_riscv64_qemu_trap_dispatch(
    frame: *mut tx_hal_riscv64_qemu_virt::Rv64TrapFrame,
) -> tx_hal::TrapAction {
    let Some(frame) = (unsafe { frame.as_mut() }) else { return tx_hal::TrapAction::Terminate; };
    tx_hal_riscv64_qemu_virt::dispatch_trap_frame::<tx_kernel::trap::KernelTrapDispatcher>(frame)
}
```

The vector assembly calls `tx_kernel_riscv64_qemu_trap_dispatch` by name; that
function is the one place that can name *both* the concrete `Rv64TrapFrame` and
the kernel's `KernelTrapDispatcher`, because the binary sits at the top of the
diamond.

> **Divergence — the bridge lives in the binary, not the platform crate.** The
> doc's shell-to-sink sketch (`txdoc:HAL-TRAPIF-…-THE-SHELL-TO-SINK-CONTRACT-1`)
> puts a generic `rust_trap_entry<P, K>` *in the platform crate*. Reality routes
> through `dispatch_trap_frame::<K>` (in the platform crate, generic over the sink
> only) plus a `#[no_mangle]` bridge *in the binary*. This is the one sanctioned
> breach of board-binary minimalism (Chapter 1): the trap symbol must see both
> sides of the diamond, and the binary is the only crate that does.

## `TrapFrameView` and `TrapFrameMut`: the read/write surface

Sink methods must read and write trap-frame fields *by name* — `pc`, `sp`,
`syscall_number`, `fault_address` — without ever seeing `sepc` or `sstatus`. Two
types provide that.

`TrapFrameView` (`crates/tx-hal/src/trap.rs:84`) is a plain `Copy` struct of
named fields — `pc`, `sp`, `syscall_number`, `syscall_args: [u64; 6]`,
`fault_address`, `previous_mode`, `user_tls_register`, … The read path is fully
concrete: no dispatch.

`TrapFrameMut` (`crates/tx-hal/src/trap.rs:199`) is the write handle, and it uses
a **vtable**:

```rust
pub struct TrapFrameMut<'a> {
    view: TrapFrameView,
    raw: NonNull<()>,                       // type-erased &mut Rv64TrapFrame
    vtable: &'static TrapFrameMutVtable,
    _frame: PhantomData<&'a mut ()>,
}

pub struct TrapFrameMutVtable {
    pub read_view: fn(NonNull<()>) -> TrapFrameView,
    pub set_pc: fn(NonNull<()>, VirtAddr),
    pub set_sp: fn(NonNull<()>, VirtAddr),
    pub set_syscall_return: fn(NonNull<()>, i64),
    pub set_syscall_error: fn(NonNull<()>, i32),
    pub set_user_tls_register: fn(NonNull<()>, u64),
    pub capture_user_context: fn(NonNull<()>) -> UserTrapContext,
    pub restore_user_context: fn(NonNull<()>, &UserTrapContext),
    pub set_signal_handler_regs: fn(NonNull<()>, SignalHandlerRegs),
    pub rewind_pc: fn(NonNull<()>, usize),
}
```

The board constructs one `static RV64_TRAP_FRAME_MUT_VTABLE` whose functions know
the `Rv64TrapFrame` layout, and `frame.view_mut()` packages a `(view, raw, vtable)`
handle. Sink code calls `view.set_syscall_return(n)` and it routes through the
vtable function pointer.

Why a vtable here when everything else in the HAL is monomorphized? The doc
(`txdoc:HAL-TRAPIF-…-TRAPFRAMEVIEW-AND-TRAPFRAMEMUT-1`) gives the reasoning, and
the code follows it: the alternative — making `TrapFrameMut` generic over `P` —
would propagate `P` into every signature that returns from a syscall, everywhere.
The vtable confines the indirection to the *write* methods (rare: typically one
`set_syscall_return` per syscall) while the *read* path (`TrapFrameView`) stays
fully concrete. It's one `&'static` per platform and one indirect call per
writeback, dwarfed by the syscall's own cost. (Reality adds a few methods the doc
didn't list — `capture_user_context`, `restore_user_context`,
`set_signal_handler_regs`, `rewind_pc` — which Chapters 10 and 12 need.)

## The sink: `KernelTrapDispatcher`

The sink trait (`crates/tx-hal/src/trap.rs:311`) is six methods:

```rust
pub trait KernelTrapSink<P: TxPlatform> {
    fn on_page_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction;
    fn on_syscall(view: TrapFrameMut<'_>) -> TrapAction;
    fn on_timer_interrupt(cpu: CpuId, view: TrapFrameMut<'_>) -> TrapAction;
    fn on_external_irq(cpu: CpuId) -> TrapAction;
    fn on_ipi(cpu: CpuId) -> TrapAction;
    fn on_illegal_or_sync_fault(view: TrapFrameMut<'_>, fault: FaultInfo) -> TrapAction;
}
```

and it has exactly one impl, `KernelTrapDispatcher` (`crates/tx-kernel/src/trap.rs:12`),
generic over `P`. Reading its methods tells you the kernel's whole trap policy:

- **`on_page_fault`**: kernel-mode fault → `Terminate`; user-mode fault → snapshot
  context and hand off to a userspace-run wait (Chapter 10), returning `Reschedule`.
- **`on_syscall`**: a fast path for a few "direct trap" syscalls
  (`try_direct_trap_syscall` handles e.g. `rt_sigprocmask`, `set_tid_address`),
  else snapshot + hand off + `Reschedule`. Crucially, *no writeback happens here* —
  the comment cites the "Plan B writeback discipline" (Chapter 10).
- **`on_timer_interrupt`**: cancel the deadline; if the trap came from user mode,
  record a timer preemption and `Reschedule`; if from kernel mode, `Resume` (the
  kernel is cooperative, never preempted).
- **`on_external_irq`**: `P::claim()` → `P::dispatch_irq(irq)` → `P::complete(irq)`;
  `Reschedule` if a handler woke something, else `Resume` (Chapter 13).
- **`on_ipi`**: ack pending Membarrier / Reschedule / TlbShootdown IPIs; `Resume`
  (Chapter 14).
- **`on_illegal_or_sync_fault`**: kernel-mode → `Terminate` (a kernel bug should
  stop the world); user-mode → deliver a fatal signal and `Reschedule` (a bad user
  instruction must never panic the kernel).

`TrapAction` (`crates/tx-hal/src/trap.rs:302`) is the four-way result:

```rust
pub enum TrapAction { Resume, Reschedule, DeliverSignal, Terminate }
```

What the shell does with each — and why `Reschedule` is the linchpin of the whole
async kernel — is the subject of the next chapter.

> **Divergence — `on_timer_interrupt` gained a `view`.** The doc's sink has
> `on_timer_interrupt(cpu)`; the shipped one is `on_timer_interrupt(cpu, view)`,
> because timer-driven preemption of *userspace* needs to snapshot the user
> context (Chapter 10). Small signature drift, real reason.

## What you should take away

- `TrapIf` splits trap handling into an arch-specific shell (board) and a neutral
  sink (`KernelTrapSink<P>`, one impl in `tx-kernel`); neither speaks the other's
  vocabulary.
- The raw frame is a concrete `Rv64TrapFrame`, not an associated type; the vector
  saves it (with `sscratch` swap and lazy FP), and a `#[no_mangle]` bridge in the
  *binary* connects `dispatch_trap_frame::<K>` to `KernelTrapDispatcher`.
- `TrapFrameView` is a concrete read snapshot; `TrapFrameMut` is a vtable-backed
  write handle, deliberately confining indirection to rare writeback calls.
- The six sink methods encode the kernel's entire trap policy; user faults never
  panic the kernel, kernel faults always do.

Next: [Chapter 10 — Why divergent-into-userspace returns](ch10-stackless-coroutine-return.md),
the trick that lets `Reschedule` turn an `sret` into a normal function return.
</content>
