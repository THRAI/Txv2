# Reactor architecture survey

Date: 2026-07-13
Scope: current tx-reactor runtime, wake/fanout, syscall drive bridge, and SMP/timer boundaries.

## Findings

The current reactor is a cooperative, stackless Future executor. A task is
stored in a generation-checked `TaskTable`; the scheduler owns runnable
placement per hart; the reactor removes the future from the table, polls it
outside reactor locks, and commits `Ready`, `Pending`, preemption, or slice
expiry afterward. A plain `Pending` becomes `Parked` unless a wake arrived
during the poll, which closes the poll-to-park lost-wake window.

The wake path is split into an object-side publication path and a task-side
scheduler path:

```text
semantic object / RawQueue / RawPort
  -> WaitSource subscribers (Weak<TaskMailbox>)
  -> MailboxEvent / mailbox coalescing
  -> Waker / TaskWakeState wake bit
  -> reactor wake queue or owner wake inbox
  -> parked task becomes runnable
  -> per-hart scheduler queue
  -> future poll and fresh semantic observation
```

The mailbox is an event inbox, not the runnable queue, and a wake is only a
retry hint. Generation, source, and interest matching reject stale or
unrelated events. However, `TaskWakeState::wake` still appends one task id for
every wake even when the atomic wake bit is already set. The effect is mostly
extra queue traffic, but it makes fanout cost and overflow accounting harder to
reason about.

Syscall integration is only partially converged. Immediate syscalls bypass the
reactor; regular asynchronous paths use `StepOp + drive()` and resolve
`OnWaitSource`/`OnTimer` through mailbox and timer registration. `epoll_wait`
and the legacy wait helper still contain private loops and wait bridges. The
drive path uses `ResumeOutcome`, while the reactor wait API exposes
`WaitOutcome`, so the same waiting concept currently has two integration
surfaces.

SMP currently provides owner-aware placement, local queues, wake inboxes, and
remote reschedule IPI routing. The checked implementation does not establish a
fully closed work-stealing protocol matching every requirement in
`SCHED-SMP_v1`; real concurrent steal, lock-and-recheck races, and hardware IPI
behavior need stronger evidence. The timer path is also transitional: the old
reactor timer module is gone, but substrate timer types and compatibility
conversions remain while `tx-time` is extracted.

## Main problems

1. The current runtime has two near-duplicate poll/commit paths, which can
   drift semantically.
2. Wake delivery has multiple routes (direct owner post and deferred wake
   inbox), with different latency and instrumentation behavior.
3. Wait correctness is strongest in prepared registrations; the legacy wait
   future does not visibly re-check the semantic predicate itself.
4. Subscriber fanout is a linear scan under the source lock, so producer
   latency grows with reader count.
5. `TaskWakeState` does not transition-filter wake-queue insertion.
6. `Reactor` and task-local context disagree on hart capacity (64 versus 8).
7. `StepOp`/`drive`, `WaitOutcome`, epoll's private loop, and timer migration
   have not yet converged on one stable boundary.

## Verification and next action

This was a read-only fanout survey using five bounded explorer prompts and
current line-numbered source inspection. Relevant focused tests are recorded
in `docs/progress/research/2026-07-13-reactor-fanout-readers-audit.md`; this
survey did not modify Rust source. Next action is to define the desired single
wait bridge and typed wake-post result before optimizing subscriber storage or
changing scheduler queues. Outstanding blockers are the dirty checkout and
the absence of a complete real-SMP/concurrent-steal proof.
