# Decision D4: PR-3D bus / mailbox layering — move wake substrate down

**Date:** 2026-05-11
**Status:** decided
**Worker:** W-C (research-only)
**Companion:** [D2](2026-05-11-d2-waitsource-coexists-with-rawport.md),
[PR-3 shape ADR](2026-05-11-pr-3-wake-substrate-shape.md)
**Supersedes:** the "Phase PR-3D" sequence in
[`2026-05-11-pr-3-wake-substrate-shape.md`](2026-05-11-pr-3-wake-substrate-shape.md)
is **refined**, not replaced — that ADR's four phases were written before
the layering blocker surfaced. The phase plan in §6 below takes priority
for the actual PR-3D landing.

## 1. Problem statement

PR-3A/3B/3C landed the wake-substrate primitives in **`tx-reactor`**:

```
crates/tx-reactor/src/mailbox.rs       TaskMailbox, WaitGeneration,
                                       MailboxEvent, ActiveWait,
                                       MAILBOX_QUEUE_BOUND
crates/tx-reactor/src/wait_source.rs   WaitSource, SubscriberId,
                                       PreparedWaitRegistration,
                                       WaitRegistrationGuard
```

PR-3D (per `docs/Txv3/07_BLAST_RADIUS.md` §4 row F, §5.2) is supposed
to retire the **33 `core::task::Waker` sites in
`crates/tx-substrate/src/bus/{common,graph,port,queue}.rs`** by routing
their wake delivery through `TaskMailbox` / `WaitSource` instead.

The blocker:

```toml
# crates/tx-reactor/Cargo.toml
[dependencies]
tx-substrate = { path = "../tx-substrate" }

# crates/tx-substrate/Cargo.toml
[dependencies]
tx-hal = { path = "../tx-hal" }
```

`tx-reactor` depends on `tx-substrate`. **`tx-substrate::bus::*` cannot
directly import `tx_reactor::TaskMailbox`** — that would be a back-edge
and Cargo would reject the workspace.

Today the 33 `Waker` sites use `core::task::Waker` (which lives in
`core`, below substrate), so the layering works. PR-3D must preserve a
working layering story while replacing the `Waker` field with something
mailbox-shaped.

## 2. Options considered

### Option A — Move `bus/` up from `tx-substrate` to `tx-reactor`

**Sketch.** Hoist `crates/tx-substrate/src/bus/` to a new
`crates/tx-reactor/src/bus/` and remove the `pub mod bus;` from
`tx-substrate/src/lib.rs`. Bus then sits next to `mailbox` and
`wait_source` in the same crate, and the import is direct.

**External `tx_substrate::bus::*` consumers (grep evidence):**

```
crates/tx-reactor/src/runtime.rs          uses DeclaredPort / RawPort / DeclaredQueue
crates/tx-reactor/src/wait.rs             uses DeclaredPort / RawPort / DeclaredQueue
crates/tx-reactor/tests/wait_bus.rs       uses DeclaredPort / DeclaredQueue
crates/tx-substrate/tests/bus.rs          self-test (would move with bus/)
crates/tx-subsystems/src/tty/structure/identity.rs  uses RawPort + RawQueue
```

**What breaks.**

- `tx-subsystems::tty::structure::identity` (an upper crate) currently
  reaches *down* into `tx-substrate::bus`. If bus moves to `tx-reactor`,
  this is still fine because `tx-subsystems` depends on **both** crates
  — the imports become `tx_reactor::bus::*`. Mechanical rename.
- `tx-substrate/tests/bus.rs` moves to `tx-reactor/tests/bus.rs`. The
  `bus_lifecycle!` / `bus_readiness!` / `bus_wire_owner_manifest!`
  macros (re-exported from `crate::` in
  `tx-substrate/src/bus/mod.rs`) also move with bus.
- The bus's own internals depend on `crate::epoch::Guard` and
  `tx_hal::CpuId` (see `bus/common.rs:9` and `bus/owner.rs:1,3`). Both
  remain reachable from `tx-reactor` since `tx-reactor` already depends
  on `tx-substrate` (which re-exports `epoch::*`) and bus could
  re-import `tx_substrate::epoch::Guard`. No layering inversion.

