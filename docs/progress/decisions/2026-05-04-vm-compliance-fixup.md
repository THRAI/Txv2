# VM compliance fixup against VM_v1_2

**Date:** 2026-05-04
**Branch:** `vm-compliance-fixup`
**Status:** Complete (revised). CI green (11 gates). 79/79 vm:: tests pass.

**Revision note.** Initial branch landed only a doc note for the
`AcquireResult` → `StepOutcome` structural drift (item B4). Reviewer
flagged the pivot; B4 was re-opened and a follow-up commit (`b9c7c38`)
made the canonical change with a dual-API split. Spec is now realized
literally on the public surface; the rich variant is retained as a
`_rich`-suffixed method for writer-preference tests.

## Context

A post-merge audit of `crates/tx-subsystems/src/vm/` against
[`VM_v1_2.md`](../../design/03_memory-vm/VM_v1_2.md) classified the drift
between the implementation and the spec into three buckets: naming,
structural, and core-DS. Most items were naming/structural and
mechanically closeable on a focused branch; deeper items (`exec_aspace`
needing `ExecImage` and the process subsystem; persistent-BTree
recipes) were deferred. This branch lands the closeable subset.

## What changed

### 1. Script naming (commit `cb35a62`)

Aligned VM execution surface with VM_v1_2 §5 spec spelling. Dropped the
`_async` suffix on canonical async scripts; renamed the synchronous
"try-once" helpers to `try_*` so the public surface clearly distinguishes
the canonical async script from the non-blocking try variant.

| Before | After | Spec |
|---|---|---|
| `map_script_async` | `mmap_script` | §5.2 |
| `unmap_async` | `munmap_script` | §5.3 |
| `protect_async` | `mprotect_script` | §5.4 |
| `remap_async` | `mremap_script` | §5.5 |
| `fault_script_async` | `fault_script` | §5.1 |
| `brk_script_async` | `brk_script` | §5.8 |
| `map_script` (sync) | `try_mmap` | helper |
| `remap_script` (sync) | `try_mremap` | helper |
| `unmap` (sync) | `try_munmap` | helper |
| `protect` (sync) | `try_mprotect` | helper |

Test function names updated to match. Test count unchanged at 77.

### 2. Doc reconciliation (commit `63e72fc`)

Edited `VM_v1_2.md` to match the realized implementation surface:

- `VAddrRange` → `UserRange` throughout. The impl uses the typed-boundary
  spelling and the spec previously carried both interchangeably; one
  spelling now.
- `UserRange::full_user()` → `::full_user_v1()` and
  `UserRange::new(addr, len)?` → `::new_aligned(addr, len)?` to match
  impl-actual constructor names.
- §5.9 `mincore` clarified to return per-page `Vec<bool>` matching POSIX
  `mincore(2)`'s residency vector. Was previously implied to be
  `StepOutcome<u32>`.
- §3.1 RangeLock — added an implementation note that the canonical
  `StepOutcome<RangeGuard>` return is realized as the richer
  `AcquireResult` enum, whose `WouldBlock` variant carries an internal
  `PendingWriter` slot used for writer-preference. Async script wrappers
  project to `StepOutcome` by extracting `wait_token()`. Spec semantics
  unchanged; impl shape documented.
- §2 AddressSpace recipes — added an implementation note that the
  semantic `PersistentBTree<UserRange, VmEntry>` is realized as a
  copy-on-write `BTreeMap<UserVirtAddr, VmEntry>` published behind
  `AtomicPtr` with EBR. Snapshot-consistency holds; structurally-shared
  persistent BTree is a future optimization.

No code changes in this commit.

### 3. MADV_DONTNEED / MADV_FREE implementation (commit `83a3903`)

`madvise` was a no-op for every advice; VM_v1_2 §5.9 specifies
`DontNeed` as range-scoped pmap teardown + shootdown that preserves the
`VmEntry` recipe (next access refaults clean), and `Free` is "equivalent
to DontNeed for anonymous memory". Implemented:

- `MadviseAdvice` gained a `Free` variant.
- `madvise(range, DontNeed | Free)` acquires `ExclusiveWriter`, calls
  `pmap.teardown_range`, refreshes `AddressSpaceStats`. Recipes
  preserved. Returns `WouldBlock` on contention.
- `Normal`, `Random`, `Sequential`, `WillNeed` remain no-ops per §9.7.

Tests:

- `vm_madvise_dontneed_tears_down_ptes_and_preserves_recipe` — faults
  two pages in, calls madvise(DontNeed), verifies mincore reports both
  absent and lookup still resolves the recipe.
- `vm_madvise_free_acts_as_dontneed_for_anon` — same shape with `Free`.
- Old `vm_madvise_accepts_documented_advice_without_state_change`
  renamed to `vm_madvise_noop_advice_preserves_recipes_and_pmap` with
  DontNeed removed from the no-op list.

VM lib tests: 79/79 pass with `--test-threads=1` (was 77).

### 4. Cargo fmt (commit `a4ef976`)

Two doc-comment-trigger formatting deltas from the rename pass.

### 5. RangeLock canonical StepOutcome surface (commit `b9c7c38`)

Closes audit item B4. Public method
`RangeLock::acquire_step(range, mode) -> StepOutcome<RangeGuard<'_>>`
matches the spec literally; the rich `AcquireResult` is retained as
`acquire_step_rich` for writer-preference tests. Same shape for
`acquire_pair_step` / `acquire_pair_step_rich`. Production callers
(scripts, sync helpers, `reserve_map`, `acquire_writer`) all use the
canonical surface. Two private helpers in `execution.rs`
(`await_range_lock`, `unreachable_acquire_step`) centralise the
await-and-retry and unreachable-arm patterns.

`MapReserveResult::WouldBlock(WouldBlock<'a>)` simplified to
`MapReserveResult::Blocked(WaitToken)` — the rich carrier was only
reached by the async script and the new canonical surface returns the
token directly.

## What was deferred (not drift)

- **`exec_aspace` rebuild half.** Doc says
  `exec_aspace(old_aspace, new_image: &ExecImage) -> Result<AddressSpace, Errno>`;
  impl is `exec_aspace(old_aspace) -> usize` (teardown count). Building
  the new `AddressSpace` requires `ExecImage` from `EXEC_v1` and the
  process subsystem, neither of which exists yet. Out of scope for this
  branch.
- **Persistent-BTree recipes.** Implementation note added in the doc;
  COW-`BTreeMap` is interim. Revisit if profiling shows mutation cost.

## Verification

- `cargo xtask ci` — 11/11 gates pass:
  - format / clippy / host workspace check / host unit tests /
    architecture lint / documentation lint / unused lint / progress json /
    rv64 qemu target / rv64 m1dock mock / la64 qemu target.
- `cargo test -p tx-subsystems --lib vm:: -- --test-threads=1` — 79/79.

## Commit ledger

- `cb35a62` — `vm: rename scripts to VM_v1_2 spec names`
- `63e72fc` — `docs(vm): reconcile VM_v1_2 with implementation surface`
- `83a3903` — `vm: implement MADV_DONTNEED + MADV_FREE per VM_v1_2 §5.9`
- `a4ef976` — `vm: cargo fmt`
- `0ba9fea` — `docs(progress): record VM compliance fixup completion`
- `b9c7c38` — `vm: align RangeLock acquire_step with canonical StepOutcome surface`

## Next step

Branch is ready to merge. Suggested follow-ups (separate work):

1. Process subsystem (and downstream `exec_aspace` rebuild half once
   `ExecImage` exists).
2. Persistent-BTree recipes (only if profiling motivates).

## Blockers

None.
