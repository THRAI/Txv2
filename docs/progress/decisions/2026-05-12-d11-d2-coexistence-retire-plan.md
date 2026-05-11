# Decision D11: D2 coexistence — retire plan for legacy `Channel`/`Waker` paths

**Date:** 2026-05-12
**Status:** decided (research-only audit + retire plan; no production code change)
**Worker:** W-OO (audit pass)
**Companion:** [D2](2026-05-11-d2-waitsource-coexists-with-rawport.md),
[D4](2026-05-11-d4-bus-mailbox-layering.md),
[D9 family](2026-05-11-d9-signal-mailbox-routing.md) (signal/signalfd mailbox routing — adjacent, not a Channel retire target),
PR-3D-1..5 worker reports.
**Supersedes:** nothing. Refines D2's §"PR-3D landing sequence" §PR-3D.5
(the bullet that said "Remove or demote `RawPort.subscribe(waker)`") into
a measured phase plan with a sharper boundary between **wake-routing
Channels** (retire) and **bus-protocol `RawPort` / `RawQueue`** (stay).

---

## 1. Executive summary

<!-- txdoc:D11-EXEC-SUMMARY -->

PR-3D-1..5 landed the new `WaitSource` / `TaskMailbox` substrate for the
five readiness consumers D2 prioritised (pipe, futex, exit_source, tty,
vfs). For every one of those five, the producer-side **fires both** the
legacy `tx_reactor::wait::Channel` and the new `Arc<WaitSource>` —
exactly the D2 coexistence shape. Three additional consumers (aio,
io_uring, userfaultfd) registered a `Channel` carrier-id reservation but
never fire it in production; **only the new `WaitSource` is fired**, so
those are effectively new-only.

| Metric | Count | Where |
|---|---|---|
| Legacy `Channel` producer sites in production that fire **both** Channel + WaitSource (D2-paired) | **7** | pipe (×2), futex (×1), exit_source (×1), tty (×1), vfs (×2), signalfd (×1), userfaultfd (×1) |
| Legacy `Channel` registered but **never fired** (dead carrier, new-only) | **4** | aio (×2: `iocb_arrived`, `events_available`), io_uring (×2: `sqe_arrived`, `cqe_available`) |
| Legacy `Channel`-only (no paired `WaitSource`) — **unmigrated** | **1** | `vm::structure::range_lock::RangeLockState::wait_channel` |
| Production parked-on-Channel consumer sites (`wait_source::wait_on_token`) | **10** | tx-shims: io.rs ×3, vm.rs ×2, proc.rs ×1, signalfd.rs ×1, aio.rs ×1, userfaultfd.rs ×1; tx-subsystems: vm/execution.rs ×1 |
| Reactor-internal `Channel` users (out of D2 retire scope) | **4 files** | `tx-reactor::sync_coord`, `tx-reactor::completion`, `tx-reactor::runtime`, `tx-reactor::wait` itself |

Adding the unmigrated `RangeLockState` brings total legacy fire sites
to **8** (7 paired + 1 unpaired). Total parked-on-Channel call sites
remain **10** — those are the migration targets.

**Effort estimate: 5 worker-days, fully parallelizable into 4 single-day
workers.** Detailed phase plan in §5.

**No substrate blockers found.** Every parked-on-Channel site has a
clean `WaitSource`/`TaskMailbox` equivalent via the new
`WaitSource::prepare(..).install_if(..)` shape (validated by PR-3D-1
pipe canary and the v3_exit_wait_source integration test).

---

## 2. Per-consumer status table

<!-- txdoc:D11-PER-CONSUMER -->