**Cost.** ~13 file moves, two test-file moves, ~5 import-path rewrites
in `tx-subsystems::tty::structure::identity` and the reactor's own
`wait.rs`/`runtime.rs`. Touches ~7 files. No semantic change.

**Cognitive surface.** Bus is currently in tx-substrate because it
predates the runtime. Architecturally bus is a wake-delivery / event
routing primitive — it is *not* below the wake substrate, it **is** the
wake substrate. Moving it next to `TaskMailbox` reflects that.

The catch: `epoch` and `zone` are in `tx-substrate`, and bus uses
`epoch::Guard` heavily (see `bus/owner.rs`). Moving bus up means an
epoch-using subsystem now lives in tx-reactor — a small but real
inversion of the "primitives below, semantics above" story.

### Option B — Move `TaskMailbox` / `WaitSource` down from `tx-reactor` to `tx-substrate`

**Sketch.** Move `crates/tx-reactor/src/mailbox.rs` and
`crates/tx-reactor/src/wait_source.rs` into `crates/tx-substrate/src/`
(or under `crates/tx-substrate/src/wake/`). Re-export from
`tx-reactor::lib.rs` so existing `tx_reactor::TaskMailbox` paths still
resolve.

**What `TaskMailbox` transitively needs:**

```rust
// crates/tx-reactor/src/mailbox.rs imports today:
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use core::task::Waker;
use alloc::collections::VecDeque;
use tx_substrate::step_v3::{InterestMask, WaitSourceId};
use tx_substrate::SpinMutex;
```

```rust
// crates/tx-reactor/src/wait_source.rs imports today:
use alloc::sync::Weak;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use tx_substrate::step_v3::{InterestMask, WaitSourceId};
use tx_substrate::SpinMutex;
use crate::mailbox::{MailboxEvent, TaskMailbox, WaitGeneration};
```

**Both files already depend on nothing from `tx-reactor`** except their
sibling module. `WaitSourceId`, `InterestMask`, and `SpinMutex` all
live in `tx-substrate`. The only above-substrate concept is
`core::task::Waker`, which lives in `core` — below tx-substrate.

The stack `TaskMailbox` needs (zone / epoch / cap):

- **Today**: nothing from `zone`/`epoch`/`cap`. The current shape uses
  `alloc::sync::Arc<TaskMailbox>` and `alloc::sync::Weak<TaskMailbox>`.
- **Eventual** (per `wait_source.rs:30–33` doctext): the `Weak` becomes
  `tx_substrate::zone::Weak<TaskMailbox>` once PR-3D+ wires zones in.
  `zone::Weak` already lives in `tx-substrate`.

So Option B is a **straight relocation, not a redesign**. The only
content change in tx-reactor is `pub use tx_substrate::wake::*;` shims
(or full re-exports) for back-compat.

**External `tx_reactor::{mailbox,wait_source}::*` consumers (grep
evidence):**

```
$ grep -rn 'tx_reactor::mailbox\|tx_reactor::wait_source' crates/
(no matches)

$ grep -rn 'tx_reactor::TaskMailbox\|tx_reactor::WaitSource' crates/
(no matches)
```

**Zero external consumers of these types as types.** Tests inside
`tx-reactor` consume them via `crate::`. tx-subsystems / tx-shims /
tx-fs only reference `YieldShape::OnWaitSource` (whose `source` field
is a `WaitSourceId` — already in tx-substrate).

**Cost.** Move ~600 LoC of code + tests; add `pub use` shims in
`tx-reactor::lib.rs` so the names still resolve there. Zero external
import rewrites required if shims preserve the path. Bus then imports
`tx_substrate::wake::{TaskMailbox, WaitSource}` directly.

**Cognitive surface.** Excellent. The PR-3 ADR's hierarchy

```
Raw channel / raw queue   low-level delivery primitive   tx-substrate
WaitSource                object-owned semantic wait     tx-substrate (was tx-reactor)
TaskMailbox               task-owned hint delivery       tx-substrate (was tx-reactor)
ActiveWait                driver-local active wait       tx-substrate (was tx-reactor)
```

becomes consistent: bus, WaitSource, TaskMailbox, and ActiveWait all
live below the reactor task abstraction. The reactor owns
`ReactorTask`/scheduling/AST, but the wake **substrate** is one crate
down — matching the name "wake-substrate" the docs already use.

### Option C — `WakeSink` trait in `tx-substrate`; `TaskMailbox` in `tx-reactor` impls it

