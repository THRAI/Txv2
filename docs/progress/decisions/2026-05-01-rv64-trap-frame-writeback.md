---
date: 2026-05-01
topic: "RV64 trap-frame writeback and user-return skeleton"
status: complete
---

# RV64 Trap-Frame Writeback and User-Return Skeleton

## Decision

RV64 QEMU now backs `TrapFrameMut` with the live saved trap frame instead of a
read-only marker. `tx-hal` exposes a small `TrapFrameMutVtable` for PC, SP,
syscall return/error, and user TLS writes. `Rv64TrapFrame::view_mut()` installs
the RV64 vtable, and `dispatch_trap_frame` hands that mutable handle to the
kernel sink for page faults, syscalls, and synchronous faults.

The RV64 trap vector now writes saved `sepc` and `sstatus` back before `sret`,
so frame-level PC and privilege/status edits take effect on trap return. RV64
also has `Rv64TrapFrame::prepare_user_return()` to clear SPP and set SPIE, plus
an unsafe `return_to_userspace` register-restore skeleton for the future
userspace trampoline.

## Boundary

- This is the trap-frame writeback substrate. It is not syscall dispatch, VM
  page-fault handling, signal delivery, user trap stack switching, user-access
  recovery, external IRQ/device dispatch, or ThreadRuntime userspace-run
  integration.
- `return_to_userspace` is an unsafe restore primitive. It is not called by the
  scheduler or reactor yet, and there is no runnable user task model to enter.
- The platform still owns raw trap mechanics; the kernel sink owns trap policy.
  There is no runtime HAL manager or HAL callback table.

## Verification

- `cargo test -p tx-hal-riscv64-qemu-virt`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask build --target rv64-qemu`
- `cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `cargo xtask ci`
- `git diff --check`
