# Userspace first-entry gap (post-cpio-unpacker)

**Date:** 2026-05-08
**Branch:** `feat/busybox-smoke`
**Status:** Architecture finding; needs a dedicated slice.

## TL;DR

The kernel never enters userspace under the current
`run_userspace_reactor_loop` + `run_thread` design. **The boot
sentinel `boot:ok` fires before userspace runs, and userspace
never runs.** Both the bake-in `init_fixture` and the new busybox
init path hit this; verified empirically by adding a trap-shell
trace at the top of `on_page_fault` / `on_syscall` /
`on_illegal_or_sync_fault` and observing **zero trap traces** in
either configuration after a 5-second QEMU run.

## What the test actually proves today

`cargo xtask test busybox-smoke --target rv64-qemu` watches for
the `boot:ok` sentinel and PASSes when it appears. That sentinel
is emitted by `CoreInit::boot_sentinel()`, which fires *before*
`run_userspace_reactor_loop`. The serial after a successful run:

```
:init:fixture:ok
:initramfs:2-files:9-dirs:6-symlinks:1021810-bytes:ok  (this slice)
:bootstrap-exec:ok                                     (Phase 7)
:boot:ok                                               (← test PASSes here)
[no further output; no traps; sentinel-watcher kills QEMU]
```

The userspace-exited sentinel `txkernel:...:userspace:exited:N`
that `run_userspace_reactor_loop` would emit if init zombified is
**never observed** — userspace doesn't run, init doesn't zombify,
the BSP loop just polls + WFIs forever until QEMU times out.

## Root cause

`crates/tx-kernel/src/thread_future.rs::run_thread`'s loop body:

```rust
loop {
    let wait = payload.userspace_slot().start_request()?;  // (1)
    payload.set_active_userspace_request(Some(wait.request()));
    let trap = wait.await;                                  // (2) ← yields
    match trap { /* dispatch syscall / page fault */ }      // (3)
    let entry_wait = payload.userspace_slot().start_request()?;  // (4)
    let ctx = prepare_userspace_entry_payload(&payload);    // (5)
    <P as TrapIf>::enter_userspace_with_context(ctx);       // (6) `-> !`
}
```

**Chicken-and-egg**: line (6) is the only path to userspace, but
it's gated by `wait.await` at (2), which only resolves when the
trap shell calls `complete_interesting_trap` — which only fires
when userspace traps — which can't happen before line (6).

The future yields Pending on (2). The reactor schedules other
tasks (none ready). The BSP `step_boot_reactor_once` reports
idle. `run_userspace_reactor_loop` calls
`P::wait_for_interrupt_once()`. WFI wakes on timer interrupt;
the BSP polls again, reactor re-polls run_thread which yields
again at (2). Loop forever.

## Why we didn't notice earlier

The existing `cargo xtask test busybox-smoke` only watches for
`boot:ok`. The bake-in init fixture's exec'd ELF was assumed to
run because `bootstrap-exec:ok` fired (= `exec_script` succeeded
+ `saved_user_context` was seeded). But seeding the saved context
is not the same as actually re-entering userspace; the seeded
context only matters *if* `enter_userspace_with_context` ever
gets called.

Every "shell-prompt roadmap" slice up through this one was
correct and necessary kernel work — fd table, syscalls, fault
path, ELF loader, cpio unpacker — but none of them required
userspace to actually run because the host-side dispatch tests
exercise the syscall arms directly.

## Fix shape (for the next slice)

The cleanest shape is to **invert the first iteration** of
`run_thread`'s loop so userspace entry happens before the first
await:

```rust
loop {
    let entry_wait = payload.userspace_slot().start_request()?;
    payload.set_active_userspace_request(Some(entry_wait.request()));
    let ctx = prepare_userspace_entry_payload(&payload);
    <P as TrapIf>::enter_userspace_with_context(ctx);  // `-> !` first iter
    // Subsequent iterations resume here via the trap shell waking
    // entry_wait after a userspace trap and re-polling this future.
    let trap = entry_wait.await;
    match trap { /* syscall / page-fault dispatch */ }
}
```

Alternatives considered:

1. **Bootstrap-side first entry**: `drive_bootstrap_exec` calls
   `enter_userspace_with_context` once *before* submitting
   `run_thread` to the reactor. Cleaner separation but breaks the
   "future owns the userspace round-trip" invariant the slice
   plan committed to.

2. **Synthetic first-trap injection**: bootstrap manually calls
   `complete_interesting_trap(initial_request, /*synthetic
   FirstEntry*/ trap)`. Requires a new `UserspaceTrapInfo::FirstEntry`
   variant the dispatcher can recognise as a no-op. More moving
   parts than the loop-inversion.

