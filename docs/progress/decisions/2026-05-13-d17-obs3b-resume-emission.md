# D17: OBS-3b Resume Emission — Mechanical vs. Architectural

**Date:** 2026-05-13
**Status:** Decision
**Author:** Investigation agent
**Txdoc:** `OBS-V1-MIGRATION-1` §OBS-3b, `txdoc:OBS-V1-OPEN-1` §15.1

---

## 1. Problem Statement

Perfetto flow visualization requires a matched producer/consumer pair: the producer instant
(`WaitSourceNotify`) records which hart fired a `WaitSource` and carries
`(task_id_low, wait_generation_low)` material; the consumer instant (`Resume`) records that the
parked task re-entered the run queue and carries the same material. The daemon hashes both into the
same `flow_id` via `compute_flow_id(task_id, wait_gen, FlowKind::SourceWake, boot_id)`, producing
a Perfetto flow arrow between them.

Today, OBS-4 landed the producer side: `WaitSource::notify_emit` emits one
`PayloadWaitSourceNotify` per woken task (`wake/wait_source.rs:224`). The daemon's
`push_wait_source_notify` and `push_resume` are structurally complete
(`perfetto/writer.rs:281,311`) and marked `#[allow(dead_code)]` — waiting only for kernel events.
Without `Resume` records, Perfetto sees producer instants with no matching terminating flow: the
"why did this task unpark?" arrow is absent from every trace.

`Resume` records answer: at the moment a parked task transitions to runnable, emit one
`TxTraceKind::Instant` / `TxPayloadTag::Resume` record carrying `(resume_kind, abort_reason,
object_id_low, wait_generation)`. Together with the producer record, the daemon draws the flow arrow.

---

## 2. Reactor State Inventory

The convergence point where OBS-3b must emit is the `Yield` arm of `drive`
(`crates/tx-scripts/src/drive.rs:169–175`), specifically after `yield_resolve` returns `None`
(wait completed, loop continues). That arm receives `shape: YieldShape` from `StepOutcome::Yield`.

| Field needed | Available at the Yield arm today? | Evidence |
|---|---|---|
| `task_id_low` | **No — zero-filled** | `drive.rs:101` sets `task_id_low: 0` in `PayloadDriveBegin`; same deferral applies to Resume. Threading task_id through `ScriptCtx<I>` is labelled "OBS-4 / later phase". |
| `wait_generation` | **Yes, in `YieldShape::OnWaitSource`** | `step_v3/mod.rs:126–128` — `YieldShape::OnWaitSource { source: WaitSourceId, interests: InterestMask }`. The generation is **not** in the shape itself; it is captured in `ActiveWait` at the call site that builds the `yield_resolve` closure. That closure holds `WaitGeneration` from `TaskMailbox::next_generation()`. It is in scope at the `drive` call site. |
| `wait_source_id` | **Yes** | `YieldShape::OnWaitSource { source, .. }` — `source` is a `WaitSourceId` with `.raw() -> u64`. |
| `flow_kind` | **Yes (trivial)** | Only `FlowKind::SourceWake` (value 1) for `OnWaitSource` yields; `OnAgent` / `OnTimer` are separate enum arms. |
| `resume_kind` | **No** | `PayloadResume::resume_kind` encodes why the wait ended (Retry / WithReply / TimerExpired / Aborted). `yield_resolve` returns `None` for "continue" but the actual reason (source fired vs. signal vs. timeout) is embedded inside the closure — not threaded back to `drive`. |

Summary: the fields needed for flow reconstruction (`wait_source_id`, `wait_generation`) are
accessible at the convergence point, but two of them require plumbing that does not yet exist.
`task_id_low` has been explicitly deferred (same note appears in `notify_emit`). `resume_kind` has
no return-value path from `yield_resolve`.

---

## 3. Convergence Point Identification

The spec (`08_OBSERVATION_v1.md §6`, table row "Resume") places OBS-3b at
`tx-scripts::drive`, in the `wait_active` resume classification path — i.e., just after
`yield_resolve` returns `None`. In the current code:

```rust
// crates/tx-scripts/src/drive.rs  lines 169–175
StepOutcome::Yield { shape, .. } => {
    // L3 yield/resume hooks will be inserted here in OBS-3b.
    // For OBS-3a the yield_resolve callback decides the wait.
    if let Some(errno) = yield_resolve(&shape, ctx) {
        break DriveOutcome::Err(errno);
    }
    // ← OBS-3b Resume emit lands here, after yield_resolve returns None
}
```

The `yield_resolve` callback is an `FnMut(&YieldShape, &mut ScriptCtx<I>) -> Option<Errno>`.
It returns `None` to signal "wait resolved, retry the step." At that return point the closure still
holds `ActiveWait` state (the generation and source that were used to register with the
`WaitSource`). The `drive` function sees only the `YieldShape`; everything else is opaque inside
the closure.

There is a single convergence point. There are no alternative paths through which a task can
resume from an `OnWaitSource` wait that bypass this Yield arm.

---

## 4. Classification: Mechanical vs. Architectural

**This is architectural — the `yield_resolve` closure must be extended to return wake-context metadata before OBS-3b can emit a complete `Resume` record.**

The reactor is structurally real: `Phase1Scheduler`, `TaskTable`, and the `drain_wakes` /
`run_until_idle` loop in `runtime.rs` are implemented and exercised. Parking and wake delivery work
via `TaskWakeState` (line 252 of `task.rs`, `drain_wakes` iterates and sets `Runnable`). So the
block is not that the reactor is a stub; it is that the *callsite contract* of `yield_resolve`
carries no outbound information.

Two pieces of state are needed:

1. **`wait_generation`** — the `WaitGeneration` that the wait was registered under. It lives in the
   `ActiveWait` inside the closure; `drive` never sees it.
2. **`resume_kind`** — whether the wakeup was a source fire, a signal, or a timeout.

Both require the `yield_resolve` signature to evolve from:
```rust
FnMut(&YieldShape, &mut ScriptCtx<I>) -> Option<Errno>
```
to something that also returns wake-context metadata (e.g., a new `YieldResolved` struct).

`task_id_low` is also absent, but that is an acknowledged parallel deferral (same comment in
`notify_emit` at `wait_source.rs:249`) and is not a blocker specific to OBS-3b.

---

## 5. If Mechanical: Implementation Plan

Not applicable — classification is architectural. But the implementation sketch for when the
architectural prerequisite lands:

**Files to touch:**

- `crates/tx-scripts/src/drive.rs` — emit `Instant(resume)` + `PayloadResume` after
  `yield_resolve` returns `None`, using state returned by the revised closure.
- `crates/tx-observe/src/encode.rs` — add `encode_resume` / `resume_tag()`. The struct
  `PayloadResume` already exists in `tx-observe-types` (`payload.rs:198–206`).

**Encoding helper to add** (`crates/tx-observe/src/encode.rs`):

```rust
// Wire layout per §8.4: size = 16
#[inline]
pub fn encode_resume(p: &PayloadResume) -> ([u8; 16], u16) {
    let mut buf = [0u8; 16];
    write_u8(&mut buf, 0, p.resume_kind);
    write_u8(&mut buf, 1, p.abort_reason);
    // _pad at 2-3 stays 0
    write_u32_le(&mut buf, 4, p.object_id_low);
    write_u64_le(&mut buf, 8, p.wait_generation);
    (buf, core::mem::size_of::<PayloadResume>() as u16)
}
pub const fn resume_tag() -> TxPayloadTag { TxPayloadTag::Resume }
```

**Test plan:** extend the existing pipe-EOF smoke test to assert that `Resume` records appear in the
ring after a `WaitSource::notify_emit`, and that the daemon `compute_flow_id` produces matching IDs
for producer/consumer pairs.

**Estimated blast radius:** 3 files. Low. The daemon's `push_resume` is already implemented and gated behind `#[allow(dead_code)]`.

---

## 6. If Architectural: Blocker Enumeration

### Reactor primitives that need to expose new state

The `yield_resolve` closure signature needs to return the wake-context. The minimal change is a
new `YieldResolved` struct:

```rust
// Proposed, in tx-scripts::drive or tx-substrate::step_v3
pub struct YieldResolved {
    pub resume_kind: u8,         // 0=Retry, 1=WithReply, 2=TimerExpired, 3=Aborted
    pub abort_reason: u8,        // valid if resume_kind == 3
    pub wait_generation: WaitGeneration,
    pub source_id: WaitSourceId, // 0 for non-OnWaitSource
}
```

The `yield_resolve` signature becomes:
```rust
FnMut(&YieldShape, &mut ScriptCtx<I>) -> Option<(YieldResolved, Errno)>
                                    // None = resolved; Some = abort with errno
```

All 92+ call sites that pass a `yield_resolve` closure (`tx-shims`, `tx-scripts`, `tx-fs`,
`tx-subsystems`) must be updated to return `YieldResolved` alongside the current `Option<Errno>`.

### Who owns the reactor design

The v3 reactor concept is specified in `docs/Txv3/Reactor_concept_v5_RefactorSpec v4.md`
(referenced throughout `08_OBSERVATION_v1.md` as the `WaitGeneration / TaskMailbox / WaitSource`
spec). The scheduler is in `crates/tx-reactor/src/scheduler.rs`. The drive contract is in
`crates/tx-scripts/src/drive.rs`. No separate `SCHEDULER_v0.md` exists in `docs/Txv3/`; the
closest is the `docs/design/02_execution/SCHEDULER_v0.md` referenced by `08_OBSERVATION_v1.md §1`.

### Minimum viable reactor primitive that unblocks OBS-3b

The minimum is the `YieldResolved` return value above: no scheduler changes required, no new
run-queue primitives, no new mailbox fields. The generation is already present in the closure's
`ActiveWait`; the change is purely to thread it back out.

### Deferral marker

`08_OBSERVATION_v1.md §15.1`:

> 15.1. Reactor parking in `drive::AcceptOutcome::Resolve`. L3 Yield/Resume hook code lands in
> OBS-3b but does not fire until reactor parking lands.

And `§16` migration table: OBS-3b is "Low — code compiles but inert until reactor parking."

---

## 7. Recommendation

**(b) Defer OBS-3b until the `yield_resolve` signature is extended to return `YieldResolved`.**

The reactor is real and functional; it is not a stub. But the `drive` convergence point lacks an
outbound channel for wake-context metadata — specifically `wait_generation` and `resume_kind`. The
`PayloadResume` struct is defined, the daemon's `push_resume` is implemented, and `encode_resume`
is trivial to add. What's missing is the `yield_resolve → YieldResolved` plumbing change, which
touches 92+ call sites. That change is a PR in its own right and must land first. Flag as
ARCH-3 review: the change is mechanical in isolation but wide in blast radius, and the observation
doc (`§15.1`) already records it as a known deferral. Once the plumbing lands, OBS-3b becomes a
two-file PR with high confidence and zero semantic risk.

---

## 8. Open Questions

1. **`task_id_low` threading.** Both `notify_emit` and `PayloadDriveBegin` defer `task_id_low` to a
   later phase. The daemon's `compute_flow_id` takes `task_id: u32` as input. Until task_id is
   threaded, the producer and consumer both emit `task_id_low = 0`, so flow IDs will hash to the
   same bucket for all tasks on the same `(wait_gen, boot_id)`. This could produce false flow arrows
   in multi-task traces. Whether `task_id_low = 0` is acceptable for an initial OBS-3b landing (with
   a known limitation) could not be resolved from read-only inspection.

2. **`resume_kind` discriminant source.** After `yield_resolve` returns, the reason for the wake
   (source fired vs. signal interrupt) is currently implicit: `None` means "resolved." The proposed
   `YieldResolved` shape assumes callers can populate `resume_kind` from their interrupt/signal
   state at the time of return. Whether all 92 call sites can supply this cheaply (most use
   `NoInterrupts`) was not verified.

3. **Span pairing discipline.** The spec shows `SpanBegin(yield.<shape>)` before yield and
   `SpanEnd` after resume (`08_OBSERVATION_v1.md §6`, table row "Yield begin"). The `drive_span`
   is already tracked; a yield-sub-span would need its own `SpanId` stored across the `yield_resolve`
   boundary. Whether this requires state in the `yield_resolve` closure or a new field in `drive` was
   not fully traced.