**Sketch.** Define a thin trait in `tx-substrate` (next to bus) that
captures the wake-delivery interface bus needs. `TaskMailbox` (still in
`tx-reactor`) implements it. Bus stores `dyn WakeSink` or generic
`W: WakeSink`.

```rust
// in tx-substrate
pub trait WakeSink: Send + Sync {
    fn post(&self, event: MailboxEvent) -> bool;
    // or, more minimal:
    fn wake(&self);
}

// in tx-reactor
impl WakeSink for TaskMailbox { ... }
```

But `MailboxEvent` itself currently lives in `tx-reactor::mailbox`
along with `WaitGeneration` and references `WaitSourceId`/`InterestMask`
from `tx-substrate`. To make the trait useful — to let bus post a
typed event — `MailboxEvent` must already be visible to bus, which
means it has to live in `tx-substrate`. At that point we've already
done most of Option B and the trait adds dispatch overhead for no
remaining purpose.

A minimal variant — `trait WakeSink { fn wake(&self); }` — sidesteps
that by making bus call the sink unconditionally and letting the
mailbox figure out what to do. But then bus loses the
generation/source/interest filtering that is *the whole point* of
PR-3: stale-hint suppression depends on `MailboxEvent::SourceFired
{ generation, source, interests }`. A bare `wake()` reverts to
"signal-the-task" semantics, equivalent to the old `Waker` and not a
PR-3 retirement.

**Dyn vs generic on the hot path.** The wake path is hot but bounded:
each `WaitSource::notify(mask)` walks a per-source subscriber list
(typically 1–4 entries; pipes/futexes might reach tens). A vtable
indirection per subscriber is one extra cache line per call, well
under 100ns even on a cold path. **Tolerable, but unnecessary if
Option B is available** — and Option B is.

**Generic variant** (`W: WakeSink` everywhere). Forces bus to
monomorphize on the wake-sink type. The 33 sites today are
heterogeneous: each `Subscriber` holds a `Waker`, not a `&T`. Replacing
`Waker` with a generic parameter would propagate the type parameter
through `RawPort`, `RawQueue`, `Subscriber`, `SubscriptionId`, and the
declared wrappers — a much larger refactor than the option claims to
be. **Reject the generic variant.**

**Cost.** Trait definition (~30 LoC) + impl on `TaskMailbox` (~30 LoC)
+ rewrite bus's `Subscriber { waker: Waker }` to
`Subscriber { sink: alloc::sync::Arc<dyn WakeSink> }` across 33 sites
+ change all `RawPort::subscribe(waker)` / `RawQueue::subscribe(waker)`
public signatures + adapt the 5 external bus callers. **More disruptive
than Option B's relocation** because it changes a public API.

**Cognitive surface.** Adds an interface that exists only to launder a
layering constraint. The PR-3 ADR (§"Final relationship") already
treats `WaitSource` as the publication point and `TaskMailbox` as the
delivery point. Introducing `WakeSink` between them is a phantom layer.

## 3. Blast radius — concrete grep results

```
$ grep -rln 'tx_substrate::bus\|tx_substrate::{[^}]*bus' crates/
crates/tx-reactor/src/runtime.rs
crates/tx-reactor/src/wait.rs
crates/tx-reactor/tests/wait_bus.rs
crates/tx-substrate/tests/bus.rs
crates/tx-subsystems/src/tty/structure/identity.rs
```

5 external files importing bus. Option A rewrites all 5 to
`tx_reactor::bus::*`. Option B leaves all 5 untouched.

```
$ grep -rln 'tx_reactor::mailbox\|tx_reactor::wait_source\|tx_reactor::TaskMailbox\|tx_reactor::WaitSource' crates/
(no matches)
```

0 external files importing `TaskMailbox` or `WaitSource` directly.
Option B's relocation requires either no import rewrites (if
`tx-reactor::lib.rs` adds `pub use`) or trivial ones.

```
$ grep -c 'Waker' crates/tx-substrate/src/bus/{common,graph,port,queue}.rs
common.rs:2
graph.rs:9
port.rs:11
queue.rs:11   (33 total)
```