3. **Per-iteration enter-then-await** (recommended): the inversion
   above. Single-line restructure; the comment block describing
   the loop semantics needs an update; existing host tests on
   `prepare_userspace_entry_payload` and `dispatch` still apply.

## Next slice scope

- Restructure `run_thread`'s loop to enter-then-await per the
  recommendation above.
- Verify the post-trap re-entry path: after a syscall, we hit
  `match trap` → dispatch → fall through to top of next iteration
  → start a fresh `entry_wait` → re-enter. The `entry_wait`
  generation must not collide with the previous wait's generation.
- Add a trap-shell trace (under a `#[cfg(feature = "trap-trace")]`
  gate or similar) so we can observe the userspace round-trip
  on demand in future smoke runs.
- Update `cargo xtask test busybox-smoke` to also watch for the
  `:userspace:exited:N` sentinel (proving init actually ran).
- Possibly: a new sentinel `:userspace:first-trap:ok` emitted on
  the first time `complete_interesting_trap` resolves a wait,
  so we can distinguish "userspace ran" from "userspace
  zombified" in the smoke.

## What's still good in this slice

The cpio unpacker, cmdline-init parser, and ELF-loader BSS-overlap
fix landed in commit `fe9a30b` are all correct and necessary.
None of them require userspace to actually run; they're tested
host-side. The integration testing for the userspace path was
just always missing.

---

## 2026-05-08 update — slice 2 implementation in progress

Commits landed:

- **`196a969`** "thread future: invert run_thread loop + drop -> !
  from enter_userspace_with_context" — the type/loop/test/docs
  piece. Workspace-wide host tests green (0 failed). The runtime
  still diverges via `return_to_userspace` (the body type-changed
  but didn't yet return at runtime); that came in the asm slice.

- **`eb3e66a`** "rv64-qemu trap shell: reschedule longjmp via
  per-hart KernelResumeCtx + per-CPU trap stack" — the asm slice.
  Static `[Rv64TrapStack; MAX_BOOT_CPUS]` in .bss; boot-time
  sscratch primer in `install_kernel_stack`; trap-vector prologue
  does `csrrw sp, sscratch, sp` onto the trap stack; new asm
  helpers `tx_rv64_enter_userspace_save_resume` and
  `tx_rv64_resume_kernel_after_reschedule`; `apply_trap_action`'s
  Reschedule arm re-primes sscratch and longjmps via the helper.
  Cross-builds clean for `riscv64gc-unknown-none-elf` (full
  kernel binary builds; asm assembles).

Static-array vs substrate-allocated trap stack: chose static for
two reasons: (1) the trap stack must exist before substrate is up
because the trap vector is installed early in boot, and (2) at
4 × 16 KiB = 64 KiB total it's a trivial .bss cost. Substrate
allocation can replace the static array later if memory pressure
becomes an issue — the asm only sees the stack-top pointer.

### What QEMU validation showed (status: needs debug iteration)

`cargo xtask test busybox-smoke --target rv64-qemu` after the asm
slice landed produced multi-hart concurrent panics with corrupted
serial. The pattern emitted from `tx_rv64_qemu_trap_panic` is
visible:

```
txkernel:qemu-riscv64-virt:trap
reason=trap-action-terminate
scause=0x100001  sepc=0x1ffffff0f5a09c4  stval=0x1ffffff0f0fffe1
```

The fact that `tx_rv64_qemu_trap_panic` is reached means the
trap-vector prologue's sscratch swap worked (we got into the Rust
handler, dispatched, and chose Terminate). The bug is likely
elsewhere: the `sepc` / `stval` values do not match any normal
kernel-VMA range, so something is being dereferenced through a
junk pointer. Suspects to investigate:

1. **AP boot path** — does `secondary_start` go through
   `install_kernel_stack` in a way that primes sscratch before any
   trap can fire on the AP? Need to confirm the AP boot trampoline
   sequencing.
2. **`tx_rv64_qemu_install_kernel_stack`'s sp swap** — it does
   `mv sp, a0; ret` which replaces the stack mid-function. Adding
   the sscratch primer right after means the calls
   `current_cpu_id()` and `trap_stack_top_for_cpu()` happen on the
   newly-installed stack; that should be OK but worth verifying
   the calling-convention contract holds.
3. **First user trap** — even before busybox runs, init-fixture's
   bootstrap-exec runs to seed `saved_user_context`. If the very
   first user trap from the bake-in init fixture's first ecall
   exposes a bug in the swap discipline, that's where to look.

### Next steps for iteration

- Add a transient `tx-trap-trace` feature: emit a one-character
  serial sentinel from each phase of the asm path
  (`csrrw`, `addi`, `csrr` for sp save) so we can pinpoint where
  in the prologue the corruption happens.
