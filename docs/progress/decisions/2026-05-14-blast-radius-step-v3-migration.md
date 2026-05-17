# Decision: BLAST_RADIUS Step Model v3 Migration — Completion ADR

**Date:** 2026-05-14
**Status:** decided

## Summary

The BLAST_RADIUS v3 migration (per `docs/Txv3/07_BLAST_RADIUS.md`) retired
the v4 step vocabulary and wired the v3 step model across all authored
crates. This ADR records the landing state and verification evidence.

## Completed Axes

### 1. v4 Vocabulary Retirement

All v4 identifiers removed from mainline code:

| Identifier | Pre-migration | Post-migration |
|---|---|---|
| `Blocked(WakeCarrier, InterestConditions)` | 16 sites | 0 |
| `Advanced(Progress)` | 0 sites (pre-cleaned) | 0 |
| `AdvancedThenBlocked(...)` | 0 sites | 0 |
| `read_wq` / `write_wq` | 0 sites | 0 |

Lint gate: `cargo xtask lint invariants v4-vocabulary` — ceiling 0,
current 0. ✅

### 2. StepOutcome Four-Variant Shape

`StepOutcome<T, P>` narrowed to four variants:

```rust
enum StepOutcome<T, P> {
    Continue { progress: P },
    Yield { progress: P, shape: YieldShape },
    Done(T),
    Err(Errno),
}
```

All 423 construction sites use the four-variant shape. ✅

### 3. StepOp Wrapping

69 `step_*` functions covered by 66 `impl StepOp` blocks across all
subsystems:

| Subsystem | Wraps |
|---|---|
| TTY | 11 ioctl + 6 core ops |
| VM | 7 ops |
| VFS | 5 ops |
| PageBacked | 6 ops |
| Process | 11 ops |
| Cred | 4 ops |
| Signal | 4 ops |
| Futex | 2 ops |
| Pipe | 3 ops |
| ThreadRuntime | 1 op |
| Mount | 0 (no step fns) |
| Other | 6 ops |

Lint gate: `cargo xtask lint invariants step-discipline` — ceiling 0,
current 0. ✅

### 4. bus/ Waker → TaskMailbox

All ~43 bare `Waker` sites in `tx-substrate/src/bus/` replaced by
`Weak<TaskMailbox>` + `WaitGeneration`. Subscriber notification via
`post_source_fired()` → `MailboxEvent::SourceFired`. Reactor wait
futures (`WaitFuture` / `DeclaredWaitFuture` / `DeclaredReadinessWaitFuture`)
updated to hold `Arc<TaskMailbox>` and poll mailbox events.

Compatibility: `_with_waker` bridge methods on `RawQueue`, `RawPort`,
`DeclaredQueue`, `DeclaredPort`, `SubscriptionGraph`, and their
subscription types preserve backward compatibility for integration tests.

Files changed: 9 bus files + 2 reactor files + 1 test file. ✅

### 5. step_v3 Scaffold (drive.rs)

`tx-scripts/src/drive.rs` fully wired:

```rust
pub async fn drive<S, I>(
    op: S,
    ctx: &mut ScriptCtx<I>,
    mode: DriveMode,
    mailbox: Option<&Arc<TaskMailbox>>,
    delegate_registry: Option<&DelegateRegistry>,
    timer_wheel: Option<&TimerWheel>,
) -> Result<S::Output, Errno>
```

Three `YieldShape` resolvers wired:
- `OnWaitSource` → TaskMailbox + fallback `wait_source::wait_on_token()`
- `OnAgent` → DelegateRegistry::install_request → mailbox park
- `OnTimer` → TimerWheel::install(PrimarySleep) → mailbox park

Tests: 8/8 passing (`tx-scripts/tests/drive.rs`). ✅

### 6. v4 → v5 Deprecation Annotations

`[deprecated by v5]` banners added to:
- `docs/design/02_execution/STEP_MODEL_v1.md`
- `docs/design/02_execution/THREAD_RUNTIME_v1.md`
- `docs/design/02_execution/SCHEDULER_v0.md`

These files are retained for historical reference; all active design
lives in `docs/Txv3/`. ✅

## Verification

| Gate | Result |
|---|---|
| `cargo xtask lint invariants all` | 10/10 green, all ceilings at 0 |
| `cargo test -p tx-subsystems --lib` | 623 passed, 0 failed |
| `cargo test -p tx-shims --lib` | 234 passed, 0 failed |
| `cargo test -p tx-scripts --lib` | 49 passed, 0 failed |
| `cargo test -p tx-kernel --lib` | 43 passed, 0 failed |
| `cargo test -p tx-scripts --test drive` | 8 passed, 0 failed |
| `cargo check --tests --workspace` | Clean (pre-existing `register_zone_for` conflict fixed) |

## Remaining Items

Per BLAST_RADIUS §7 Success Criteria, two canary use cases remain:

| Item | Status |
|---|---|
| userfaultfd E2E | ⬜ pending |
| AIO worker E2E | ⬜ pending |

These are tracked in `docs/progress/plans/` and do not block the
vocabulary retirement declaration.

## References

- `docs/Txv3/07_BLAST_RADIUS.md` §7 — Success criteria
- `docs/Txv3/03_STEP_MODEL_v2.md` — Canonical step model
- `docs/Txv3/02_INVARIANTS_v5.md` — Invariant families
- `docs/progress/decisions/2026-05-11-pr-3-wake-substrate-shape.md` — Wake substrate ADR
- `docs/progress/plans/2026-05-09-v3-tdd-migration.md` — Migration plan
