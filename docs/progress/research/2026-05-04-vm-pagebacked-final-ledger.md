---
date: 2026-05-04
topic: "VM/PageBacked v1 completion final ledger (post-audit revision)"
status: complete
plan: docs/progress/plans/2026-05-04-vm-pagebacked-v1-completion.json
prior:
  - docs/progress/research/2026-05-03-vm-doc-gap-ledger.md
  - docs/progress/research/2026-05-04-vm-pagebacked-gap-update.md
  - docs/progress/research/2026-05-04-vm-pagebacked-midway-checkpoint.md
---

# VM/PageBacked v1 Completion Final Ledger

## Revision history

- **Initial closure** at `f8181c1` recorded 17 plan steps complete and
  estimated ~85% structure / ~80% behavior. Classified `fork_aspace` and
  `exec_aspace` as Process-blocked.
- **Audit revision** (this rev): a re-read of VM_v1_2 §5.6, §5.7, §3.1,
  §9.5 caught three gaps the initial closure missed. Three plan-extension
  slices landed in `a70f4a0` (fork+exec) and `e8ed0a4` (doc-spelling
  polish + full_user_v1 fork serialization), bringing the plan to 20
  steps complete. The "Process-blocked" classification was wrong:
  fork_aspace and exec_aspace operate entirely on AddressSpace
  primitives. Revised completion estimate is in the table below.

## Question

Where does VM/PageBacked stand against the active VM_v1_2 / PAGE_BACKED_v1
contracts after the `2026-05-04-vm-pagebacked-v1-completion` plan landed
all 17 of its planned slices, the resolver plan-extension prerequisite,
and the post-closure audit follow-ups (fork_aspace, exec_aspace,
acquire_step rename, full_user_v1 lock)?

## Summary

VM/PageBacked has moved from the post-resync ~45% structure / ~30% behavior
to roughly **92% structure / 88% behavior**. All in-VM and in-PageBacked
work the plan and the audit follow-ups promised has landed end-to-end,
gated by ~138 host-side tests. The remaining contract surface is items
the active design docs explicitly defer (rmap, hugetlb, userfaultfd,
in-place mprotect retag, reclaim policy, writeback scheduling), items
that depend on subsystems not yet built (concrete VFS backends,
ThreadRuntime trap dispatch wiring), or stylistic polish (StepOutcome
return type, true persistent BTree, exposed `rewrite_range` primitive).

## Plan steps complete

| Slice | Commit |
|---|---|
| `gap-ledger-refresh` | `cea4d25` |
| `user-buffer-byte-copy` | `4cf0b37` |
| `vm-fault-pc-size-checks` | `1755c5c` |
| `vm-pmap-walk-protect-surface` + `wait-aware-step-outcome` | `a880398` |
| `madvise-msync-mincore` | `e4b46cd` |
| `pagebacked-fallocate` | `08267a5` |
| `partial-page-byte-fidelity` | `cb844bd` |
| `cross-variant-scripts` + `mock-fs-pagebacking` | `fa6d9f5` |
| `persistent-epoch-recipes` | `818771a` |
| `reflink-cow-scaffold` | `5dbaf5b` |
| `waittoken-channel-resolver` (plan-extension prerequisite) | `49c2e87` |
| `mmap-script-async` | `3268dd0` |
| `munmap-mprotect-mremap-scripts` | `8649f57` |
| `brk-script` | `ac92164` |
| `fault-script-async` | `8e24647` |
| `ledger-and-status-final` (initial closure) | `f8181c1` |
| `fork-aspace` + `exec-aspace` (audit follow-up) | `a70f4a0` |
| `vm-doc-polish-and-full-user-range` (audit follow-up) | `e8ed0a4` |

Test counts at this revision: **vm 77 ok, page_backed 49 ok, lib 138 ok**,
all with `--test-threads=1`. Substrate `page_allocator` 18 ok. Workspace
clippy clean, `cargo xtask lint arch/unused/docs` ok, `cargo xtask
progress validate` 24 records ok.

## VM_v1_2 contract status

