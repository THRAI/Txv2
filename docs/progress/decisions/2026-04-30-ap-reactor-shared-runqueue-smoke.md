---
date: 2026-04-30
topic: "AP reactor shared runqueue smoke"
status: complete
---

# Decision: AP Reactor Shared Runqueue Smoke

## Context

The previous AP wake-loop smoke proved that an AP could wake from
`IpiKind::Reschedule` and consume a kernel-owned work marker. That was still
short of reactor execution: the AP did not borrow reactor state, consume the
target hart's `need_resched` marker, or poll a task from its runqueue.

## Decision

`tx-reactor` now has an explicit shared-reactor boundary:

- Submitted task futures are `Send + 'static`.
- `SharedReactor` wraps an initialized `Reactor` behind a spin lock.
- `Reactor::run_rescheduled_on_hart_with_reschedule` consumes the target hart's
  dispatch marker before draining that hart's runqueue.

The generic kernel boot path now initializes one boot reactor after BSP
substrate/heap/trap setup and before AP bring-up. The dispatcher smoke submits a
remote-affinity task into that shared reactor, parks it once, fires its wait
channel, drains the wake from the BSP, and sends a reschedule IPI. The AP loop
acknowledges the IPI, locks the shared reactor, consumes its hart's
`need_resched` marker, polls the queued task, and records task completion.

RV64 QEMU now prints:

```text
txkernel:qemu-riscv64-virt:reactor:ap-runqueue:ok
```

## Boundary

This is real AP-side reactor task polling, but it is still a serialized boot
reactor smoke. The shared reactor lock prevents concurrent mutation of the task
table, scheduler queues, timers, and dispatch state. That is acceptable as the
first correctness boundary, but it is not the final production shape for high
I/O throughput.

Remaining production work includes per-hart reactor shards or finer-grained
state locks, timer/IRQ integration in the AP idle loop, full bus declaration
hardening beyond the first queue/port wrappers, epoch-protected wire
destruction, kernel-managed shootdown fallback, and userspace
trap/thread-runtime integration.

## Changed Surface

- `crates/tx-reactor/src/task.rs`
  - task futures are now `Future<Output = ()> + Send + 'static`
- `crates/tx-reactor/src/runtime.rs`
  - `RunStats::empty`
  - `SharedReactor`
  - `Reactor::run_rescheduled_on_hart_with_reschedule`
- `crates/tx-kernel/src/init.rs`
  - boot-level shared reactor initialization
  - AP loop drains shared reactor runqueue work after reschedule IPI
  - RV64 QEMU smoke prints `:reactor:ap-runqueue:ok`
- `crates/tx-reactor/tests/reactor_smoke.rs`
  - shared-reactor runqueue drain coverage
- `crates/tx-reactor/tests/completion.rs`
  - host tests now use `Arc`/`Mutex` instead of `Rc`/`Cell` across submitted
    futures

## Verification

```text
cargo fmt --check
cargo test -p tx-reactor --test reactor_smoke
cargo test -p tx-reactor --test completion
cargo test -p tx-reactor
cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo build -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000
```

QEMU serial includes:

```text
txkernel:qemu-riscv64-virt:reactor:dispatch:ipi:ok
txkernel:qemu-riscv64-virt:reactor:ap-loop:ok
txkernel:qemu-riscv64-virt:reactor:ap-runqueue:ok
```

## Next Step

The next dispatcher shard should harden the remaining bus/runtime side:
trace subscriber/nop-patching runtime, subscription lifetime, and concrete
VFS/device owner implementations. For SMP runtime throughput, a later reactor
shard should split the temporary shared reactor lock into per-hart queues plus
a separately protected task table/timer surface.

## Blockers

No blocker for prototype AP-side reactor polling. Full production VFS/device
and block runtime still needs trace subscriber/nop-patching runtime,
subscription lifetime hardening, real IRQ/timer delivery into the loop,
userspace trap integration, and a non-global-lock reactor ownership model.
