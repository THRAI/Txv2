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