| Consumer | New `WaitSource` fires | Legacy `Channel` fires | Parked-on-Channel sites (production) | Retire-ready? |
|---|---|---|---|---|
| **pipe** (PR-3D-1) | ✓ (×2: PIPE_READABLE, PIPE_WRITABLE) | ✓ (×2 paired) | io.rs `sys_read` / `sys_write` (~3 sites use this carrier id) | **Yes** — full coexistence; migrate shims, drop Channel fire |
| **futex** (PR-3D-2) | ✓ (×1: FUTEX_WAKE_MASK) | ✓ (×1 paired) | 0 direct shim sites (futex_wait future already on `WaitSource`) | **Yes — easiest** — no shim migration needed, can drop Channel fire today |
| **exit_source** (PR-3D-3) | ✓ (×1: per-process) | ✓ (×1 paired) | proc.rs `sys_wait4` / `sys_waitid` (×1 site, `exit_source_wait_token`) | **Yes** — single shim migration in proc.rs |
| **tty** (PR-3D-4) | ✓ (×1: TTY_READABLE) | ✓ (×1 paired) | io.rs read path (shares carrier with vfs RNode) | **Yes** (separate from RawPort/RawQueue boundary — see §4) |
| **vfs** (PR-3D-5) | ✓ (×2: read/write) | ✓ (×2 paired) | io.rs `sys_read` / `sys_write` (~3 sites) | **Yes** — shares shim migration with pipe (io.rs) |
| **signalfd** (D9-D) | ✓ (×1: SIGNALFD_READABLE) | ✓ (×1 paired) | signalfd.rs `sys_signalfd_read` (×1) | **Yes** — single shim site |
| **userfaultfd** | ✓ (×1: UFD_READABLE) | ✓ (×1 paired) | userfaultfd.rs read path (×1), vm.rs (×2 for fault wakes) | **Yes** — three shim sites |
| **aio** | ✓ (×2) | **(none — Channel registered but never fired)** | aio.rs (×1) — currently parks on the dead carrier | **Already new-only on producer side**; shim migration just drops the `wait_on_token` call in favour of `WaitSource::prepare(..)` |
| **io_uring** | ✓ (×2) | **(none — Channel registered but never fired)** | 0 (SQPOLL kthread uses `NoopPending`, not `wait_on_token`) | **Already new-only**; just drop the dead `register_wait_channel` calls |
| **range_lock** | ✗ **(not yet migrated)** | ✓ (×1) | none in tx-shims; tx-subsystems internal | **No — blocked on PR-3D-6 (sub-day migration)**: add `Arc<WaitSource>` per RangeLockState, mirror pipe pattern |
| **process/execution `ProcessPayload` mint** | ✓ (paired, via `exit_wait_source`) | ✓ (paired, via `exit_source`) | see exit_source row | covered by exit_source migration |

---

## 3. Methodology (per audit target)

<!-- txdoc:D11-METHOD -->

### 3.1 `tx_reactor::wait::Channel` uses across `tx-subsystems`

7 files (`grep -rln '::wait::Channel\|wait::Channel::' crates/tx-subsystems`):
`pipe.rs`, `futex.rs`, `process/execution.rs`, `process/structure.rs` (via
struct field; mint in `execution.rs`), `tty/structure/identity.rs`,
`vfs/structure.rs`, `vm/structure/range_lock.rs`, plus
`io_uring.rs`/`aio.rs`/`signalfd.rs`/`userfaultfd.rs`. Classifications:

- **D2-paired-fire (production fires both):** pipe, futex, exit_source,
  tty, vfs, signalfd, userfaultfd. (7 producer sites; pipe/vfs each fire
  on 2 directions → 9 *masks* total).
- **Legacy-only fire (would be a bug):** **none found**. The audit's
  pessimistic case ("we fire only Channel for some site") is empty.
- **New-only fire (Channel registered but dead):** aio, io_uring. Per
  `aio.rs:381-388` and `io_uring.rs:260-265`, the local `Channel::new()`
  value is moved into `register_wait_channel` *but never stored on the
  struct and never fired*. The id is reserved so a `WaitSourceId.raw()`
  can be used by future `wait_on_token` callers, but those callers wake
  only via the paired `WaitSource::notify`. Effective state: new-only;
  the legacy resolver lookup returns a `Channel` that nobody ever fires.
- **Test scaffolding:** `vm/tests/script_async.rs` constructs a
  `Channel` for an async test harness; not a production fire site.
- **Doc reference:** various `//! 1. The legacy Channel...` block
  comments in pipe/futex/userfaultfd; cosmetic only.

### 3.2 `core::task::Waker` direct uses outside `tx-reactor`

Only **bus internals** (`crates/tx-substrate/src/bus/{common,graph,port,queue}.rs`)
and one doc string in `tx-subsystems/src/thread_runtime/structure.rs:160`.
The bus internals are the 33 `Waker` field sites D4 §3 already
catalogued; their retire is **PR-3D-4 in D4's plan** (bus's
`Subscriber.waker: Waker` → `Subscriber.mailbox: Weak<TaskMailbox>`),
which is the larger bus rewrite that D4 separates from D11's scope.
**D11 explicitly does NOT retire bus's `Waker` field — that is PR-3D-4
under D4.**

