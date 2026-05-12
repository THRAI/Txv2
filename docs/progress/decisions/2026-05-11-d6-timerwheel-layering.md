# Decision D6: PR-7B follow-up — move `TimerWheel` down to `tx-substrate::wake`

**Date:** 2026-05-11
**Status:** decided
**Worker:** W-L (research-only)
**Companion:** [D4](2026-05-11-d4-bus-mailbox-layering.md) (template),
[PR-7B landing notes](../STATUS.md) entry
"v3 PR-7B OnAgent mailbox + timer-wheel integration LANDED",
[PR-7B option-(b) decision recorded in
`crates/tx-substrate/src/step_v3/agent.rs:786-799`]
**Refines:** the "Left for follow-ups (b)" bullet in the PR-7B
STATUS entry — that bullet flagged the move as a candidate for a
future ADR; this ADR is that ADR.

## 1. Problem statement

PR-7B (just landed) wired the timer-tick → substrate-state-machine
glue at the **reactor side** via two new entry points on
`crates/tx-reactor/src/timer.rs`:

```rust
TimerWheel::install_delegate_timeout(deadline, DelegateTokenId)
    -> TimerGuard
TimerWheel::fire_due_delegate_timeouts(now, &DelegateRegistry) -> usize
```

The `DelegateRegistry` lives in `tx-substrate::step_v3::agent`; the
`TimerWheel` and `TimerGuard` live in `tx-reactor::timer`. Because
**`tx-reactor` depends on `tx-substrate`** (and not the other way
around), the wheel can call into the registry but not vice versa.

The user-visible consequence shows up at the call site: a thread
that wants a `DelegateTimeout` on an `OnAgent` op holds **two**
guards in parallel —

```rust
let agent_guard: AgentTokenGuard<'_> =       // substrate
    registry.install_request(/* ... */);
let timer_guard: TimerGuard =                // reactor
    wheel.install_delegate_timeout(deadline, agent_guard.id());
// ... park ...
// resume: both guards drop together (or both forget()-ed together)
```

This is correct but awkward: the `AgentTokenGuard` *logically owns*
the timer registration (the timer exists solely to retire the
token), yet `AgentTokenGuard` cannot **carry** the `TimerGuard` as
a field because that would require importing `tx_reactor::TimerGuard`
from `tx-substrate` — the same back-edge that D4 fixed for
`TaskMailbox`.

D4's resolution: when the primitive's natural home is the lower
layer, **move the primitive down** and add a `pub use` shim at the
old location. This ADR applies the same playbook to `TimerWheel`.

## 2. Prerequisite survey — what blocks a clean move?

PR-7B's worker (W-H) called out two reactor-internal dependencies
that the public timer surface touches. Both are surfaced for
inspection here.

### 2.1 `SpinLock` (reactor-private) vs `SpinMutex` (substrate-public)

`crates/tx-reactor/src/spin_lock.rs` (58 LoC, `pub(crate)`) and
`crates/tx-substrate/src/sync.rs` `SpinMutex` (71 LoC, `pub`) are
**functionally identical**:

| Property | `tx_reactor::SpinLock` | `tx_substrate::SpinMutex` |
|---|---|---|
| Lock cell | `AtomicBool` | `AtomicBool` |
| Lock op | `compare_exchange(false, true, Acquire, Relaxed)` + `spin_loop` | identical |
| Unlock op | `store(false, Release)` in guard `Drop` | identical |
| `Send`/`Sync` | `Send + Sync` for `T: Send` | `Sync` for `T: Send` (Send auto) |
| Poisoning | none | none |
| Const-new | yes | yes |
| Debug | none | yes (formats inner via lock) |
| Visibility | `pub(crate)` | `pub` |

The two diverge only in two cosmetic places: `SpinMutex` adds a
`Debug` impl, and `SpinLock` has an explicit `unsafe impl Send`
where `SpinMutex` relies on auto-`Send`. Neither matters at the
call site — `TimerWheelState` is `Clone + Debug`-free internal
state.

**Recommendation: use `tx_substrate::SpinMutex` in the moved file
and delete the `SpinLock` import.** The reactor's other consumers
of `SpinLock` (`mailbox.rs`, `wait.rs`, `wait_source.rs`, etc.)
were converted to `tx_substrate::SpinMutex` as part of D4's
PR-3D-0 already; the only remaining `SpinLock` users in tree are
`timer.rs` (this move retires that one) plus whatever
reactor-internal code still imports it. **No `SpinLock` move is
required.**

