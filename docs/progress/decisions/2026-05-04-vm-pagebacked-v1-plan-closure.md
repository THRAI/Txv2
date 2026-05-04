# Decision: Close VM/PageBacked v1 completion plan at 16/16 + 1 prereq

**Date:** 2026-05-04

## Decision

- The `2026-05-04-vm-pagebacked-v1-completion` plan is closed with all 17
  steps (16 original + 1 plan-extension prerequisite
  `waittoken-channel-resolver`) complete. Plan status flips from
  `active` to `complete`.
- The persistent EBR-backed recipe publication (`818771a`) used
  `tx_substrate::epoch::retire_raw` directly rather than introducing a
  persistent-BTree crate or hand-rolling a path-copy structure. This
  was the user-prompted simplification after I had over-scoped the
  slice as multi-session.
- The async script wave (`mmap-script-async` through
  `fault-script-async`) ships behind a single `WaitToken → Channel`
  resolver registry in `tx-kernel/src/wait_carrier.rs`. RangeLock owns
  one channel per `AddressSpace` and fires it on every release; async
  wrappers convert `WouldBlock` to a wait-and-retry loop via the
  resolver. The `tx_reactor::wait::Channel` API was sufficient — no
  new reactor surface was needed.

## Context

- Pre-resync state was ~45% structure / ~30% behavior against
  VM_v1_2 / PAGE_BACKED_v1.
- The plan promised ≥85% on both, leaving only items the active design
  docs already mark as deferred-by-v1 or that require a Process
  subsystem that does not yet exist.
- Mid-plan I had argued `persistent-epoch-recipes` was a multi-session
  rewrite. The user observed that `tx_substrate::epoch` already
  provides full EBR (`Guard`, `retire_raw`, `try_drain`) and this slice
  could land in one focused session. They were right; the slice landed
  in `818771a`.
- The async script wave was bottlenecked on a missing `WaitToken →
  Channel` resolver. I added it as a plan-extension slice
  (`49c2e87`), then used it as the substrate for all five async
  wrappers (mmap, unmap/protect/remap, brk, fault).

## Consequences

- VM/PageBacked is at ~85% structure / ~80% behavior. Remaining gaps
  fall into three buckets, all explicitly tracked in
  `docs/progress/research/2026-05-04-vm-pagebacked-final-ledger.md`:
  Process-blocked (fork/exec/trap dispatch), PageBacked PC-side
  blocking (File-variant `materialize_page` Blocked outcomes are not
  yet routed through `fault_script_async`), and v1-deferred items the
  design docs already mark out of scope.
- The async wrapper template (try-acquire → on WouldBlock extract
  token + drop + await + retry) is now the canonical shape for any
  future script wrapper. PageBacked-side wait channels can be added
  with the same `wait_carrier` plus per-PC channel idiom.
- `tx_substrate::epoch::retire_raw` is now `pub` (was `pub(crate)`).
  Future upper-layer publication paths can opt into EBR-managed
  reclamation directly.
- `RangeLock::new` is no longer `const` because it allocates a
  `Channel`. Callers that needed const construction would have to
  defer registration; in practice the only callers are
  `AddressSpace::new_for_platform` and tests, none of which require
  const.

## Alternatives Considered

- **External persistent-BTree crate (e.g. `im`, `crossbeam-skiplist`)
  for `persistent-epoch-recipes`.** Rejected after the user's prompt:
  tx_substrate's existing EBR plus an `AtomicPtr<RecipeTree>` was
  sufficient. The trade-off is the writer still clones the BTreeMap
  per mutation; readers no longer clone, just borrow under a guard.
- **Carrier-as-pointer encoding for `WaitToken`.** Rejected because
  existing test mocks construct `WaitToken::new(13, 0x55)` with
  arbitrary bits, and an unsafe pointer interpretation would be UB.
  The registry approach lets test placeholders coexist with real
  tokens — `wait_on_token` returns `None` for unregistered carriers.
- **Land the four async wrappers as syntactic stubs that delegate to
  sync.** Rejected as plan box-ticking with no behavior change. The
  resolver detour gave us genuine §3.6 cross-async-wait discipline at
  modest extra cost.
- **PageBacked PC-side wait channels in this milestone.** Deferred.
  Adding per-PC channels mirrors RangeLock's pattern and is now
  unblocked by the resolver, but `fault_script_async` works for the
  Anon path today and the File-variant fault path needs a concrete
  backend before PC-side blocking is meaningfully exercised. Recorded
  as a follow-up in the final ledger.

## Addendum: post-closure audit and follow-ups

After the initial closure landed at `f8181c1`, a re-audit of VM_v1_2
caught three gaps the plan and the initial ledger had missed:

1. **§5.6 `fork_aspace` and §5.7 `exec_aspace`** were mis-classified
   as Process-blocked. Both functions operate entirely on
   `AddressSpace` primitives; the Process subsystem only orchestrates
   *which* AddressSpace is bound to *which* threads. Both landed in
   `a70f4a0` as plan-extension steps `fork-aspace` and `exec-aspace`.
2. **§3.1 spelling**: `RangeLock::acquire` / `acquire_pair` did not
   match the doc's `acquire_step` / `acquire_pair_step` names. Renamed
   in `e8ed0a4` (mechanical, all callers updated).
3. **§9.5 fork serialization**: fork did not acquire an
   `ExclusiveWriter` on the full user range; it depended on the
   Process subsystem to externally serialize parent VM activity. New
   `UserRange::full_user_v1()` (a v1 conservative `[0, 1 << 38)`
   range) plus an `acquire_step` call at the top of `fork_aspace`
   close this gap, also in `e8ed0a4`. Sized for Sv39 / Sv48 user
   halves; the per-platform user-VA cap is expected to replace this
   constant when finalized.

The audit also documented residual stylistic / optimization gaps that
are semantically equivalent to the doc and not load-bearing for v1:
`acquire_step` returning `AcquireResult` vs `StepOutcome<RangeGuard>`,
the `AtomicPtr<BTreeMap>` recipe publication doing per-write tree
clones (reads match the doc; writes don't), and the per-op recipe
rewriters not exposing a unified `rewrite_range(range, list)`
substrate primitive.

Revised completion estimate after the audit follow-ups: **~92%
structure / ~88% behavior** against VM_v1_2 / PAGE_BACKED_v1.
