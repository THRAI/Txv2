# Chapter 11 — User-memory access: eager walk and the surviving fixup exception

A syscall like `write(fd, buf, len)` hands the kernel a *user* pointer `buf`. The
kernel must read those bytes — but the user pointer is interpreted against the
user page table, the pages may not be present, and (this being txKernel) the
fetch might need to `.await` a file-backed page. This chapter is about how
txKernel reads and writes user memory, and about a piece of machinery the design
doc declared *retired* that nonetheless still ships for one narrow purpose.

## The two classic approaches

Historically there are two ways to touch user memory from the kernel:

1. **Trust-and-fault (the Linux `copy_to_user` model).** The kernel enables a
   "supervisor may access user" mode (SUM on RISC-V, SMAP/STAC on x86), does a
   plain `memcpy`, and registers a *fixup table*: if the copy faults, the
   kernel-mode page-fault handler looks up the faulting PC, finds a recovery stub,
   and returns `-EFAULT` instead of panicking. Fast, but it requires kernel-mode
   fault recovery and an asm copy loop.
2. **Eager walk.** The kernel walks the user page tables itself, page by page,
   materializing each page (allocating anonymous pages, fetching file-backed
   pages) and copying through the kernel's own mapping of that physical frame. No
   kernel-mode faults, no fixup table — but the kernel does the address
   translation in software.

txKernel chose **eager walk** as the main path, and the doc (`HAL_v1 §12`)
records that the `UserAccessIf` trait, the `KernelPtr<T>` wrapper, the
`FixupEntry` struct, and kernel-mode fault-fixup recovery were all **retired**.

## Why eager walk fits an async kernel

The deciding factor is Chapter 10's stackless-coroutine model. A user buffer may
be file-backed and not yet in memory; fetching it is an `.await`. The
trust-and-fault model can't express that — a page fault in the middle of a
`memcpy` can't suspend a future. Eager walk can: the walk materializes each page
through its `VmEntry.backing`, and if a page needs fetching it returns a "blocked"
outcome carrying a wait token, the syscall carrier rewinds, and the future
`.await`s the fetch and retries. The doc's retirement note spells out the three
outcomes of each materialization: a frame, `Errno::EFAULT`, or a yield carrying a
`WaitToken`.

The implementation lives in the VM subsystem, not the HAL —
`crates/tx-subsystems/src/vm/user_access.rs` as inherent methods on
`AddressSpace`: `copy_from_user`, `copy_to_user`, `read_user`, `write_user`,
`read_user_cstr`. Syscall arms call `ctx.aspace.copy_from_user(...)` etc. The HAL
provides only the `UserPtr<T>` value type (Chapter 3) and the trap classification;
the *policy* of "walk the recipes, materialize, copy through the direct map" is VM's.