Confirms `07_BLAST_RADIUS.md` §4 row F's "43 sites" was a v3-draft
count that included `owner.rs` and adjacent files; the post-merge
count concentrated in those four files is **33**, matching the v3 spec
language in §5.2 PR-3 row ("retire the 43 Waker sites in
tx-substrate/src/bus") with a moderate undercount — close enough that
the day-budget stands.

## 4. Recommendation — Option B

**Move `TaskMailbox`, `WaitSource`, and the wake substrate primitives
down to `tx-substrate`.**

Rationale:

1. **Cognitive surface.** The wake substrate already calls itself the
   *substrate*. Bus, generation, mailbox, and source all belong on the
   same architectural layer. Option B aligns code with naming;
   Option A inverts it (epoch-using code moves up); Option C invents
   a layer.

2. **Migration cost.** Option B is the smallest concrete diff: 600 LoC
   of code/tests move; **zero external import rewrites required**
   (because no crate consumes these types directly today); bus's
   `Subscriber` field becomes `Weak<TaskMailbox>` directly. Option A
   touches 7 files plus the test infrastructure; Option C rewrites 33
   public-signature sites in bus.

3. **Performance.** The hot wake path stays direct: a `Weak::upgrade`
   + a `SpinMutex::lock` + a `VecDeque::push_back`. No dyn dispatch.
   Same shape as today.

4. **Forward compatibility.** The PR-3 ADR's eventual plan is for
   `Weak<TaskMailbox>` to become `tx_substrate::zone::Weak<TaskMailbox>`
   under zone allocation. Hosting `TaskMailbox` in tx-substrate makes
   that future migration a local edit, not a cross-crate refactor.

5. **No competing claim.** No external code consumes `TaskMailbox` or
   `WaitSource` as types yet. The relocation is "free" in the sense
   that we are choosing the home before any consumer pins it.

**Trade-off acknowledged.** `core::task::Waker` (in `TaskMailbox`)
bridges to async futures. Hosting that bridge in tx-substrate slightly
broadens what tx-substrate cares about (it gains a `Waker` field). But
`Waker` lives in `core`, below tx-substrate already, so the layering
is clean. Tx-substrate already cares about `SpinMutex`, atomics, and
`alloc::sync::Arc`; adding `Waker` is no philosophical departure.

## 5. Action items in PR-3D-0 (the move)

Before any bus site is migrated:

1. Create `crates/tx-substrate/src/wake/mod.rs` (or top-level
   `crates/tx-substrate/src/mailbox.rs` + `wait_source.rs` — pick one;
   the wake/ submodule is cleaner since it groups them).
2. Move `crates/tx-reactor/src/mailbox.rs` →
   `crates/tx-substrate/src/wake/mailbox.rs`.
3. Move `crates/tx-reactor/src/wait_source.rs` →
   `crates/tx-substrate/src/wake/wait_source.rs`.
4. Re-export from `crates/tx-substrate/src/lib.rs`:
   `pub mod wake;` plus `pub use wake::{TaskMailbox, WaitSource, ...};`.
5. In `crates/tx-reactor/src/lib.rs`, replace the `pub mod mailbox;`
   and `pub mod wait_source;` lines with `pub use tx_substrate::wake::*;`
   (preserving the existing `tx_reactor::TaskMailbox` etc. names so
   nothing else in the workspace breaks).
6. Run `cargo check --workspace` — there should be **zero** import-path
   rewrites required outside the two moved files and the two lib.rs
   files.
7. Update the doc anchors in the two moved files: `[`crate::wait::Channel`]`
   references in `wait_source.rs:18` and elsewhere become
   `[`tx_reactor::wait::Channel`]` (cross-crate doc link), or just drop
   the link.

## 6. Refreshed PR-3D wave plan

Replacing the four-phase sketch in `2026-05-11-pr-3-wake-substrate-shape.md`
("Phase PR-3D — retire direct Waker sites"). The refresh adds a phase
PR-3D-0 for the layering move and preserves D2's coexistence rule.

| Phase | Goal | Days | Touches |
|---|---|---|---|
| **PR-3D-0** | **Layering move.** Move `mailbox.rs` and `wait_source.rs` from `tx-reactor` to `tx-substrate::wake`. Add re-export shims in `tx-reactor::lib.rs`. **Zero behavioural change.** | 0.5 | 4 files (2 moved, 2 lib.rs) |
| **PR-3D-1** | **Pipe readiness migration.** Convert pipe's `read_wq` / `write_wq` from `Channel`+`Waker` to `WaitSource::register_prepared` + `TaskMailbox`. Per D2: leave `RawPort::subscribe(waker)` intact, just don't add new callers. Validates the lost-wake fix end-to-end (`crates/tx-subsystems/src/pipe.rs` is the highest-confidence canary). | 1 | ~3 files |
| **PR-3D-2** | **Timerfd / pidfd / process exit_source migration.** Convert `crates/tx-subsystems/src/process/structure.rs` `exit_source` (formerly `exit_port`) and timerfd readiness to `WaitSource`. | 1 | ~4 files |
| **PR-3D-3** | **Remaining bus users — TTY readiness, futex.** Convert `crates/tx-subsystems/src/tty/structure/identity.rs` (currently the one tx-subsystems site reaching into `tx_substrate::bus`) and `crates/tx-subsystems/src/futex.rs`. | 1 | ~3 files |
| **PR-3D-4** | **Retire `RawPort::subscribe(waker)` / `RawQueue::subscribe(waker)`** and the 33 `Waker` field sites in `bus/{common,graph,port,queue}.rs`. Convert the bus's `Subscriber.waker: Waker` to `Subscriber.mailbox: Weak<TaskMailbox>` (or demote `RawPort` to a `WaitSource` internal). The reactor's own `tx-reactor::wait.rs` (which still uses `RawPort` directly) is converted in the same PR. | 1.5 | ~10 files |

**Total: 5 days**, slightly over the 4-day allotment in
`07_BLAST_RADIUS.md` §5.2 row PR-3 — but PR-3 in that table is the
whole wake substrate, of which PR-3A/3B/3C are already in tree. The
remaining PR-3D budget is the "framework already exists, migration
remains" half. The 0.5-day PR-3D-0 layering move plus 4 days of
per-subsystem migration fits.

PR-3D-0 is **mergeable independently** and unblocks the other phases —
it should land first, alone, so reviewers see only the move, not
the migration.

## 7. Unexpected coupling discovered

Two findings worth recording for future cleanup:

1. **`tx-subsystems::tty::structure::identity`** reaches *down* into
   `tx_substrate::bus::{RawPort, RawQueue}` — it is the only
   non-reactor upper-crate consumer of bus types as types. The PR-3D-3
   migration above converts it to `WaitSource` along with the other
   readiness sites. Worth flagging because this is the only site
   that proves the bus's "raw" surface still escapes tx-substrate.

2. **`tx-subsystems::wait_source`** (distinct from
   `tx-reactor::wait_source`) is a registry that maps a
   `WaitToken::source_id()` to a `tx_reactor::wait::Channel`. It is
   tx-subsystems-internal and uses the **old** `Channel` API, not the
   new `WaitSource`. When PR-3D-1/2/3 migrate the underlying
   readiness sites, this registry should also migrate to lookup
   `&'static WaitSource` (or `Arc<WaitSource>`) rather than `Channel`.
   That work is **out of scope for PR-3D-0**; flag it as a PR-3D-2 or
   PR-3D-3 follow-up.

## 8. Relationship to existing ADRs

- **`2026-05-11-pr-3-wake-substrate-shape.md`** is the architecture
  ADR. Its §"Migration story: wrap first, replace later" Phase PR-3D
  remains correct in spirit but predates the layering blocker. **This
  ADR refines that phase plan**; the shape ADR does not need to be
  edited — the refined plan in §6 above is the authoritative landing
  sequence and the shape ADR is cross-referenced.
- **`2026-05-11-d2-waitsource-coexists-with-rawport.md`** chose option
  A (parallel surfaces) over rewriting `RawPort` internals. This ADR
  is fully compatible: bus stays in tx-substrate, `RawPort`'s old
  `subscribe(waker)` surface stays through PR-3D-3, and PR-3D-4
  retires it. D2's "PR-3D landing sequence" §"PR-3D.1 / PR-3D.2 / ..."
  maps onto §6's PR-3D-1/2/3/4 here.

D2 does **not** need updating: its 5-phase sequence and §"Status"
section ("PR-3D.1 is already landed") remain accurate. This ADR adds
PR-3D-0 *before* D2's PR-3D.1, which is consistent with D2's framing —
the layering move is a precondition to the migration, not part of it.