| Section | Status | Notes |
|---|---|---|
| §1 Authoritative bindings | **Done** | recipes authoritative, PTEs derived |
| §2 AddressSpace structure | **Done** | zone entity + recipes + pmap + RangeLock |
| §3 RangeLock | **Done** for v1 | bounded reservation set; release fires `RANGE_LOCK_RELEASE_MASK` channel; cross-async-wait discipline honored by `*_async` wrappers |
| §4 VmEntry incl. zero frame | **Done** | three `VmBacking` variants |
| §5.1 fault_script | **Done** | `fault_script_async` ; PC-side blocking is follow-up |
| §5.2 mmap | **Done** | `map_script_async` |
| §5.3 munmap | **Done** | `unmap_async` |
| §5.4 mprotect | **Done** | `protect_async` |
| §5.5 mremap | **Done** for v1 | `remap_async` ; disjoint-only per §9 |
| §5.6 fork_aspace | **Done** | `AddressSpace::fork_aspace::<P>` ; `ExclusiveWriter` on `full_user_v1()` per §9.5; recipes cloned via `Cap` refcount bumps; MAP_PRIVATE PTEs torn down in parent for refault-CoW |
| §5.7 exec_aspace | **Done** | `AddressSpace::exec_aspace` + `teardown_all_pmap` ; recipe-tree replacement remains caller-side because the new image's recipe shape comes from the (Process-side) exec image loader |
| §5.8 brk | **Done** | `brk_script_async` ; in-VM portion |
| §5.9 madvise/msync/mincore | **Done** | observation surface on `AddressSpace` |
| §6 Race walkthroughs | **Proven** | 77 vm tests including async script tests |
| §7 MAP_PRIVATE CoW | **Done** | byte-copy via FrameCopier |
| §8 Entry split/merge | **Done** | |
| §9 Tech debt 9.1–9.11 | **Deferred by design** | rmap, hugetlb, userfaultfd, in-place mprotect, fairness, etc. |

VM publication via persistent EBR: lock-free reads under `epoch::Guard`,
atomic-swap writes, retire through `tx_substrate::epoch::retire_raw`.
Satisfies §1.2 publication rule with guard-scoped reader lifetimes.

VM trap page-fault dispatch: **ThreadRuntime-blocked**. The async
`fault_script_async` is wired but the kernel trap entrypoint that funnels
faults into it is part of the trap/Process integration slice.

## PAGE_BACKED_v1 contract status

| Section | Status | Notes |
|---|---|---|
| §2 RNodeBacking | **Interface shell** | three-variant dispatch in place |
| §3 PageContainer + Kinds | **Done** | Anon/File/Device |
| §4.1–4.3 Lifecycle | **Done** for v1 | reclamation §8 deferred |
| §4.4 Size and bounds | **Done** | dynamic `PC.size` |
| §5.1 Read | **Done** | copyless `step_read` + byte-accurate `step_read_to_user<H>` |
| §5.2 Write | **Done** | copyless `step_write` + `step_write_from_user<H>` |
| §5.3 Truncate | **Done** | partial-EOF tail-zero on shrink |
| §5.4 Fsync | **Done** | dirty page flush + backing fsync |
| §5.5 Fallocate | **Done** | grow-without-materialize, Device EINVAL |
| §6 FsPageBacking | **Trait shell + mocks** | `LifecycleFs`/`RecordingFs`/`BlockingFs` cover dispatcher; concrete ext4/devfs/bdev-fs is its own milestone |
| §7 Reflink + 7.1–7.4 | **Scaffolding** | `install_shared_page` + `cow_replace_into_private` ; full cross-RNode reflink waits on a backend with refcount support |
| §8 Reclaim | **Deferred by design** | v1 debt |
| §9.1 splice | **Deferred** | needs Pipe StructBacked |
| §9.2 sendfile | **Deferred** | needs Pipe StructBacked |
| §9.3 copy_file_range | **Done** | `step_copy_file_range` over PageBacked variants |
| §10 VM ↔ PageBacked | **Done** | `materialize_pagebacked` dispatch |
| §12 Open questions | **Deferred by design** | |

User-buffer byte transfer uses the substrate `FrameKernelAddr` hook
(installed at boot via `frame_kernel_addr_direct_map` and at host-test
init via `kernel_addr_for_test`). Errno gained `EFAULT`.

## What's left, classified

**Process- / ThreadRuntime-side orchestration** (out of scope for VM):
- Whose `Cap<AddressSpace>` does each thread hold; how exec replaces
  the binding; how fork attaches the child AS to the new process —
  Process subsystem's job. The VM-side primitives (`fork_aspace`,
  `exec_aspace`) are landed and ready.
- Trap page-fault dispatch — needs ThreadRuntime supplying authority
  evidence to call `fault_script_async` from the kernel trap entry.

**PageBacked PC-side blocking** (follow-up that completes §5.1/§5.2 for
File backings under fault):
- Per-`PageContainer` wait channels, registered with `wait_carrier`
  analogous to RangeLock's release channel.
- `FsPageBacking` completion paths (fetch/flush) that fire those
  channels.
- `fault_script_async` consumes the PC carrier when materialize_page
  returns Blocked. Today it propagates the error.

**Stylistic / optimization gaps** (semantically equivalent to today):
- §3.1 `acquire_step` returns `AcquireResult` rather than the
  documented `StepOutcome<RangeGuard>`. Same shape (Done + Blocked + Err
  vs Acquired + WouldBlock plus error folded into surrounding result
  types). A future StepOutcome integration can fold them.
- §2 says recipes is a "persistent BTree" with O(1) clone. Our impl
  publishes `AtomicPtr<BTreeMap>` under EBR — reads are lock-free
  borrows (matches the doc), writes still clone the BTreeMap (doc says
  O(1) path-copy clone). Performance, not correctness.
