# Reactor fanout/readers audit

Date: 2026-07-13
Scope: current tx-reactor wake, wait-source, mailbox, and SMP owner-routing paths.

## Conclusion

The current implementation does not have a separate reactor-level fanout-reader
pool. Fanout is object-owned: `WaitSource` stores a linear `Vec<Subscriber>` of
`Weak<TaskMailbox>` rows, filters by interest, and posts one `SourceFired` event
per matching mailbox. The reactor then turns mailbox publication into runnable
placement. Readers remain responsible for re-observing semantic truth after a
wake.

## Current path

`semantic object -> WaitSource::notify* -> TaskMailbox::post* -> waker/wake
inbox -> reactor owner route -> per-hart scheduler queue -> future poll ->
fresh semantic observation`.

Important implementation anchors:

- `crates/tx-substrate/src/wake/wait_source.rs:75-257,482-515`: subscriber
  rows, interest filtering, dead-weak cleanup, pending-mask delivery, and
  owner-aware posting.
- `crates/tx-substrate/src/wake/mailbox.rs:310-563`: bounded mailbox, source
  event coalescing, overflow latch, scheduler-hint latching, and registered
  waker.
- `crates/tx-reactor/src/runtime.rs:737-795,871-945`: owner lookup, parked to
  runnable transition, placement, remote IPI, and wake-inbox draining.
- `crates/tx-reactor/src/runtime.rs:986-1132`: poll loop and the distinction
  between a parked `Pending`, a self-yield, a wake during poll, and a completed
  task.
- `crates/tx-reactor/src/task.rs:174-360`: generation-checked task slots,
  mailbox ownership, and pending-poll commit.

## What works

- Lost-wake protection exists through `PreparedWaitRegistration::install_if`
  and the source pending mask.
- Wait events carry source and wait generation; `ActiveWait` rejects stale or
  unrelated events.
- Mailbox source events coalesce by `(generation, source)` and OR interests.
- Scheduler placement deduplicates runnable state even when several wake
  notifications arrive.
- Owner-aware source delivery can route directly to the current hart owner;
  fallback paths drain task wake ids and local wake inboxes.
- Focused tests pass for wait recheck, cross-hart wait delivery, source
  unsubscribe, duplicate wake coalescing, and substrate bus fanout.

## Problems and risks

1. **Fanout is O(N) under one source lock.** Every notification scans the full
   subscriber vector and upgrades weak mailboxes while holding the source lock.
   A pipe, tty, or shared readiness source with many readers therefore couples
   producer latency to subscriber count. `SubscriptionGraph` does not solve
   this hot path; it is a separate bounded long-lived bus structure.

2. **There are two wake-routing paths.** Producers with reactor context use
   `notify_with_owner_post`; generic producers enqueue into the task wake inbox
   and the reactor later resolves ownership. This is functionally useful, but
   makes latency, ordering, and instrumentation dependent on which producer
   adapter was selected.

3. **The waker path still pushes one task id per wake.**
   `TaskWakeState::wake` sets an atomic bit but unconditionally pushes the task
   id into the shared queue. The later task-table/scheduler checks remove most
   duplicate effects, but duplicate queue traffic and lock contention remain.

4. **Mailbox post has an overloaded boolean result.** `post_with_scheduler_hint`
   returns `false` both when a `SourceFired` event was coalesced and when the
   bounded mailbox overflowed. Owner-aware callers cannot distinguish harmless
   deduplication from a delivery failure, which weakens overflow accounting and
   direct-routing diagnostics.

5. **Owner routing is not fully closed against the documented SMP protocol.**
   The implementation reads the mailbox owner and then performs task-table and
   scheduler transitions, while the active SMP document requires destination
   queue locking plus an in-lock owner recheck to close wake-vs-steal races.
   Existing tests cover remote wake and stealing, but the code/document audit
   still identifies the lock-and-retry protocol as incomplete.

6. **The reactor is still Phase-1 policy.** Queue classes, hints, aging, and
   remaining-budget heuristics provide useful behavior, but there is no fair
   virtual-time/deadline model. A signal-priority scheduler test currently
   fails in the dirty checkout, so the hint policy is not fully stable.

7. **Userspace rendezvous is a single-slot side channel.**
   `UserspaceRunSlot` serializes one active request and stores one waker. That is
   appropriate for the current thread-entry shell, but it is not a general
   fanout reader mechanism and has no direct coverage for multiple concurrent
   userspace-entry consumers.

## Verification

Passed in this checkout:

- `cargo test -p tx-reactor --test wait_bus` - 14 passed.
- `cargo test -p tx-substrate --test bus` - 28 passed.
- Focused duplicate-wake tests in `reactor_smoke` and `scheduler` passed.

The broader suites are not clean in the current dirty checkout:

- `cargo test -p tx-reactor --test reactor_smoke` - 3 failures in delegate
  timeout/agent-died and mixed-producer setup paths.
- `cargo test -p tx-reactor --test scheduler` - 1 failure in
  `signal_delivery_hint_prioritizes_already_queued_userspace_task`.

These failures were observed only; no reactor source was edited by this audit.

## Recommended next step

First define a typed post result such as `Enqueued | Coalesced | Overflowed`
and make the wake doorbell transition-sensitive, so only the first wake in an
unobserved batch enters the reactor inbox. Then add a focused multi-reader
benchmark/test matrix for 1, 2, 16, and 64 subscribers, including concurrent
notify/unregister and cross-hart migration. Only after those measurements
should the subscriber storage move from a vector to a sharded/indexed design.