Outside bus + reactor + that one doc string: zero direct `Waker`
holdings in upper crates.

### 3.3 `tx_reactor::wait::WaitOutcome` uses

12 hits in production + tests (`grep -rn 'WaitOutcome' crates/`).
**`tx_substrate::step_v3::WaitOutcome` is a separate, newer type**
(`step_v3/wait_protocol.rs:57`) — it is the v3-shape outcome used by
StepOp scripts. The two namespaces *coexist* for the same reason as
`WaitProtocol`: the reactor's `WaitOutcome` is a Channel-side concept
(returned from `Channel::wait()` futures); the step_v3 one is a
walker-side concept. **The D6 timer queue migration cleaned the timer
side, but the reactor-internal sites in `sync_coord`, `completion`,
`runtime`, and the reactor's own `wait.rs` still produce
`tx_reactor::wait::WaitOutcome`.** Those are reactor-substrate
internals, not D2 coexistence — keep them. Once `Channel` retires from
upper layers, the reactor can compress its own `WaitOutcome` into the
step_v3 enum if desired (a follow-up, not D11 work).

### 3.4 `tx-substrate::bus::{RawPort, RawQueue}` uses — boundary clarification

Per D2: bus is the **delivery primitive**, and `RawPort`/`RawQueue` are
not D2 retirement targets. Verified:

- **tx-reactor**: `runtime.rs`, `wait.rs`, `tests/wait_bus.rs` use
  `DeclaredPort`/`RawPort` as the wake-substrate building block for
  `tx_reactor::wait::Channel`. **Retires with Channel** in PR-3D-4
  (D4 §6), not D11.
- **tx-substrate**: `bus::*` internals — bus-protocol, stays per D2.
- **tx-subsystems::tty::structure::identity**: 4 fields:
  - `input_readable: RawQueue` — **bus-protocol** (BIF-5 readiness wire,
    used by TTY ingest dispatch). Per D4 §7 finding 1, this is the only
    tx-subsystems site reaching into `tx_substrate::bus::*`; per D2 it
    stays. **Not a D11 target.**
  - `output_writable: RawQueue` — same; bus-protocol, stays.
  - `hangup_port: RawPort` — same; bus-protocol, stays.
  - `session_ctl_port: RawPort` — same; bus-protocol, stays.

  These 4 fields are **wire-shaped readiness queues** that the TTY's
  ingest pipeline drains via `tty::execution::step_ingest`. They are
  NOT wake-routing for blocked syscalls — the syscall side parks on
  `wait_channel` / `wait_source` (the new pair added in PR-3D-4),
  which **are** D11 targets. The bus types remain.

  W-P's prior report flagged "tty has multiple RawPort/RawQueue uses" —
  confirmed but **all four are bus-protocol, none are wake-routing for
  syscalls.** Disposition: keep as-is; do not migrate.

### 3.5 Legacy `wait_source::wait_on_token` vs new `await_agent_reply` / TaskMailbox poll

`wait_on_token` consumers (10 production sites): see §1 table. These
are the **parked-on-Channel** call sites — the actual migration target.
Each driver awaits `wait_on_token(token).await` after a
`Yield { OnWaitSource }`. To migrate: replace with
`WaitSource::prepare(generation, mask).install_if(predicate)` against
the `Arc<WaitSource>` obtained from the per-object accessor that
PR-3D-1..5 added (e.g., `pipe.reader_wait_source()`,
`process.exit_wait_source()`).

`await_agent_reply` is a **different** facility (Agent/delegate replies),
not a Channel-side reader; it already uses `TaskMailbox` and is not in
D11 scope.

### 3.6 Legacy `WaitProtocol`

Two `WaitProtocol` types exist:

- `tx_reactor::wait::WaitProtocol` — Channel-side enum with embedded
  deadlines (`InterruptibleTimeout(u64)`). Used by reactor's own
  `Channel::wait_event` machinery. Retires with `Channel`.
- `tx_substrate::step_v3::WaitProtocol` — walker-side enum, deadline
  carried separately. Used by every PR-3D-migrated step function.
  **Stays — this is the v3 shape.**

The retire collapses two enums into one. Mechanical at the end of the
plan.

---

## 4. RawPort/RawQueue boundary clarification