- Run busybox-smoke with `cargo xtask fault-decode --target
  rv64-qemu` parsing the serial — the fault-decode tool already
  handles low-linked + high-VMA layouts and demangling, so the
  corrupted output may yield more signal.
- Single-hart QEMU run (`-smp 1`) to remove the multi-hart
  interleaving from the serial.
- Dump `sscratch` and the per-CPU trap-stack base via the panic
  path (extend `console_write_trap_summary`) to confirm sscratch
  is what we expect at trap time.
- If the first trap is from a fault-on-fault (re-entry into the
  trap vector before it finished setup), an SPP guard in the
  prologue may be needed after all.

Status: type+loop+docs (196a969) is solid and tested; asm slice
(eb3e66a) compiles + cross-builds but needs QEMU iteration to
validate the runtime path. Both commits are reachable on
`feat/busybox-smoke`.

---

## 2026-05-08 second update — runtime validated, three bugs fixed,
## new blocker (page-fault loop) identified

After a single-hart QEMU run + `cargo xtask fault-decode`, three
unrelated bugs surfaced and were fixed in `007acca`:

1. **sscratch primer in dead code path.** The Rust
   `install_kernel_stack` is only invoked from a unit test; the
   runtime boot uses an asm `tx_rv64_qemu_install_kernel_stack`
   directly. Moved the primer to `install_early_percpu`, which
   runs from `tx_hal::entry()` on every hart's boot path.

2. **Trap stack in `.rodata`.** A plain `static [Rv64TrapStack; N]`
   landed in `.rodata` (mapped `KERNEL_RO`); the trap-vector's
   first store faulted. Wrapped in `PerHartCell` (UnsafeCell
   newtype) so the linker keeps it in `.bss` (writable).

3. **`console_write_hex` off-by-3 shift.** `(0..64).rev().step_by(4)`
   yielded 63, 59, ..., 3 instead of the intended 60, 56, ..., 0.
   Every printed address was shifted by 3 bits, breaking fault
   triage. Fixed to `(0..64).step_by(4).rev()`.

After these fixes:

- Boot proceeds cleanly through `:reactor:timer-idle:ok` (timer
  interrupts are handled correctly through the new sscratch-swap
  trap vector — first runtime proof the asm prologue works).
- `:bootstrap-exec:ok` and `:boot:ok` fire as expected.
- The reschedule longjmp **works end-to-end**: a temporary trace
  showed continuous `[ENT][RSC][RTN]` cycles — userspace enters
  via the save_resume helper, traps, the trap shell chooses
  Reschedule, the longjmp asm helper unwinds back through
  `enter_userspace_with_context`'s normal return, the future
  awaits the resolved wait, dispatches the trap, and loops back
  to the next entry. **The entire stackless-coroutine + reschedule
  longjmp model executes correctly.**

### New blocker — userspace page-fault retry loop

Trap-class tracing (also temporary, removed before commit) showed
**every userspace trap is `[pi]` (instruction page fault from
user)**, repeating in a tight loop. The thread future's `PageFault`
arm calls `aspace.fault_script(VmFault).await`, falls through on
`Ok` to the loop top, re-enters userspace at the same `sepc`, and
takes the same fault again.

This is **not** a slice-2 issue. It's a question about what the
fault script actually does for a busybox text page that should
already be present (the eager-walk VM model is supposed to
materialise demand-loaded pages on first access). Suspects to
investigate in a follow-up:

- Does `aspace.fault_script` actually publish an executable PTE
  for the faulting page? Maybe it returns `Ok` without installing
  a leaf, or installs without `X` permission for `.text`.
- Does the dispatch path emit `sfence.vma` for the just-mapped
  page so the retry sees a fresh TLB? (The first map of a never-
  cached page typically doesn't need a flush, but the elf loader
  may be re-using a slot.)
- Is busybox's text region actually backed by a recipe? The eager
  ELF loader registers segments — verify the recipe is in the
  `BTreeMap<Range, Recipe>` for the entire text segment.
- The cmdline-init path resolves `tx.profile=busybox` →
  `/bin/busybox` (symlinked from `/init` per `prepare_busybox_rootfs`).
  Maybe the fallback to bake-in `/init` fired and bake-in `/init`
  faults at `_start`. Verify which exec succeeded by capturing
  the `:bootstrap-exec:fallback:<tag>` sentinel.

### Files touched in 007acca

- `boards/tx-hal-riscv64-qemu-virt/src/lib.rs`: move sscratch
  primer; wrap RV64_TRAP_STACKS in PerHartCell.
- `boards/tx-hal-riscv64-qemu-virt/src/trap.rs`: fix
  console_write_hex shift sequence.

cargo build (host + RV64) clean; workspace host tests green.