This is the HAL boundary doing its job: user-memory access is a *semantic*
operation (it consults the address space's VM recipes), so it belongs above the
HAL, in the subsystem that owns address spaces — not in a `UserAccessIf` trait the
board would implement.

## The narrow exception that survived

Here's the divergence. Eager walk is the rule, but it has one structural
limitation: it works when the resolve-side machinery is reachable — when you can
`.await`, rewind, and retry. There is one moment where that isn't true:
**synchronous signal-frame writes** (Chapter 12). When the trap shell selects a
signal to deliver, it must write the signal frame onto the *user* stack page right
then, with the user image suspended at trap entry, before the future's resolve
machinery is back in play. There's no future to suspend, no wait to rewind.

So a narrow SUM-window + asm-copy + fixup primitive survives, board-internal, in
`boards/tx-hal-riscv64-qemu-virt/src/user_access.rs`:

```rust
unsafe extern "C" {
    fn tx_rv64_cfu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;  // copyin
    fn tx_rv64_ctu_raw(dst: *mut u8, src: *mut u8, len: usize) -> usize;  // copyout
    // label symbols bounding the faulting load/store instructions:
    fn tx_rv64_cfu_ld_s(); fn tx_rv64_cfu_ld_e(); fn tx_rv64_cfu_fault();
    fn tx_rv64_ctu_st_s(); fn tx_rv64_ctu_st_e(); fn tx_rv64_ctu_fault();
}
```

These are byte-loop copy routines (`lbu`/`sb`) wrapped in a SUM-window guard that
toggles `sstatus.SUM` so supervisor code may touch user pages. The copy functions
return `0` on success or the faulting VA on failure.

### A two-entry fixup table

Because these copies *can* fault on a bad user address, there is — yes — a fixup
table, but a tiny static one with exactly two entries (`user_access.rs:157`):

```rust
static RV64_FIXUP_TABLE: [RawFixupEntry; 2] = [
    RawFixupEntry { pc_start: tx_rv64_cfu_ld_s, pc_end: tx_rv64_cfu_ld_e, recovery_pc: tx_rv64_cfu_fault },
    RawFixupEntry { pc_start: tx_rv64_ctu_st_s, pc_end: tx_rv64_ctu_st_e, recovery_pc: tx_rv64_ctu_fault },
];
```

Each entry says: "a kernel-mode fault whose PC is in `[pc_start, pc_end)` should
not be treated as a kernel bug; redirect `sepc` to `recovery_pc` and carry on."
`fixup_lookup(fault_pc)` (`user_access.rs:176`) is a two-element linear scan.

And it is consulted in exactly the place Chapter 9 showed — the page-fault arm of
`dispatch_trap_frame` (`board trap.rs:860`), *only* for kernel-mode faults:

```rust
TrapClass::PageFault { write, instruction } => {
    if !from_user {
        if let Some(recovery_pc) = user_access::fixup_lookup(frame.sepc) {
            frame.sepc = recovery_pc;     // jump to the recovery stub
            frame.x[X_A0] = frame.stval;  // hand it the faulting VA
            return TrapAction::Resume;
        }
    }
    // … otherwise, real fault → K::on_page_fault → terminate (kernel) or hand off (user)
}
```

If a kernel-mode fault's PC isn't one of the two known copy instructions, there is
no recovery — it falls through to `on_page_fault`, which `Terminate`s (a genuine
kernel bug). The fixup table is *not* a general kernel-fault recovery mechanism;
it is two instructions' worth of escape hatch for the one synchronous
user-touching primitive that eager walk can't cover.

> **Divergence — "retired" vs the surviving exception.** The doc's §12 says the
> fixup table, `FixupEntry`, and kernel-mode fault recovery were retired, and
> §21.1 even lists `KERNEL_FIXUP_TABLE` (a `linkme` slice) as the *one* allowed
> trap-consulted slice. Reality: the general `linkme` fixup table is gone (eager
> walk replaced it, exactly as the doc says), **but** a board-internal, 2-entry,
> non-`linkme` `RV64_FIXUP_TABLE` survives for synchronous signal-frame writes,
> consulted directly in the trap shell. The doc's own §12 retirement note actually
> acknowledges this ("the narrow exception is signal-frame writes… they still use
> a board-internal SUM/asm primitive"), so the two halves of the doc disagree with
> each other; the code matches the §12 note, not the §21 slice list. See
> [Appendix A](appendix-a-design-vs-code-ledger.md).

## Why the split is the right shape

It would be simpler to pick one model. But the two paths have genuinely different
constraints:

- **Bulk syscall I/O** (`read`/`write`/`copy_*_user`) can and must be async — the
  pages might be file-backed — so it uses eager walk above the HAL, where
  `.await` is available.
- **Signal-frame writes** happen at a single synchronous instant inside trap
  handling, before the async machinery is reachable, so they use a tiny SUM+fixup
  primitive below the HAL boundary, in the board.

The lesson generalizes: the HAL boundary follows *capability*, not topic. "Access
user memory" isn't one operation at one layer — the async bulk path lives in VM,
the synchronous trap-time path lives in the board, and they don't pretend to be
the same thing.

## What you should take away

- The main user-memory path is eager walk, implemented in VM
  (`AddressSpace::copy_*_user`), not in a HAL trait — because it must `.await`
  file-backed page fetches, which the stackless-coroutine model supports and
  trust-and-fault does not.
- A narrow board-internal SUM + asm-copy + 2-entry fixup primitive survives for
  synchronous signal-frame writes, consulted only for kernel-mode faults in the
  trap shell.
- The fixup table is an escape hatch for two specific instructions, not a general
  kernel-fault recovery; any other kernel-mode fault terminates.
- The doc's "retired" language is half-right (the general/`linkme` table is gone)
  and the surviving exception matches the doc's own §12 note.

Next: [Chapter 12 — Signals across the ABI boundary](ch12-signals-abi.md), which
is the consumer of that synchronous write primitive.
</content>