- §8 `recipes::rewrite_range(range, list)` substrate primitive is
  hidden inside the per-op rewriters (`commit_map`, `unmap`, `protect`,
  `remap_disjoint`); a unified primitive could be lifted out as a
  refactor.
- `FULL_USER_V1_TOP = 1 << 38` is a v1 constant; the per-platform user
  VA cap should replace it once Process / boot finalize the cap.

**v1-deferred by the design docs themselves** (not load-bearing for v1):
- VM_v1_2 §9: rmap, hugetlb, userfaultfd, mlock-as-observation,
  in-place mprotect retag, fairness tuning, MAP_HUGETLB / MAP_STACK /
  MAP_UNINITIALIZED, MADV_WILLNEED no-op, single WaitToken per
  RangeLock.
- PAGE_BACKED_v1 §8 reclaim policy, §12.1–12.7 open questions
  (writeback scheduling, network FS, large pages, character device
  binding).

**Concrete backends** (own milestone):
- ext4, devfs, bdev-fs concrete `FsPageBacking` implementations.
  v1 mocks (`LifecycleFs`, etc.) cover the dispatcher.

## Design choices recorded

- **Persistent EBR-backed publication** (`818771a`): tx-substrate's
  existing `epoch::Guard` + `epoch::retire_raw` was sufficient; no new
  persistent-BTree crate or hand-rolled structure needed. `RecipeIndex`
  publishes via `AtomicPtr<RecipeTree>` + writer mutation `SpinMutex`,
  retires old trees through EBR.
- **WaitToken → Channel resolver** (`49c2e87`): a `wait_carrier`
  registry keyed by carrier id. Subsystems that produce blocked
  outcomes register their `tx_reactor::wait::Channel` and embed the
  returned id in `WaitToken`. Async wrappers convert tokens via
  `wait_on_token`. Test placeholders with arbitrary token bits return
  `None` rather than UB.
- **RangeLock release wake integration** (`3268dd0`): each `RangeLock`
  owns a `Channel` registered at construction. Every release fires the
  channel; `WouldBlock<'a>` carries a `&'a RangeLock` and exposes
  `wait_token`. This is the template the script wave reuses.
- **Async script template**: try-acquire, on WouldBlock extract token
  + drop blocked + await + retry. Inner sync helpers preserved as
  compatibility surface. Used identically by `map_script_async`,
  `unmap_async`, `protect_async`, `remap_async`, `brk_script_async`,
  `fault_script_async`.
- **Fork serializes via `full_user_v1` ExclusiveWriter** (`e8ed0a4`):
  `fork_aspace` acquires a single ExclusiveWriter on the conservative
  `[0, 1 << 38)` user range before snapshotting parent recipes,
  satisfying VM_v1_2 §9.5 fork-serializes-parent. The 256 GiB cap fits
  inside Sv39 / Sv48 user halves; the per-platform cap will replace
  this constant when finalized.
- **VM-side fork/exec are not Process-blocked** (`a70f4a0`): the
  initial closure mis-classified §5.6 / §5.7 as Process-blocked.
  Re-reading the doc showed both functions operate entirely on
  AddressSpace primitives. The Process subsystem orchestrates
  *which* AddressSpace is bound to *which* threads; the VM-side
  `fork_aspace` and `exec_aspace` stand alone.

## Verification at this revision

- `cargo fmt --check`: clean.
- `cargo test -p tx-kernel script_async -- --test-threads=1`: 17 ok.
- `cargo test -p tx-kernel vm -- --test-threads=1`: 77 ok.
- `cargo test -p tx-kernel page_backed -- --test-threads=1`: 49 ok.
- `cargo test -p tx-kernel --lib -- --test-threads=1`: 138 ok.
- `cargo test -p tx-substrate --test page_allocator`: 18 ok.
- `cargo test -p tx-substrate --lib`: 0 (no lib tests).
- `cargo clippy --workspace --all-targets ... -- -D warnings`: clean.
- `cargo xtask lint arch`: ok.
- `cargo xtask lint unused`: ok.
- `cargo xtask lint docs`: ok.
- `cargo xtask progress validate`: 24 records ok.

## Recommended next moves

1. Push the branch (28 commits ahead of `origin/main` after this rev).
2. Open the Process / ThreadRuntime milestone: trap page-fault
   dispatch wires `fault_script_async` to the kernel trap entry; fork
   / exec / clone orchestrate which AddressSpace each thread sees;
   `Cap<AddressSpace>` ownership lives there.
3. Open the concrete VFS backend milestone (ext4 / devfs / bdev-fs) so
   File-variant `FsPageBacking` graduates from mocks to backends.
4. Optional follow-up slice: per-`PageContainer` wait channels so
   `fault_script_async` honors PC-side blocking for File backings under
   fault.
5. Optional follow-up slice: replace the v1 `AtomicPtr<BTreeMap>`
   recipe publication with a true persistent BTree to drop the
   per-write tree clone. Reads already match the doc; writes are the
   remaining performance gap.
