---
name: tx-vm-pagebacked
description: Use when implementing or auditing VM, AddressSpace, RangeLock, recipes, PageContainer, PageBacked, mmap/munmap/mprotect, VM fault materialization, or VM-facing page-cache work.
---

# tx-vm-pagebacked

Use this skill before VM or PageBacked implementation. The canonical docs are
the contract; code may stage toward them, but must name any incomplete seam.

## Read First

- `docs/design/03_memory-vm/VM_v1_2.md`
- `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md`
- `docs/design/01_substrate/EBR_ZONE_INTERFACE_v1.md`
- `docs/design/00_meta-framework/INVARIANTS_v4.md` ARCH, BIF, ZONE, EBR rules
- `docs/design/02_execution/STEP_MODEL_v1.md`
- relevant `docs/progress/worktrees/` and `docs/progress/research/` notes

## Preserve

- `AddressSpace` is recipes + pmap + `RangeLock`; recipes are authoritative
  range bindings. Do not replace them with a fixed array or ad hoc linear store
  unless the result is explicitly marked as a bounded prototype.
- VM recipes must be range-index shaped. A temporary `BTreeMap` wrapper is a
  staging seam; the full doc target is persistent/epoch snapshot recipes.
- `RangeLock` coordinates declared operation ranges. If its internals are not a
  production interval tree, label them as a simple bounded v1 reservation set.
- PTEs are derived materializations. They must be justified by recipes and
  invalidated/retained through pmap/shootdown rules.
- `PageContainer` owns page-cache/page-backed storage. VM fault code consumes
  PageBacked materialization; it does not invent file-cache ownership locally.
- User access, syscall fault dispatch, Process `Frame`, ThreadRuntime state,
  and VFS/PageBacked integration are adjacent seams. Do not silently implement
  them inside a narrow VM lane.

## Implementation Harness

- Start with an implementation-readiness pass: list which doc obligations are
  implemented, staged, or blocked.
- Keep the first patch narrow: VM value/index/coordination code in VM-owned
  paths; PageBacked code in PageBacked-owned paths.
- When staging, encode the seam in types or comments and in the worktree record.
- Tests should cover split/rewrite, overlap rejection, declared-range locking,
  materializer/writer exclusion, and any range-index lookup behavior.

## Checks

- `cargo fmt --check`
- `cargo test -p tx-kernel vm`
- `cargo test -p tx-kernel`
- `cargo xtask progress validate`
- `git diff --check`

Add `cargo xtask lint docs` when design/progress docs are edited.
