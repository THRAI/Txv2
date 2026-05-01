---
date: 2026-04-30
topic: "Reactor readiness for subsystem development"
status: complete
---

# Research: Reactor Readiness For Subsystem Development

## Question

How far is the current reactor design and implementation from a solid reactor
that subsystem authors can build against?

## Conclusion

The active reactor design is mostly ready as an architectural contract for
subsystem authors. It clearly preserves the core boundaries: steps are
synchronous, scripts adapt `Blocked` outcomes into reactor waits, wakes are
hints rather than truth, tasks are temporal handles rather than semantic
entities, and thread runtime owns userspace thread semantics.

The implementation is earlier. `tx-reactor` is a host-testable cooperative
executor skeleton with task-local wakers, parked/runnable task state, wait
channels, timeout waits, and a first `Phase1Scheduler` shell. That is useful
for prototyping kernel-only async flows and for exercising wait discipline. It
is enough for a reactor task or mock device completion to update owner truth and
fire a wake, but it is not yet the runtime substrate that Process,
ThreadRuntime, VM, VFS, TTY, device, block, and signal delivery can depend on
directly.

## Solid Pieces

- `REACTOR_v0` pins the reactor as infrastructure: schedule tasks, mediate
  wait/wake, own AST and cross-core sync carve-outs, and avoid semantic entity
  ownership.
- `STEP_MODEL_v1` and `THREAD_RUNTIME_v1` give subsystem authors a usable
  composition model: `thread_future` contains scripts, scripts compose bounded
  synchronous steps, and wait-adapt is the only blocking mechanism between
  steps.
- `SCHEDULER_v0` defines the policy boundary and a Phase 1 round-robin/two-queue
  scheduler contract.
- `crates/tx-reactor` implements task status, task-local waker coalescing,
  mask-based wait channels, `wait_event` re-observation, timeout wake driving,
  scheduler-facing task/hart/slice/stop/wake types, and `Phase1Scheduler`.

## Blocking Gaps

- Real reactor loop: current `run_until_idle` exits when no runnable task
  exists. There is no long-running idle/WFI/interrupt-driven kernel loop.
- Userspace runtime: no `request_userspace_run`, saved-register userspace
  dispatch, interesting-trap resolution, or trap-to-future handoff exists yet.
- Signal classification: `Interrupted` and `Killed` are API variants, but
  wait-adapt does not consult a thread-runtime interrupt source.
- AST and return-to-user delivery: `AstSlot` is only a placeholder.
- Hardware time: the HAL `TimeIf` surface exists, but reactor timeouts are
  still host-driven through `advance_time_to`.
- Bus/substrate integration: `tx-substrate::bus` now has typed declaration
  carriers: `WireEventSet`, `WireDeclaration<E>`, `DeclaredQueue<E>`, and
  `DeclaredPort<E>` validate fired bits and subscription interests against a
  declared carrier. Raw bus storage is `Arc` plus spin-locked subscriber state
  and can cross hart boundaries at the storage level. `WireRetirement` now
  records an epoch-fenced terminal/drain handshake for queue/port destruction.
  `SubscriptionGraph<N>` owns long-lived raw queue/port subscription tokens
  with generation-checked keys for epoll-style consumers, and later
  `DeclaredSubscriptionGraphKey<E>` helpers keep declared queue/port graph
  operations typed. `WireOwnerRetireFence` bridges newly terminal wire retire
  records into EBR-delayed owner-storage reclaim. Follow-up macro work
  generates typed readiness/lifecycle bit sets for queue/port declarations.
  This is prototype-ready for subsystem development where mock devices or
  reactor tasks update owner truth and fire wakes, but not production-complete
  for VFS/device/block runtime. Remaining production gaps are real subsystem
  migration to declared waits, trace subscriber/nop-patching runtime, concrete
  VFS/device/fs/block owners and owner manifests, final global epoll/fd
  teardown and spill/fanout policy,
  AP/shared-reactor replacement with the final per-hart production loop,
  VM/user/trap integration, and production lost-wake linearization. Follow-up
  reactor slices
  now add `DeclaredChannel<E>` over `DeclaredPort<E>` for typed declared-port
  waits and `DeclaredReadinessChannel<E>` over `DeclaredQueue<E>` for typed
  queue/readiness waits, while keeping the raw `Channel` / `Mask` path for
  completion and sync coordination internals.
- Cross-hart coordination: low-level AP boot and IPI send/ack primitives now
  exist for RV64 QEMU, reactor rendezvous storage is `Send + Sync`, RV64 pmap
  shootdown can use SBI RFENCE for online remote harts, and `Phase1Scheduler`
  now reports affinity-aware remote wake placement. Reactor dispatch state now
  marks the target hart `need_resched`, and the kernel runtime has a
  `SmpIf::send_ipi(..., IpiKind::Reschedule)` bridge with RV64 QEMU smoke
  coverage. RV64 QEMU also has a kernel-owned AP loop that wakes from a
  reschedule IPI, acknowledges pending SSIP state, consumes reactor
  `need_resched`, and drains a real shared-reactor runqueue. There is still no
  final per-hart reactor sharding, no kernel-managed shootdown fallback, and no
  production idle/timer loop.
- Thread drain/cancel: `TaskHandle` is a copyable id today; thread-runtime
  ownership and payload drain semantics remain to be implemented.

## Design Gaps To Close Before Freezing The API

- Choose one canonical wait shape. `REACTOR_v0` exposes
  `wait(channel, mask, protocol) -> Ready`, while Concepts/Bus also discuss
  `channel + condition + protocol -> ConditionTrue`. The implementation should
  pin where the recheck lives.
- Spell the lost-wake linearization rule for subscribe/recheck/park precisely.
  The docs require atomic register-or-recheck, but the current prose leaves
  room for unsafe check-then-subscribe implementations.
- Reconcile one-shot completion semantics: broadcast-like `done: AtomicBool`
  versus credit-consuming completion unless explicitly declared broadcast.
- Close API spelling for task drain/cancel, `yield_now`, AST queues, and
  synchronous coordination.

## Readiness Rating

Ready for subsystem design and step/script skeleton work: mostly.

Ready for executable subsystem development that blocks on real runtime waits,
signals, userspace scheduling, device/block IRQ completion across harts, or
cross-hart coordination: no, not yet.

Practical distance: the kernel-only cooperative reactor now has enough
mechanism for prototype waits, mock completions, HAL-clock smoke execution,
AP boot experiments, AP-local substrate initialization, low-level IPI
acknowledgement, scheduler-owned affinity placement, and a first
reactor-to-`SmpIf` reschedule bridge with AP-side shared-reactor runqueue
draining. It remains several slices from the full subsystem runtime. The next
useful sequence is concrete VFS/device owner implementations over the
owner-retire fence plus target-fd reverse-index teardown and global epoll table
integration on top of the bounded subscription graph, then timer/IRQ delivery
into the runtime loop and userspace-run/AST return-to-user integration once
trap/thread-runtime pieces exist.

## Verification

Subagents read the active reactor design docs, current `tx-reactor` code/tests,
and reactor progress records. The main session also ran:

```text
cargo test -p tx-reactor
cargo xtask qemu --target rv64-qemu --profile smoke --expect-sentinel --timeout-ms 15000
```

Result: current `tx-reactor` tests pass; the RV64 QEMU smoke prints the AP
online, RFENCE shootdown, IPI ack, reactor dispatch IPI, AP reactor loop,
AP runqueue, reactor task, and boot sentinels.
