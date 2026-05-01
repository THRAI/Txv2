---
name: tx-process-threadruntime
description: Use when implementing or auditing Process, ThreadRuntime, signal delivery, syscall/AST loop, exec/fork/clone/exit/wait, first userspace, or runtime integration over VM and trap state.
---

# tx-process-threadruntime

Use this skill before Process, ThreadRuntime, signal, syscall, exec, or first
userspace work. These areas are tightly coupled; do not fan them out until the
shared entity and trap/VM seams are explicit.

## Read First

- `docs/design/04_process-signals/PROCESS_v1.md`
- `docs/design/02_execution/THREAD_RUNTIME_v1.md`
- `docs/design/04_process-signals/SIGNAL_v1.md`
- `docs/design/04_process-signals/SIGNAL_ATTACHMENTS_v1.md`
- `docs/design/02_execution/EXEC_v1.md`
- `docs/design/02_execution/REACTOR_v0.md`
- `docs/design/02_execution/SCHEDULER_v0.md`
- `docs/design/03_memory-vm/VM_v1_2.md`
- `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`
- relevant trap/HAL progress notes when touching return-to-user paths

## Preserve

- Process and ThreadRuntime share key seams: `Process`, `Frame`,
  `ThreadIdentity`, `ThreadPayload`, task ownership, saved user registers, and
  signal state. Do not implement them as independent incompatible lanes.
- `ThreadPayload` owns user register/signal/thread state; the reactor owns task
  scheduling mechanics. Do not put POSIX policy inside `tx-reactor`.
- User return and syscall/page-fault writeback cross HAL trap-frame APIs. Do
  not mutate trap/HAL contracts from a process lane without coordinator scope.
- `Frame.vm` depends on real VM `AddressSpace`; fd/fs slots depend on VFS and
  Mount seams. Stub only with explicit blockers.
- Step implementations must use the five-stage order: observe, upgrade,
  reserve, commit, publish.
- Upper APIs expose role-shaped handles (`Cap<T>`, `Weak<T>`,
  `IdentRef<'g, T>`, witnesses), not raw `Zone<T, Policy>`.

## Implementation Harness

- Start with a readiness table for Process, ThreadRuntime, signal, VM, trap,
  VFS, and reactor dependencies.
- Prefer one coordinator-owned `process-thread-runtime-core` lane for shared
  types before splitting syscall, signal, exec, and wait/exit scripts.
- Keep first userspace behind VM AddressSpace, trap return/writeback,
  ThreadRuntime payload, and minimal VFS/exec seams.
- Progress records must name deferred seams explicitly; do not hide them behind
  placeholder structs.

## Checks

- `cargo fmt --check`
- targeted `cargo test -p tx-kernel ...` for touched modules
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
  when trap/runtime paths are touched
- `cargo xtask progress validate`
- `cargo xtask lint docs` when docs are touched
- `git diff --check`