### 2.2 `WaitOutcome` (reactor-internal) — used only by the internal `TimerQueue`

`crates/tx-reactor/src/wait.rs::WaitOutcome` is referenced from
`timer.rs` in exactly two places, both inside the **internal**
`DeadlineFuture`:

```
timer.rs:37   use crate::wait::WaitOutcome;
timer.rs:133  type Output = WaitOutcome;             // DeadlineFuture::poll
timer.rs:140  return Poll::Ready(WaitOutcome::TimedOut);
```

The `DeadlineFuture` is wholly internal to the (pre-PR-8)
`TimerQueue` — the layer that drives `WaitProtocol::*Timeout`
paths via `Waker`s. The **public** `TimerWheel` / `TimerGuard` /
`TimerToken` / `TimerGuardRole` surface (lines 180-524 of
`timer.rs`) **does not reference `WaitOutcome`** at all.

So the file naturally splits along the comment banner at
`timer.rs:180`:

```
// =========================================================================
// PR-8 public surface: TimerToken, TimerGuardRole, TimerWheel, TimerGuard.
// =========================================================================
```

Above the banner (lines 1-179, ~178 LoC, including the file header
docs that mention both layers) stays in `tx-reactor::timer`:
`TimerQueue`, `TimerQueueState`, `TimerWaiter`,
`InternalTimerToken`, `DeadlineFuture`. These continue to import
`WaitOutcome` and the reactor-private `SpinLock` (or migrate to
`SpinMutex` opportunistically).

Below the banner (lines 180-524, ~344 LoC) moves to substrate:
`TimerToken`, `TimerGuardRole`, `Entry`, `TimerWheelState`,
`TimerWheel`, `TimerGuard`. The PR-7B extensions
(`install_delegate_timeout`, `fire_due_delegate_timeouts`) move
naturally with the wheel — both reference `DelegateRegistry` and
`DelegateTokenId`, which already live in `tx-substrate::step_v3`.

**Neither prerequisite blocks the move. `SpinLock` is solvable
in-line (use `SpinMutex`); `WaitOutcome` does not appear in the
public surface so the file split is clean.**

## 3. Move scope — what relocates

### 3.1 Public types relocating to `tx-substrate`

| Type | Today | After D6 | Notes |
|---|---|---|---|
| `TimerToken` | `tx_reactor::timer` | `tx_substrate::wake::timer` | `pub use` shim in tx-reactor |
| `TimerGuardRole` | `tx_reactor::timer` | `tx_substrate::wake::timer` | `pub use` shim |
| `TimerWheel` | `tx_reactor::timer` | `tx_substrate::wake::timer` | `pub use` shim |
| `TimerWheel::install` | method | method | unchanged |
| `TimerWheel::install_delegate_timeout` | method | method | uses `DelegateRegistry` / `DelegateTokenId` (both already substrate-side) — relocates naturally |
| `TimerWheel::fire_due_delegate_timeouts` | method | method | calls `registry.mark_timed_out` directly from substrate — the **PR-7B reactor-side glue collapses to a same-crate call** post-move |
| `TimerWheel::armed_count` / `lookup` | method | method | unchanged |
| `TimerGuard` | `tx_reactor::timer` | `tx_substrate::wake::timer` | `pub use` shim |
| `TimerGuard::token` / `deadline` / `role` / `forget` | method | method | unchanged |

### 3.2 Types staying in `tx-reactor`

| Type | Reason |
|---|---|
| `TimerQueue` | `pub(crate)`, holds `Waker`s, drives `WaitProtocol::*Timeout` |
| `TimerQueueState` / `TimerWaiter` | internal to `TimerQueue` |
| `InternalTimerToken` | private; distinct from public `TimerToken` |
| `DeadlineFuture` | `pub(crate)`; uses `WaitOutcome` |
| `SpinLock` | reactor-private; remaining `TimerQueue` use only |

### 3.3 File layout

**Before:**

```
crates/tx-reactor/src/timer.rs                   524 LoC, two-layer file
```

**After:**

```
crates/tx-substrate/src/wake/timer.rs            ~344 LoC, public wheel + guard
crates/tx-reactor/src/timer.rs                   ~180 LoC, internal TimerQueue only
```