<!-- txdoc:D11-BUS-BOUNDARY -->

| Site | Type | Disposition | Why |
|---|---|---|---|
| tx-substrate::bus internals | `Subscriber.waker: Waker` × 33 | **Retire under PR-3D-4 (D4 §6)** — out of D11 scope but on the substrate roadmap | Wake-routing, but coupled to bus's own Subscriber model |
| tx-reactor::wait `Channel.port: RawPort` | bus-protocol → wake-routing | **Retire with Channel itself** in D11 phase final | Channel is a thin wrapper over bus; once Channel goes, the wrapper goes |
| tty::identity.input_readable / output_writable | `RawQueue` | **Keep** | Bus-protocol BIF-5 readiness wire; TTY ingest dispatches via these, not via blocked-syscall wake |
| tty::identity.hangup_port / session_ctl_port | `RawPort` | **Keep** | Bus-protocol session-control wire; ditto |
| tty::identity.wait_channel + wait_source | Channel + Arc<WaitSource> | **Retire wait_channel under D11** | Wake-routing for blocked `read(2)` — the actual D2 target |

The boundary that matters: **`RawPort`/`RawQueue` stay when they carry
typed bus events between drivers; they go when they back a
syscall-blocking `Channel`.** Every tty bus field is the first kind;
the syscall-blocking pair `(wait_channel, wait_source)` is the second.

---

## 5. Retire plan — phase-by-phase

<!-- txdoc:D11-RETIRE-PLAN -->

Total: **5 days**, parallelizable to 4 single-day workers (phases 1–4
fan out; phase 5 is the cleanup gate).

### Phase D11.1 — futex (easiest; 0.5 day, single worker)

**Why first.** Zero `wait_on_token` consumers — futex's syscall-side
already routes through the new `WaitSource` (PR-3D-2's
`bucket_wait_source` API). The Channel fire is unobserved.

