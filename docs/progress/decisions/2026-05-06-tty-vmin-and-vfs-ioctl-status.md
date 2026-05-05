# TTY VMIN slice and VFS ioctl status

**Date:** 2026-05-06
**Branch:** local worktree
**Status:** Complete.

## Context

TTY design work had reached the point where:

- process-aware tty ioctl helpers already existed
- `OpenFile::step_read` / `step_write` already dispatched tty-backed files
- progress notes still described tty ioctl as a helper-only surface
- non-canonical `VMIN` / `VTIME` read completion remained entirely unstaged

This note records the narrower follow-up that landed without pulling syscall or
reactor runtime work onto the critical path.

## What landed

1. `OpenFile::step_ioctl(...)` is now a real VFS-local dispatch entry for
   tty-backed `OpenFile`s.
   - typed request surface: `OpenFileIoctl`
   - typed result surface: `OpenFileIoctlResult`
   - process-aware caller carrier: `OpenFileIoctlCaller`
2. Non-canonical `VMIN` now affects tty reads in the `VTIME == 0` slice.
   - `ICANON` reads are unchanged
   - `!ICANON && VTIME == 0 && VMIN > 0`: `step_read` blocks until at least
     `min(VMIN, out.len())` bytes are queued
   - `!ICANON && VTIME == 0 && VMIN == 0`: empty queue returns `Done(0)`
   - any `VTIME != 0` case still falls back to the previous staging behavior
3. Module/docs status text was updated so tty's current implementation state is
   no longer described as an early pre-VFS phase.

## What this does not finish

- No syscall request-number decoding or user-buffer copying was added.
- No fd-table lookup path was added.
- `tcsetpgrp(fd, pgid)` numeric `pgid -> Cap<ProcessGroup>` resolution is still
  follow-up work.
- `VTIME` remains unimplemented.
- Full Linux/POSIX background tty blocking semantics remain unimplemented.
- `tcsets` still uses inline linearizer application rather than a
  reactor-scheduled synthetic ingest wake.
- `TtyPayload.termios` remains `AtomicSlot<Termios>`, not the final
  `AtomicSlot<Arc<Termios>>` shape.

## Verification

- `cargo fmt --all`
- `cargo test -p tx-subsystems tty -- --test-threads=1`
- `cargo test -p tx-subsystems vfs -- --test-threads=1`
- `cargo clippy --workspace --all-targets --exclude tx-kernel-riscv64-qemu-virt --exclude tx-kernel-riscv64-m1dock-mock --exclude tx-kernel-loongarch64-qemu-virt -- -D warnings`