A `mod.rs`-only split (`crates/tx-substrate/src/wake/timer/{mod,wheel,guard}.rs`)
is **not** recommended: the public surface is small enough that
one file keeps it scannable. D4's wake/ submodule kept
`mailbox.rs` and `wait_source.rs` as flat files (538 LoC and 542
LoC respectively); a 344-LoC `wake/timer.rs` is consistent.

### 3.4 Shims preserved in `tx-reactor`

`crates/tx-reactor/src/lib.rs:49` today:

```rust
pub use timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};
```

After the move, this line becomes:

```rust
pub use tx_substrate::wake::timer::{
    TimerGuard, TimerGuardRole, TimerToken, TimerWheel,
};
```

(The `timer` module itself stays — it still hosts `TimerQueue`
and `DeadlineFuture` — but the public re-export shifts to a
substrate path. Workspace consumers of `tx_reactor::TimerWheel`
and friends resolve unchanged.)

## 4. Consumer touch-point survey

```
$ grep -rn 'tx_reactor::TimerWheel\|tx_reactor::TimerGuard\|tx_reactor::TimerToken\|tx_reactor::TimerGuardRole' crates/
crates/tx-substrate/src/step_v3/agent.rs    (doc-comment text only, no `use` clause)
crates/tx-substrate/src/step_v3/mod.rs      (doc-comment text only)

$ grep -rn 'use tx_reactor::.*Timer' crates/
crates/tx-reactor/tests/v3_timer_surface.rs:11    (reactor's own test)
crates/tx-reactor/tests/v3_pr7b_timer_routing.rs:27  (reactor's own test)

$ grep -rn 'use tx_reactor::timer' crates/
(no matches)
```

**Zero external (non-test, non-reactor) consumers** import these
types directly. The two test files inside `crates/tx-reactor/tests/`
import via the shim path (`use tx_reactor::{TimerWheel, ...}`) and
need no change post-move — the `pub use` re-export in `lib.rs`
keeps their import path valid.

This matches D4's precedent: PR-3D-0 found zero external
`tx_reactor::TaskMailbox` direct imports and the `pub use` shim
made the relocation an internal-crate diff.

## 5. Module location — `wake::timer` vs top-level `timer`

D4 chose `tx-substrate::wake` as the home for `TaskMailbox` and
`WaitSource` — the **wake substrate** as a coherent shelf. The
choice was justified by the architectural argument: bus,
generation, mailbox, and source share one layer of "object-owned
wait publication + task-owned wake delivery" and grouping them
makes the relation visible in the file tree.

Where does `TimerWheel` belong?

**Argument for `wake::timer`:**

1. Roles `PrimarySleep` and `DeadlineAbort` are *waits* — a thread
   parks against the wheel, the wheel fires a wake. The wake-fire
   path in PR-8B will fold into the same dispatch shape used by
   `TaskMailbox::post` (post a `MailboxEvent::TimerExpired { token,
   role }` or similar). Co-locating the wheel with the mailbox
   makes that join a local edit.
2. Role `DelegateTimeout` already calls `DelegateRegistry::mark_timed_out`,
   which exists to **publish a wake** (the routing extension PR-7B
   landed). The mailbox-event fan-out lives in `wake/mailbox.rs`.
   `wake/timer.rs` is the timer-tick origin of those same events.
3. Architectural narrative: "the wake substrate publishes wait
   readiness via sources, delivers wakes via mailboxes, and
   schedules deadline expiries via timers." All three are
   substrate-shelf primitives.

**Argument for top-level `tx_substrate::timer`:**

1. `TimerWheel` *does not yet* post `MailboxEvent`s. The PR-8 stub
   mechanics only track registrations; PR-8B will wire the fire
   path. Until then a top-level home doesn't pre-commit a coupling.
2. Some uses of the wheel are *not* wake-related — e.g. a
   diagnostic `armed_count()` for tests, or a future "periodic
   tick" feature that runs without a parked waiter. Putting the
   wheel inside `wake/` slightly misleads on those.
3. Symmetry with `tx-substrate::zone`, `tx-substrate::epoch`,
   `tx-substrate::page` — these are top-level substrate
   primitives, not nested under a category folder. `timer` fits
   that pattern.

**Decision: `tx-substrate::wake::timer`.** Two reasons close the
call:

(a) D4's argument that the wake substrate "calls itself the
substrate" — the wake/ submodule was chosen *because* mailbox,
source, and (now) timer are the three primitives the v3 step
model's `YieldShape::OnWaitSource` / `OnTimer` / `OnAgent` resolve
into. Grouping them keeps a one-to-one map between
`YieldShape::*` variants and `wake/*.rs` files.

(b) PR-8B's eventual wheel-fire path **will** publish
`MailboxEvent`s (the PR-7B stub `fire_due_delegate_timeouts`
already routes to `DelegateRegistry::mark_timed_out`, which posts
`MailboxEvent::Abort { reason: TimedOut }`). The coupling is real
and forward-looking; pretending it doesn't exist by hoisting
`timer` to top-level would force a later rename when PR-8B lands.

The cost of being wrong is low: if a non-wake use of the wheel
appears, we can promote it to top-level with another shim.

## 6. Recommended landing plan

Single worker, single PR, mechanical move + shim — patterned after
PR-3D-0 (D4's first phase).

### 6.1 PR-7C "move TimerWheel down" (proposed name)

| Step | Goal | Touches |
|---|---|---|
| 1 | Create `crates/tx-substrate/src/wake/timer.rs` containing `TimerToken`, `TimerGuardRole`, `Entry`, `TimerWheelState`, `TimerWheel`, `TimerGuard`. Use `tx_substrate::SpinMutex` (not the relocated `SpinLock`). Adjust internal doc-link anchors (`crate::wait_source::SubscriberId` → `super::wait_source::SubscriberId`). | 1 new file (~344 LoC) |
| 2 | Add `pub mod timer;` and `pub use timer::{TimerToken, TimerGuardRole, TimerWheel, TimerGuard};` to `crates/tx-substrate/src/wake/mod.rs`. | 1 file edit, +2 lines |
| 3 | Re-export at crate root in `crates/tx-substrate/src/lib.rs` (matching how D4 re-exports `TaskMailbox` / `WaitSource`). | 1 file edit, +1 line |
| 4 | In `crates/tx-reactor/src/timer.rs`, **delete** lines 180-524 (the public-surface section). Keep the file header (rewrite the docstring to describe only the `TimerQueue` internal layer). Delete `tx_substrate::step_v3::{DelegateRegistry, DelegateTokenId}` import (no longer used here). Keep `Deadline` import? No — `Deadline` was only used by the public surface; remove it too. | 1 file edit (~344 LoC removed) |
| 5 | In `crates/tx-reactor/src/lib.rs:49`, change `pub use timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};` to `pub use tx_substrate::wake::timer::{TimerGuard, TimerGuardRole, TimerToken, TimerWheel};`. | 1 file edit, ±1 line |
| 6 | Run `cargo check --workspace --tests`. Expected: zero import-path rewrites required outside the four files touched above. The two test files in `crates/tx-reactor/tests/` resolve via the shim and continue passing. | verification |

### 6.2 Estimate

| Phase | Days | Confidence |
|---|---|---|
| ADR (this document) | 0.5 | done |
| Move + shim (PR-7C) | 1.0 | high — mechanical, D4 precedent |
| Consumer audit (post-move grep + `cargo check --workspace --tests`) | 0.5 | high — 0 expected churn |
| **Total** | **2.0d** | matches W-H estimate (0.5 + 1 + 0.5) |

Parallel to PR-8B (the wheel-mechanics PR) — no schedule conflict
because PR-7C is a pure relocation and PR-8B works on the wheel's
internals. PR-8B may even prefer to land **after** PR-7C so that
its primary fire path is written against the substrate location
directly (avoiding a follow-up move).

### 6.3 Mergeability

PR-7C is **mergeable independently** of any follow-up. After it
lands:

- `AgentTokenGuard` can be upgraded in a separate PR (§7) to own
  a `TimerGuard` field directly.
- PR-8B's wheel mechanics (fire path, `MailboxEvent` posting) can
  proceed without further reshuffling.
- PR-7C itself changes no behaviour and no public type identity —
  it is the safest possible step.

## 7. Follow-up sketch — `AgentTokenGuard` gains a `TimerGuard` field

**Not** part of D6. Recorded here so the eventual PR has a
starting shape.

After PR-7C lands, the `AgentTokenGuard` definition at
`crates/tx-substrate/src/step_v3/agent.rs:800` can grow an
optional timer field:

```rust
use crate::wake::timer::TimerGuard;

#[must_use = "drop the guard to release the delegation; binding to _ may cancel immediately"]
pub struct AgentTokenGuard<'a> {
    registry: Option<&'a DelegateRegistry>,
    id: DelegateTokenId,
    drop_policy: TokenDropPolicy,
    cancel_policy: AgentCancelPolicy,
    /// `Some` if the call site installed a `DelegateTimeout`
    /// timer alongside the registration. Dropped before the
    /// registry CAS so the timer-tick path cannot fire after
    /// the guard has released the token.
    timer: Option<TimerGuard>,
}

impl<'a> Drop for AgentTokenGuard<'a> {
    fn drop(&mut self) {
        // Drop timer FIRST so a concurrent fire_due_delegate_timeouts
        // walk observes the timer entry retired before we attempt
        // the mark_canceled CAS. Order matters: timer drop →
        // wheel.cancel(token) → CAS on the token state.
        let _ = self.timer.take(); // drops TimerGuard → cancels wheel entry
        if let Some(registry) = self.registry.take() {
            match self.drop_policy {
                TokenDropPolicy::CancelOnDrop => {
                    let _ = registry.mark_canceled(self.id);
                }
                TokenDropPolicy::Abandon => { /* unchanged */ }
            }
        }
    }
}
```

The constructor (`DelegateRegistry::install_request_with_deadline`,
or a new variant) takes a `Deadline` plus a `&TimerWheel` and
installs both atomically:

```rust
pub fn install_request_with_deadline(
    &'a self,
    deadline: Deadline,
    wheel: &TimerWheel,
    mailbox: Weak<TaskMailbox>,
    /* drop/cancel policies, request payload, ... */
) -> AgentTokenGuard<'a> {
    let id = /* allocate token id, install slot with mailbox */;
    let timer = wheel.install_delegate_timeout(deadline, id);
    AgentTokenGuard { registry: Some(self), id, /* ... */, timer: Some(timer) }
}
```

**Drop coordinates with token-state CAS via drop ordering: the
`TimerGuard` drop runs before the `mark_canceled` CAS, so any
in-flight `fire_due_delegate_timeouts` walk either (a) already
won the CAS — in which case `mark_canceled` returns `LateNoOp`
and the wheel entry was already retired by the fire path — or
(b) lost the race to find the entry, retired-by-drop, and the
CAS in `mark_canceled` is the unambiguous last writer.** DTOK-3
(reply-vs-timeout race determinism) carries through unchanged
because the registry CAS is still the serialization point; the
timer-guard drop is a pure cleanup of the wheel entry.

**Out of scope for D6.** This sketch belongs to a PR-7D (or
similar) that lands *after* PR-7C. It is sketched here so D6
reviewers see the destination, not to commit to that exact
signature.

## 8. Relationship to existing ADRs

- **[D4](2026-05-11-d4-bus-mailbox-layering.md)** — direct
  template. D6 reuses D4's reasoning verbatim with `TimerWheel`
  swapped for `TaskMailbox`. Both moves: (i) zero external direct
  imports, (ii) `pub use` shim at the old crate root,
  (iii) substrate gains a primitive that already only depended on
  substrate-or-below types, (iv) one-day PR with `cargo check
  --workspace` clean as the acceptance gate.

- **[D2](2026-05-11-d2-waitsource-coexists-with-rawport.md)** —
  not directly involved (D2 governs `RawPort` vs `WaitSource`
  coexistence; D6 is a pure relocation with no API change). D2's
  framing of "primitives migrate down one at a time" is the
  precedent D6 follows.

- **PR-7B landing entry in STATUS.md** ("Left for follow-ups (b)")
  — D6 *is* the ADR that bullet flagged. The PR-7B status entry
  does not need editing; this ADR's existence is the closure.

- **[PR-3 wake-substrate shape](2026-05-11-pr-3-wake-substrate-shape.md)**
  — the architectural ADR that placed `TaskMailbox` and
  `WaitSource` in `tx-substrate::wake`. D6 extends that shelf to
  `TimerWheel`. The PR-3 ADR's hierarchy table at
  §"Final relationship" gains a row:

  ```
  TimerWheel              role-tagged deadline registry    tx-substrate::wake (was tx-reactor)
  ```

  The PR-3 ADR does not need editing — D6 is the extension
  recorded by reference.
