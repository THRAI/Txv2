# Reactor Third-Wave Worker Audit

**Date:** 2026-04-30

**Scope:** third-wave reactor worker merge from
`docs/progress/plans/2026-04-30-reactor-third-wave-dispatch.json`.

## Worker Results

- Yield-now added `YieldNow` and public `yield_now()` to `tx-reactor`. The
  helper self-wakes on first poll, returns `Pending`, and completes on the next
  poll without changing scheduler policy or task status semantics.
- AST poll-boundary integration added per-task `AstSlot` storage, key-checked
  queue/consume APIs, and pre-poll AST batch consumption. Tests cover
  task-local coalescing, poll-boundary consumption, terminal-task rejection, and
  stale-handle rejection.
- CoreInit HAL-clock wiring changed the existing reactor smoke task to use
  `Reactor::run_until_idle_with_clock`, sourcing time from `P::read_ns()` and
  programming/canceling deadlines through `P::set_deadline_ns()` /
  `P::cancel_deadline()`.
- Bus wire hardening added first-slice terminal/subscriber reporting through
  `RawWireError`, `RawSubscriptionError`, and `RawSubscriptionState`, plus
  `terminate` and `try_*` APIs for `RawQueue` and `RawPort`.

## Coordinator Audit Decisions

- Accepted `yield_now` as a pure cooperative future. It creates a poll boundary
  through the existing task waker path and does not add scheduler policy.
- Accepted AST consumption as mechanism-only. The reactor stores and consumes
  batches, but signal selection, handler frames, `ThreadPayload`, and
  return-to-userspace delivery remain outside this pass.
- Accepted CoreInit HAL-clock wiring because it preserves the finite smoke path
  and sentinel order. It does not introduce a permanent WFI loop.
- Accepted bus hardening as first-slice substrate work. Compatibility methods
  remain available, fallible methods expose terminal/unsubscribed state, and
  `tx-substrate` still has no `tx-reactor` dependency.
- Coordinator fixed the `tx-reactor` crate overview after AST queues became
  real mechanism rather than a missing placeholder.

## Production Runtime Boundary

The merged surface is enough for prototype kernel-only flows where a reactor
task or mock device completion updates owner truth and fires a wake. It is not
enough for the full production VFS/device/block runtime, and it is not enough
for real IRQ-driven block I/O across harts.

The current reactor wait path still uses first-slice raw `u64` masks/events,
but the bus now has typed declaration wrappers over those raw carriers.
`WireEventSet`, `WireDeclaration<E>`, `DeclaredQueue<E>`, and
`DeclaredPort<E>` let subsystem-facing code validate fired bits and
subscription interests against one declared carrier type. A follow-up SMP pass
also replaced the raw bus subscriber storage with `Arc` plus a spin lock, so
the storage is no longer `Rc<RefCell<...>>`. A follow-up bus pass added
`WireRetirement`, an epoch-fenced terminal/drain record for queue/port
destruction. Another follow-up added `StaticRawQueue` / `StaticRawPort` backing
storage for static device tables. Later macro and trace slices now generate
typed readiness/lifecycle bit sets for queue/port declarations and typed
`RawTrace<P>` payload structs. The bus still has no concrete VFS/device owner
implementations over embedded wires, trace subscriber/nop-patching runtime,
target-fd reverse-index teardown, final global epoll table integration, or
spill/fanout policy. A bounded `SubscriptionGraph<N>` now owns long-lived raw
queue/port subscriptions behind generation-checked keys for epoll-style
consumers, has a ready/terminal scan plus explicit graph clear teardown, and
`WireOwnerRetireFence`
now bridges wire retire records into EBR-delayed owner-storage reclaim.
A follow-up SMP pass added an AP-side WFI loop and serialized shared-reactor
runqueue smoke, but CoreInit still lacks the production timer/IRQ/device
runtime loop. Treat wire-destruction/static-device hardening and the later
kernel runtime loop as prerequisites before claiming VFS/device/block runtime
readiness.

## Residual Gaps

- Userspace-run remains deferred. It still crosses saved-register trap return,
  `ThreadPayload.regs`, interesting-trap resolution, and AST delivery policy.
- AST batches are consumed and observable for tests, but no thread-runtime
  consumer interprets them yet.
- Bus terminal behavior still delegates to the raw first-slice carriers.
  Typed declaration wrappers now exist for `RawQueue` and `RawPort`, but
  trace subscriber/nop-patching runtime, target-fd reverse-index teardown,
  global epoll table integration, and concrete VFS/device owner implementations
  over embedded wire reclamation remain later.
- CoreInit now has AP-side WFI/reschedule smoke and shared-reactor runqueue
  polling, but the production timer/IRQ/device idle loop remains a later
  kernel/runtime slice.
- Real SMP shootdown coordination remains later; the reactor-local
  `SyncRendezvous` has not been bridged into substrate or HAL shootdown paths.

## Verification

Fresh verification after coordinator audit:

- `cargo test -p tx-reactor --test yield_now` (3 tests)
- `cargo test -p tx-reactor --test ast_runtime` (3 tests)
- `cargo test -p tx-reactor --test wait_bus` (5 tests)
- `cargo test -p tx-substrate` (50 tests)
- `cargo test -p tx-reactor` (71 tests)
- `cargo check -p tx-kernel-riscv64-qemu-virt --target riscv64gc-unknown-none-elf`
- `cargo fmt --check`
- `cargo xtask lint arch`
- `cargo xtask lint unused`
- `cargo xtask lint docs`
- `cargo xtask progress validate`
- `cargo xtask ci` (11 passed, 0 skipped, 0 failed)
- `git diff --check`
