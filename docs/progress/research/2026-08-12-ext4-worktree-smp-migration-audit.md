# Ext4 worktree to SMP migration audit

Date: 2026-08-12

## Verdict

The ext4 reconciliation work is already part of the current SMP branch. It is
not a source to migrate again. The remaining dirty ext4 worktrees are useful as
behavioral and test oracles, but their old Mount, PageBacked, JBD2, VM, syscall,
exec, and reactor runtimes must not be imported.

The remaining current-architecture work is narrower than a worktree merge:

1. generalize the existing PageBacked-owned `PageDataLease` from an operational
   single-page projection to a real multi-page retained bundle;
2. define a VM-owned writable-PTE freeze lease for mmap/writeback/truncate
   concurrency without carrying `RangeLock`, epoch `Guard`, or borrowed PTE
   state across a yield;
3. preserve L4 ownership of admitted page/DMA bundles, L6 ownership of ready BIO
   execution, and ext4 `MutationHandle` ownership of journal/deferred-free
   phases; and
4. prove the resulting candidate with the current host, SMP1/SMP2, Linux,
   crash, and pinned-xfstests gates.

The RCU-on-VM `LargeArena` kernel-heap slice has been migrated separately in
this checkout. It reduces large kernel allocation failures caused by late
physical fragmentation, but it does not provide anonymous reclaim, swap, or a
large guest compiler acceptance claim.

## Worktree identity and state

Identity was recorded by path, branch, status counts, and directory timestamp.
No commit SHA was calculated or used.

| Worktree | Branch | Status on audit | Directory timestamp | Disposition |
| --- | --- | ---: | --- | --- |
| `/Users/3y/.config/superpowers/worktrees/Tx/ext4-worktree-reconciliation` | `codex/ext4-worktree-reconciliation` | clean | 2026-08-09 09:40 +0800 | Already integrated into the current branch; do not migrate again. |
| `/Users/3y/.config/superpowers/worktrees/Tx/ext4-jbd2-revoke` | `codex/ext4-jbd2-revoke` | 47 tracked, 2 untracked | 2026-07-30 17:03 +0800 | Keep revoke/replay cases as oracles; reject the old runtime and unrelated WIP. |
| `/Users/3y/.config/superpowers/worktrees/Tx/ext4-journal-settlement` | `codex/ext4-journal-settlement` | 64 tracked, 12 untracked | 2026-07-30 17:03 +0800 | Keep xfstests/tooling ideas only when missing from current APIs; reject old settlement/PageBacked runtime. |
| `/Users/3y/.config/superpowers/worktrees/Tx/ext4-rustc-performance` | `codex/ext4-rustc-performance` | 9 tracked, 6 untracked | 2026-08-06 15:28 +0800 | The role geometry, resolver, and workload ideas are already represented; historical placeholder receipts are not evidence. |
| `/Users/3y/.config/superpowers/worktrees/Tx/rsext4-migration-p0` | `codex/rsext4-migration-p0` | 21 tracked, 2 untracked | 2026-08-09 02:02 +0800 | Pure format tests remain oracles; reject mixed PageBacked/VM and synchronous device/runtime changes. |
| `/Users/3y/.config/superpowers/worktrees/Tx/rcu-on-vm` | `codex/rcu-on-vm` | 185 tracked, 55 untracked | 2026-08-11 14:19 +0800 | Source only for narrowly reviewed RCU/VM or heap slices; never a whole-worktree merge. |
| `/Users/3y/Downloads/Tx` | `codex/test-remote-network` | 3 tracked, 20 untracked before this record | 2026-08-12 audit | Target. Preserve pre-existing VM-L6 status and all unrelated user WIP. |

Current history subjects include `merge: integrate ext4 reconciliation
worktree`, the depth-three extent/runtime slices, multi-revoke reconciliation,
role-image QEMU wiring, Docker e2fsprogs checks, and rustc contract disposition.
That subject-level evidence is sufficient to reject a second reconciliation
merge without using SHA identity.

## Current target disposition

| Candidate | Current evidence | Classification | Action |
| --- | --- | --- | --- |
| Extent-node checksums | `crates/tx-ext4-format/src/pager.rs:2225-2235` refreshes `extent_block_csum32` on extent after-images. | Landed | Retain and regression-test. |
| Deep extent and unwritten conversion | `crates/tx-ext4/src/tests_v3.rs:1745-1977` contains depth-three public/runtime and Linux fixture witnesses. | Landed | Do not recopy old pager implementations. |
| Recursive truncate/destroy | `crates/tx-ext4-format/src/pager.rs:1524-1703` plans recursive subtrees and `:1804-1914` exposes truncate/destroy plans; focused depth-two/depth-three tests live in `tests/pager_mock.rs:3651-3918`. | Landed | Keep descendant-first immutable planning and no direct home writes. |
| Multi-page revoke | `crates/tx-ext4/src/journal.rs:739-760` reserves an explicit revoke-page count and `:895-897` validates every encoded revoke page. | Landed | Keep historical cross-transaction tests deferred; do not restore their runtime. |
| Mutation lifecycle | `crates/tx-ext4/src/mutation_lifecycle.rs:54-65` keeps cross-yield journal/deferred-free state and only child request IDs; it explicitly does not retain an L4 data lease or `Guard`. | Landed owner | Extend this owner; do not introduce caller-managed discard or direct settlement. |
| L4 custody | `crates/tx-subsystems/src/io_manager/page/manager.rs:74-115` retains each admitted `OwnedFileIoRequest` until terminal removal. | Landed foundation | Multi-page bundles must remain here after graph admission. |
| L6 execution | `crates/tx-subsystems/src/page_backed/block_runtime.rs:36-94` owns queue/tracker/depth/tag state and emits remove-first completion facts. | Landed foundation | L6 must not release PageDataLease or mutate PageSlot. |
| Role images, 4096-MiB rustc geometry, resolver shim | Current `xtask/src/ext4/`, `tools/ext4/`, `tools/images/build-riscv64-glibc-resolv-shim.sh`, and `tools/ext4/workloads/rustc-kernel-build.json` already carry the adapted surfaces. | Landed or fixture-only | Require measured guest receipts; never treat the fixture contract as evidence. |
| `PageDataLease` | `crates/tx-subsystems/src/page_backed/lifecycle.rs:10-31` stores `Box<[PageLease]>` but constructs one page and projects only `pages[0]`. | Current candidate | Generalize in place to retained multi-page segments and per-page generations. |
| Writable mmap freeze | No current active type binds writable-PTE exclusion, PageSlot generations, TLB acknowledgement, and I/O terminal settlement. | Needs design | Add a VM-owned owned token; never port a borrowed RangeLock/PTE experiment directly. |
| Historical Mount/bdevfs/PageBacked/JBD2/syscall/exec/reactor changes | The dirty sources predate the current SMP, ResidentRoot, lifecycle, and L4/L6 ownership. | Rejected | Preserve only focused tests or values that can be expressed through current owners. |