**Steps.**
1. Delete `futex.rs:228` `buckets[idx].channel.fire(..)` line.
2. Drop `channel: Channel` field from the bucket struct.
3. Drop `wait_source::register_wait_channel(channel.clone())` call.
4. Drop the matching `release_wait_channel` if futex has a teardown
   path (it doesn't — buckets are static).

**Verification.** `cargo test -p tx-subsystems --test v3_futex*` plus
the pipe canary regression suite.

### Phase D11.2 — aio + io_uring dead-carrier cleanup (0.5 day, single worker)

**Why early.** No production fires for these Channels; the carrier-id
reservation is the only thing keeping `register_wait_channel` in
`AioContext::with_nr_events` and `IoUring::with_entries`.

**Steps.**
1. In `aio.rs:381-388`, drop the four lines that allocate and register
   the two Channels. Replace `iocb_arrived_id` derivation with
   `iocb_arrived.id().raw()` (the `WaitSource` already has the id —
   currently they share the value the registry minted).
2. In `io_uring.rs:260-265`, ditto for `sqe_arrived` / `cqe_available`.
3. Update `aio.rs:692` shim site (the lone `wait_on_token` caller for
   aio) to use `WaitSource::prepare(..).install_if(..)` against
   `events_available_source()`.

**Caveat.** The `WaitSource::new(WaitSourceId::new(id))` shape currently
takes the id *from* the legacy registry. After this phase, aio /
io_uring mint their own id from a substrate counter — either a fresh
`AtomicU64` per consumer or a new
`tx_substrate::wake::next_wait_source_id()` helper. The latter is
cleaner and reused by phases D11.3–D11.5.

**Sub-step.** Add `tx_substrate::wake::next_wait_source_id() -> u64`
(thin wrapper around an `AtomicU64`). 5-line addition to
`crates/tx-substrate/src/wake/mod.rs`.

### Phase D11.3 — exit_source + signalfd + userfaultfd (1 day, single worker)

**Why grouped.** Each has exactly one upper-shim site to migrate.
- exit_source: `proc.rs:414` (`sys_wait4` / `sys_waitid`).
- signalfd: `signalfd.rs:188` (`sys_signalfd_read`).
- userfaultfd: `userfaultfd.rs:773` + `vm.rs:417` + `vm.rs:564` (the
  fault-script's three park sites).

**Steps per consumer.**
1. Replace `wait_source::wait_on_token(token).await` with a
   `TaskMailbox`-driven park: derive the `Arc<WaitSource>` from the
   per-object accessor (`process.exit_wait_source()`,
   `signalfd.wait_source()`, `ufd.wait_source()`), then
   `WaitSource::prepare(gen, mask).install_if(predicate).await`.
2. Drop the legacy `Channel` field, the `register_wait_channel` call,
   and the `release_wait_channel` from the object's `Drop`/teardown.
3. Drop the corresponding `fire(Mask::from_bits(..))` line from the
   producer.

**Verification.** `cargo test -p tx-shims --test v3_signalfd` plus
`v3_exit_wait_source` plus the userfaultfd test suite. The
`v3_signalfd` test currently exercises the paired path; after this
phase it should pass with only the WaitSource side wired.

### Phase D11.4 — pipe + tty + vfs (1.5 days, two single-day workers in parallel)

**Why grouped.** All three share `tx-shims/io.rs` as the shim site
(3 production `wait_on_token` calls; one each for read-blocked,
write-blocked, and the readiness check). Worker A takes pipe + vfs
(shared shim file). Worker B takes tty (independent shim path through
`tty/execution/step_ingest.rs`).

**Steps (worker A: pipe + vfs).**
1. In `io.rs:284, 292, 503`, replace the `wait_on_token` calls with
   `WaitSource::prepare(..).install_if(..)` against the appropriate
   `Arc<WaitSource>` (pulled from the `OpenFile`'s backing — pipe's
   reader/writer source, or vfs RNode's read/write source).
2. Drop `reader_wait_channel`, `writer_wait_channel` from `pipe.rs`
   `PipePayload`; drop their fires (`pipe.rs:308, 327, 474, 523`); drop
   the `register_wait_channel` / `release_wait_channel` calls.
3. Same for `read_wait_channel` / `write_wait_channel` on `RNode`
   (`vfs/structure.rs:507, 526`); drop the four fires in `fire_read` /
   `fire_write`.

**Steps (worker B: tty).**
1. Migrate the `tty/execution/step_ingest.rs` consumer to install on
   `tty.wait_source()` instead of `wait_channel()`.
2. Drop `wait_channel` field from `TtyIdentity` (`identity.rs:235`)
   along with its `register_wait_channel` / `release_wait_channel`.
3. **Keep all four bus fields** (`input_readable`, `output_writable`,
   `hangup_port`, `session_ctl_port`) per §4 boundary.

**Verification.** Full pipe + tty + vfs regression sweep — these are
the highest-coverage paths in the workspace.

### Phase D11.5 — range_lock migration + Channel retire gate (1 day, single worker)

**Why last.** `range_lock` is the one unmigrated D2 consumer (lacks a
paired `WaitSource`). Adding it is a sub-day mechanical mirror of pipe.
After this phase, **zero production sites fire `Channel`**, and the
type can be removed.

**Steps.**
1. Add `wait_source: Arc<WaitSource>` to `RangeLockState`
   (`vm/structure/range_lock.rs`). Pair with the existing
   `wait_channel`; on release, fire both.
2. Migrate the (in-tx-subsystems) consumer to use the WaitSource.
3. Drop `wait_channel` and the legacy registry register/release.
4. **Retire `tx_reactor::wait::Channel`** — delete the type, the
   `WaitFuture` / `WaitEventFuture`, the legacy `WaitProtocol`, and
   the legacy `WaitOutcome`. Reactor-internal consumers (`sync_coord`,
   `completion`) inline-migrate to `WaitSource` (~30 LoC each — they
   own their object, so the migration is trivial).
5. Retire `tx-subsystems::wait_source` resolver module entirely.
6. Retire `Channel::with_timer_queue` and `Reactor::channel()`
   constructor from `runtime.rs`.
7. Keep `tx_substrate::bus::{RawPort, RawQueue}` per D2/D4 §7 —
   D11 does NOT retire bus internals. (Bus's own `Waker` field
   retirement is **PR-3D-4 under D4**, separate workstream.)

**Verification.**
- `grep -r 'wait::Channel' crates/` → zero hits.
- `grep -r 'wait_source::register_wait_channel' crates/` → zero hits.
- `cargo check --workspace --tests` clean.
- Full workspace test sweep.

---

## 6. Risk register — D2 invariants the coexistence preserved

<!-- txdoc:D11-RISKS -->

D2's rationale (§"Why A") was that **old `Waker` users are not
semantically equivalent to task-owned `TaskMailbox` users**:

```
old Waker:           wake this future somehow
new TaskMailbox:     post replayable hint with generation;
                     driver filters stale hints; step rechecks truth
```

For each parked-on-Channel site, verify the new substrate provides the
needed semantics:

| Site class | Needed semantics | WaitSource has it? |
|---|---|---|
| `io.rs sys_read` on empty pipe | wake on byte arrival; recheck on resume | ✓ — `install_if(predicate)` rechecks the condition under the registration lock |
| `io.rs sys_write` on full pipe | wake on space arrival; same pattern | ✓ — same |
| `proc.rs sys_wait4` on no zombie | wake on child exit; recheck via `step_waitpid_nohang` | ✓ — `install_if` predicate runs the synchronous walker |
| `signalfd.rs sys_signalfd_read` empty queue | wake on new signal; recheck queue depth | ✓ — same pattern as pipe |
| `vm.rs userfaultfd fault park` | wake on fault-handler reply; recheck pending-fault state | ✓ — `await_agent_reply`-adjacent shape works without modification |
| `aio.rs sys_io_getevents` empty cqring | wake on completion; recheck `events_available` | ✓ — `events_available_source()` already wired |

**No blockers found.** Every site has a clean WaitSource equivalent.
The `install_if` predicate is what bridges the "raw wake" vs
"replayable hint with truth recheck" gap that D2 worried about — it
runs the post-resume truth check **before** committing the
registration, eliminating the lost-wake window that bare `Waker` users
had to defend against by hand.

**Single soft caveat.** The reactor-internal `sync_coord` and
`completion` modules use `Channel::wait_event(mask, protocol, cond)` —
their `cond` closure is the synchronous truth recheck. After phase
D11.5 inlines these to `WaitSource`, the truth recheck moves into the
`install_if` predicate. Behaviour-equivalent but mechanically distinct
— review carefully when phase D11.5 lands.

---

## 7. Out of D11 scope (deferred / adjacent)

- **PR-3D-4 bus retirement** (33 `Waker` sites in `tx-substrate::bus`).
  D4 §6 owns this; it is a larger bus rewrite that converts
  `Subscriber.waker: Waker` → `Subscriber.mailbox: Weak<TaskMailbox>`.
  D11 finishes the *outer* consumer migration; D4 finishes the *inner*
  bus rewrite. The two can land in either order — they share no files.
- **Reactor `WaitOutcome` collapse**. Once `Channel` is gone, the
  reactor's `wait::WaitOutcome` can be aliased to or replaced by
  `step_v3::WaitOutcome`. Sub-day follow-up, not on the critical path.
- **Bus `DeclaredPort` / `DeclaredQueue` / `DeclaredChannel`** (typed
  event wire). Used by the bus's own tests + reactor wait_bus
  smoke. Not Channel-side wake-routing; not a D11 target.

---

## 8. Relationship to existing ADRs

- **D2** (`2026-05-11-d2-waitsource-coexists-with-rawport.md`)
  introduced the coexistence; D11 retires it. D2's
  "PR-3D landing sequence" §PR-3D.5 ("Remove or demote
  `RawPort.subscribe(waker)`") is **refined** by D11 into a measured
  phase plan. D2 conflated "retire Channel-side wake-routing" with
  "retire bus's `Waker` field"; D11 separates them — D11 owns the
  former, D4/PR-3D-4 owns the latter.
- **D4** (`2026-05-11-d4-bus-mailbox-layering.md`) moved the wake
  substrate down to `tx-substrate::wake`. D11 builds on that —
  consumers reach for `tx_substrate::wake::WaitSource` directly, no
  reactor-side bridge.
- **D9 family** (signal mailbox routing + signalfd). D9-D introduced
  the signalfd `WaitSource`; D11 retires the Channel half. D9's
  signal-mailbox routing itself does **not** use Channel and is
  unaffected.
- **PR-3D-1..5 reports** (in `docs/progress/handoffs/` or worker
  STATUS entries 2026-05-11 through 2026-05-12). D11 closes the
  coexistence those PRs deliberately left open.

---

## 9. Verification

1. ADR complete with 8 sections covering executive summary, per-consumer
   audit, methodology, bus boundary clarification, phased retire plan,
   risk register, scope exclusions, and ADR linkage.
2. STATUS catchup appended.
3. **No production code changed** — research-only audit per worker
   scope.
