# 2026-04-29: RV64 trap classification surface

## Context

`TrapIf` had grown vector-install hooks, but code still had no typed trap
classification surface. That made the docs' full trap-shell plan hard to
connect to the executable HAL boundary.

## Decision

Add `TrapFrameSnapshot` and `TrapClass` to `tx-hal::TrapIf`. RV64 QEMU decodes
`scause` into common synchronous fault classes and supervisor interrupts while
continuing to route the direct-mode vector to the panic/spin sink.

This is not the full user trap shell. It is the typed classification boundary
that the later saved-register frame, syscall dispatch, signal delivery, and
user-return path will call through.

## Verification

- `cargo fmt`
- `cargo test -p tx-hal-riscv64-qemu-virt rv64_trap_classification`

## Next Step

Implement the saved-register `RawTrapFrame`, `KernelTrapSink` dispatch boundary,
and user-return assembly when userspace entry work begins.

## Blockers

User-return depends on the VM/user execution slice, not just page-substrate boot
initialization.
