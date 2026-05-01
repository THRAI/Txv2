---
date: 2026-05-01
topic: "Reactor runtime dispatch audit"
status: complete
plan: docs/progress/plans/2026-05-01-reactor-runtime-dispatch.json
---

# Reactor Runtime Dispatch Audit

## Accepted work

- Pasteur implemented `tx_reactor::userspace` as a reactor-local
  userspace-run wait shell. The accepted boundary is a single in-flight
  request, explicit dispatch/preemption/completion phases, and raw
  syscall/page-fault/fatal trap outcomes. Timer preemption does not resolve the
  wait, and VM, signal, ThreadRuntime, and trap-frame policy stay out of the
  module.
- Ptolemy implemented `tx_reactor::hart_loop` as a platform-independent
  per-hart step shell. The accepted boundary is a bounded step over reactor
  time advancement, wake dispatch, preemption-marker consumption, ready-task
  polling, and next-deadline reporting. HAL WFI, timer programming, IPI
  acknowledgement, and trap mechanics stay outside the crate.
- Mencius scoped the next CoreInit runtime-loop slice. The accepted next move
  is a private `CoreInit<P>` adapter over `tx_reactor::hart_loop`, preserving
  the current boot smoke while proving bounded BSP/AP runtime-loop behavior.
- Meitner scoped the AST return-to-user hook. The accepted boundary is a
  policy-neutral userspace-entry AST checkpoint that drains task-local markers
  and hands them to adjacent ThreadRuntime/signal/trap owners without selecting
  signals or mutating user trap frames.

## Coordinator decisions

- Do not wire a public `Reactor::request_userspace_run` facade in this pass.
  The userspace-run slot is useful as a host-testable mechanism, but real task
  state, `ThreadPayload`, trap classification, and VM/signal owners are still
  missing.
- Do not move CoreInit/AP loops to the new per-hart step in the same pass.
  The API now exists, but the boot tail still depends on a finite smoke path;
  replacing it needs a board/runtime slice with QEMU smoke validation.
- Do not implement AST delivery policy in reactor. Reactor owns the temporal
  checkpoint and marker handoff; ThreadRuntime, signal/process, VM, and HAL
  trap code own semantics and frame mutation.

## Verification

- `cargo test -p tx-reactor --test userspace_run`
- `cargo test -p tx-reactor --test hart_loop`
- `cargo test -p tx-reactor`
- `cargo fmt --check`
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask ci`
- `git diff --check`

## Remaining production gaps

- Full `Reactor::request_userspace_run` task integration is still pending.
- CoreInit/HAL still need a permanent timer/IPI/WFI runtime loop over the
  per-hart step shell.
- The saved-register trap shell, real `return_to_userspace`, VM fault policy,
  ThreadRuntime, and signal delivery remain adjacent blockers.
- Device/block/VFS IRQ completion can prototype against declared waits, but
  production IRQ-driven runtime still depends on the CoreInit/HAL loop slice.
