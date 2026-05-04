---
date: 2026-05-04
topic: "VM/PageBacked gap update post main resync"
status: complete
plan: docs/progress/plans/2026-05-04-vm-pagebacked-v1-completion.json
prior: docs/progress/research/2026-05-03-vm-doc-gap-ledger.md
---

# VM/PageBacked Gap Update (post main resync)

## Question

What changed for VM/PageBacked between the 2026-05-03 doc gap ledger and the
2026-05-04 main resync, and where does that leave the active mitigation
order?

## Summary

Two new HAL surfaces (`useraccessif`, `irqif`) and the HAL board cleanup
landed on `main` after the 2026-05-03 ledger was written and were absorbed
into this branch via `git reset --hard origin/main` followed by cherry-pick of
the ten local PageBacked/VM commits. The substance of the prior ledger still
holds, with two material updates:

- `useraccessif` is now an available HAL trait, removing one of the prior
  blockers ("user-buffer byte copy plumbing"). Wiring it through PageBacked
  `step_read` and `step_write` is now an unblocked slice.
- The mitigation order from the prior ledger has been promoted to a formal
  active plan, `docs/progress/plans/2026-05-04-vm-pagebacked-v1-completion.json`,
  with a 17-step DAG, three independent waves of parallel work, and explicit
  out-of-scope blockers (`fork_aspace` / `exec_aspace` / trap dispatch wait on
  Process; reclaim policy and writeback scheduling are v1-deferred).

Updated completion estimate against the `VM_v1_2` and `PAGE_BACKED_v1`
contracts: ~45% structure and ~30% behavior. The plan targets >=85% on both,
deferring only items the active design docs already mark deferred or items
blocked on Process/ThreadRuntime ownership.

## What Moved

- `useraccessif` HAL trait merged on main via commits `03557d2` and the
  follow-on `06d1461 Merge branch 'hal'`. PageBacked `step_read`/`step_write`
  can now consume it for byte-accurate user-buffer transfer without inventing
  a substitute.
- `irqif` HAL trait merged in the same wave. Not on the VM critical path but
  removes one HAL-side dependency for later trap/Process work.
- Active plan covers the seventeen slices the prior ledger listed in its
  Mitigation Order plus separate steps for VM pmap walk/protect, observation
  syscalls (madvise/msync/mincore), and reflink/CoW scaffolding.

## What Did Not Move

- Async `fault_script` retry/yield is still missing; current fault handling
  remains the synchronous `resolve_fault` helper.
- `fork_aspace` / `exec_aspace` / trap page-fault dispatch are still blocked
  on Process/ThreadRuntime ownership of `Cap<AddressSpace>` and the
  ThreadRuntime authority path.
- Concrete `FsPageBacking` backends (ext4, devfs, bdev-fs) are still missing;
  the plan introduces a mock backend as their interim substitute.
- Persistent/epoch recipe snapshots are still on the
  `BTreeMap`-snapshot-publication staging shape.

## New Active Plan

`docs/progress/plans/2026-05-04-vm-pagebacked-v1-completion.json` is the new
mitigation order. The prior ledger's "Mitigation Order" section can be read as
the plan's intent statement; the plan's `steps` array is the canonical
breakdown.

## Verification For This Update

- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `git diff --check`