## Required ownership sequence

```text
VM RangeLock + generation recheck
  -> write-protect/unmap writable PTEs
  -> TLB shootdown acknowledgement
  -> owned writable-freeze token (no Guard or RangeLock held)
  -> PageBacked multi-page PageDataLease
  -> ext4 immutable layout/JBD2 plan
  -> L4 graph admission and retained terminal bundle
  -> L6 BIO dispatch and value-only node completion
  -> L4 terminal aggregation
  -> PageBacked generation validation and PageSlot transition
  -> ext4 checkpoint and deferred-free settlement
  -> VM restores or revalidates writable mappings
```

`ResidentRoot` and EBR may publish and retire immutable resident-binding roots.
They do not replace any step in this sequence. A `Guard` is CPU-bound and
cannot cross yield or migration; DMA visibility, PageSlot generation, PTE and
shootdown state, and JBD2 phases each retain their existing owner.

## Migrated kernel-heap slice

The target now carries a reviewed adaptation of the RCU-on-VM `LargeArena`:

- `SlabPageProvider::preferred_large_arena_pages()` selects a provider-owned
  boot reserve (`crates/tx-substrate/src/slab.rs:79-83`).
- `SlabHeap::init()` reserves the arena before normal fragmentation grows
  (`crates/tx-substrate/src/slab.rs:107-122`).
- large allocations try the arena and retain the original contiguous-run
  fallback (`crates/tx-substrate/src/slab.rs:223-267`).
- the boot reserve halves on allocation failure (`crates/tx-substrate/src/slab.rs:270-283`).
- the fixed bitmap arena is bounded to 4096 pages and serializes allocation and
  release (`crates/tx-substrate/src/slab.rs:349-459`).
- global sizing selects 4096, 2048, or 512 pages only for sufficiently large
  frame pools (`crates/tx-substrate/src/slab.rs:680-694`).

The target adaptation also fixes two source issues: alignment is calculated
from the absolute PPN, and a failed fallback direct-map conversion releases its
reserved run. Tests cover allocator fragmentation, half-size reserve retry, and
an arena whose base PPN is not aligned to a 16-KiB request
(`crates/tx-substrate/tests/slab.rs:286-420`).

Verification completed in the target:

- `rustfmt --edition 2024 crates/tx-substrate/src/slab.rs crates/tx-substrate/tests/slab.rs`
- `cargo test -p tx-substrate --test slab -- --test-threads=1`: 8 passed
- `cargo test -p tx-substrate --lib`: 51 passed
- `cargo -q xtask unit`: tx-shims 663, tx-kernel 119, tx-ext4 73 with 2
  ignored, and tx-scripts 168 passed
- `cargo xtask lint invariants ext4-no-direct-home-write`: 0 violations
- `cargo xtask full-build --target rv64-qemu`: passed
- SMP1 and SMP2 smoke QEMU runs both observed the required boot sentinel
- `cargo xtask progress validate`: 49 progress records passed
- `git diff --check` and `cargo fmt -p tx-substrate -- --check`: passed

Whole-tree `cargo xtask lint docs` remains blocked by 2116 duplicate-tag and
broken-link findings under the pre-existing untracked `.io-submission-patch/`
copy. Whole-workspace `cargo fmt --check` also reports unrelated pre-existing
format drift. Neither blocker is in the heap or new progress-record scope.

This is a kernel-heap fragmentation mitigation only. It does not add anonymous
reclaim, swap, graceful late-OOM semantics, or prove a guest Cargo build.

## Execution plan and next gate

The executable dependency order is recorded in
`docs/progress/plans/2026-08-12-ext4-to-smp-migration.json`:

1. freeze and classify the worktrees (complete);
2. refresh only missing current-oracle inputs;
3. implement real multi-page `PageDataLease` custody;
4. design and implement the VM writable-PTE freeze lease;
5. converge both through current L4/L6 and `MutationHandle`; and
6. run host, SMP1/SMP2, full non-LTP OSComp, Linux/e2fsck, crash, and pinned
   xfstests acceptance.

The next implementation action is step `e3-multi-page-data-lease`. It can land
independently of the VM design update, but production mmap/writeback/truncate
closure cannot pass until `e4-writable-pte-freeze-lease` also completes.
