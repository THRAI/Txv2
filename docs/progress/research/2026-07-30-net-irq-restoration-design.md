# RV64 virtio-net IRQ restoration design

Date: 2026-07-30

Status: implementation approved; design review complete; implementation pending

## Decision

Restore the RV64 `virtio1` network interrupt as a deferred-completion IRQ.
Do not copy the 2026-07-03 `mask -> pending -> trap complete -> device ACK ->
unmask` sequence from `1e96a5e1`.

The restored path will be:

```text
PLIC claim on hart H
  -> IRQ top half publishes { irq = 2, owner = H }
  -> trap leaves the PLIC claim outstanding and wakes hart H's reactor
  -> same-hart bottom half ACKs virtio, polls queues, and kicks the net delegate
  -> bottom half makes the software slot idle
  -> bottom half completes the original PLIC claim on hart H
```

The PLIC gateway itself provides per-source throttling while the claim remains
outstanding. Runtime mask/unmask is therefore unnecessary.

## Why the historical sequence must not be restored verbatim

The July feature implementation correctly separated the lock-free IRQ top half
from the lock-taking driver bottom half, but its controller ordering was not
valid as a general PLIC protocol:

1. It disabled the source in the current target context before the generic trap
   path wrote completion.
2. The PLIC specification says a completion for a source that is not currently
   enabled for that target is silently ignored.
3. It completed the controller claim before clearing the level-triggered
   virtio source. A still-asserted device line may therefore become pending
   again before the later device ACK.

Primary references:

- [RISC-V PLIC interrupt completion](https://docs.riscv.org/reference/plic/plic-completion.html)
- [VirtIO 1.2 MMIO interrupt acknowledgement](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html)

The local implementation confirms the two sides of that contract:

- `crates/tx-kernel/src/trap.rs` currently performs
  `claim -> dispatch -> complete`.
- `virtio-drivers` clears the MMIO source by reading `InterruptStatus` and
  writing those bits to `InterruptACK`; the PCI transport clears its ISR by
  reading it.

## Required invariants

1. **No lock and no epoch guard in the top half.** The virtio
   `ack_interrupt_and_fire()` path takes the driver `SpinMutex`; it stays in
   reactor context. This also preserves `txdoc:INVARIANT-EBR-8`.
2. **Completion ownership is explicit.** A deferred claim records both the IRQ
   number and claimant hart. Only that hart may drain and complete it.
3. **Device ACK precedes controller completion.** The bottom half clears the
   level source and polls/kicks the delegate before completing the PLIC claim.
4. **The software slot becomes idle before completion.** A new event arriving
   after the device ACK but before PLIC completion remains represented by the
   asserted device line; after completion it can be claimed into an idle slot.
5. **No runtime PLIC mask/unmask window.** This avoids ignored completions and
   avoids read-modify-write races with UART/RTC enable bits in the same PLIC
   enable word.
6. **Wake from user mode follows the userspace-run handoff discipline.** An
   external IRQ that returns `Reschedule` must preserve the interrupted user
   frame exactly like a timer preemption. The `9bec2708` fix for this was also
   lost in the later merge and is required for blocked `connect()`/`read()` to
   resume safely.
7. **Publication precedes device notification enablement.** Boot installs and
   unmasks IRQ 2 first while virtio notifications are disabled; device init
   publishes `eth0` before enabling notifications.
8. **The 10 ms poll floor remains a watchdog.** Historical QEMU evidence shows
   that a cold idle virtio-mmio RX frame can occasionally appear without a
   usable interrupt. Restoring the real IRQ chain does not yet justify removing
   that correctness backstop.
9. **LA64 remains poll-backed.** Its current virtio-pci interrupt routing has no
   proven platform IRQ number. `NET_IRQ = 0` remains the unsupported sentinel
   there instead of inventing a GSI.

## API shape

- Add `IrqIf::NET_IRQ` with a default zero sentinel and RV64 value `2`.
- Add `IrqHandled::DeferredWake`. The trap dispatcher must not call
  `IrqIf::complete` for this disposition; the registered handler owns exactly
  one later same-context completion.
- Pass `TrapFrameMut` to `KernelTrapSink::on_external_irq`, restoring the
  already-proven user-preemption handoff.
- Keep deferred-claim state private to `tx-kernel::irq` behind one small atomic
  state-machine slot per hart
  (`Idle -> Publishing -> Pending -> Draining -> Idle`). Indexing publication
  and drain by the current hart makes same-hart ownership structural and avoids
  false wrong-hart reports when AP reactor loops perform their normal checks.
  Call sites use named methods rather than packed integer manipulation.

## Reactor and wait integration

- Add a generic reactor `HartPollBudget`; the reactor remains unaware of HAL or
  device work and merely returns after a caller-selected number of committed
  future polls. The kernel uses a one-poll budget, then drains deferred device
  work before and after each BSP/AP reactor step. This places completion at the
  earliest task-context boundary without hiding a callback inside the
  scheduler. A single helper groups the existing UART and new net bottom
  halves so ordering is visible at each call site.
- Restore the subscribed-state level check in `RawQueueWaitFuture`. The newer
  mailbox installation helper already uses `peek -> subscribe -> peek`, but
  the legacy socket/connect future can still be polled with an existing
  subscription and must honor its queue's asserted readiness bit.

## Verification plan

1. Host tests:
   - IRQ registration includes RV64-style `NET_IRQ`.
   - top half publishes an owned claim and returns `DeferredWake`;
   - wrong-hart drain cannot ACK or complete;
   - same-hart bottom half observes `device ACK/poll` before controller
     completion and completes exactly once;
   - subscribed `RawQueueWaitFuture` resolves from level readiness even if its
     mailbox generation did not match;
   - immediate dispositions still complete in the trap path while
     `DeferredWake` does not.
2. Build both `rv64-qemu` and `la64-qemu`.
3. Run the existing external TCP/Git/curl witnesses.
4. Add opt-in `tx.net.irq_report=1` serial diagnostics and a harness assertion
   that claims are non-zero, completions match claims, and wrong-hart /
   missing-device counters stay zero. Functional network success alone is not
   IRQ evidence because the 10 ms watchdog can mask a missing chain.
5. Keep the watchdog enabled during acceptance and record its retention as an
   explicit follow-up rather than silently treating the stack as pure IRQ.

## Implementation and witness

- Host tests cover the deferred top/bottom split, device-ACK-before-controller
  completion, non-owner hart isolation, both bounded reactor runners, and the
  `RawQueueWaitFuture` level recheck.
- Both `rv64-qemu` and `la64-qemu` kernel builds pass. LA64 deliberately keeps
  `NET_IRQ = 0` because its virtio-pci routing is still unproven.
- The first instrumented QEMU run exposed a scheduling bug rather than a PLIC
  bug: GDB observed a pending IRQ 2 claim while the unbounded inner reactor
  continued polling ready tasks. Functional Git traffic was therefore still
  progressing mostly through the 10 ms watchdog.
- After adding the one-poll owner boundary, `tools/verify-git-net.sh` passed
  all 9 Git/DNS/HTTP/HTTPS checks and reported:
  `claims=59, completions=59, wrong-hart=0, missing-device=0`. This replaces
  the pre-fix run's three eventually completed claims with a continuously
  serviced IRQ path.

## Scope boundaries

- This change does not add a guessed LA64 virtio-pci IRQ route.
- It does not remove the periodic network watchdog.
- It does not introduce NAPI or device-level interrupt suppression.
- It does not move semantic network work into HAL or IRQ context.
- Broader socket/FileOps and network-structure findings remain in
  `2026-07-30-network-refactor-merge-structural-audit.md`.
