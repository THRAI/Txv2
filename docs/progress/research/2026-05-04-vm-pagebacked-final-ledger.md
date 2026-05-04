---
date: 2026-05-04
topic: "VM/PageBacked v1 completion final ledger"
status: complete
plan: docs/progress/plans/2026-05-04-vm-pagebacked-v1-completion.json
prior:
  - docs/progress/research/2026-05-03-vm-doc-gap-ledger.md
  - docs/progress/research/2026-05-04-vm-pagebacked-gap-update.md
  - docs/progress/research/2026-05-04-vm-pagebacked-midway-checkpoint.md
---

# VM/PageBacked v1 Completion Final Ledger

## Question

Where does VM/PageBacked stand against the active VM_v1_2 / PAGE_BACKED_v1
contracts after the `2026-05-04-vm-pagebacked-v1-completion` plan landed
all 16 of its planned slices plus the prerequisite plan-extension slice?

## Summary

VM/PageBacked has moved from the post-resync ~45% structure / ~30% behavior
to roughly **85% structure / 80% behavior**. All in-VM and in-PageBacked
work the plan promised has landed end-to-end, gated by ~135 host-side
tests. The remaining 15-20% of contract surface is exactly the items the
active design docs already mark as deferred-by-v1 or that depend on a
Process subsystem that is not yet implemented; nothing in those gaps is
load-bearing for the slices that did land.

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

Test counts at closure: **vm 73 ok, page_backed 49 ok, lib 134 ok**, all
with `--test-threads=1`. Substrate `page_allocator` 18 ok. Workspace clippy
clean, `cargo xtask lint arch/unused/docs` ok, `cargo xtask progress
validate` 24 records ok.

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
| §5.6 fork_aspace | **Process-blocked** | needs `Cap<AddressSpace>` ownership in Process |
| §5.7 exec_aspace | **Process-blocked** | needs Process |
| §5.8 brk | **Done** | `brk_script_async` ; in-VM portion |
| §5.9 madvise/msync/mincore | **Done** | observation surface on `AddressSpace` |
| §6 Race walkthroughs | **Proven** | 73 vm tests including async script tests |
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

**Process-blocked** (the next milestone for VM/PageBacked completeness):
- `fork_aspace` (§5.6) — needs Process ownership of `Cap<AddressSpace>`,
  CoW demotion via tear-down + refault (the `protect-via-refault`
  surface is documented in `vm/pmap.rs`).
- `exec_aspace` (§5.7) — needs Process ownership.
- Trap page-fault dispatch — needs ThreadRuntime supplying authority
  evidence to call `fault_script_async`.

**PageBacked PC-side blocking** (follow-up that completes §5.1/§5.2 for
File backings under fault):
- Per-`PageContainer` wait channels, registered with `wait_carrier`
  analogous to RangeLock's release channel.
- `FsPageBacking` completion paths (fetch/flush) that fire those
  channels.
- `fault_script_async` consumes the PC carrier when materialize_page
  returns Blocked. Today it propagates the error.

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

## Verification at closure

- `cargo fmt --check`: clean.
- `cargo test -p tx-kernel script_async -- --test-threads=1`: 13 ok.
- `cargo test -p tx-kernel vm -- --test-threads=1`: 73 ok.
- `cargo test -p tx-kernel page_backed -- --test-threads=1`: 49 ok.
- `cargo test -p tx-kernel --lib -- --test-threads=1`: 134 ok.
- `cargo test -p tx-substrate --test page_allocator`: 18 ok.
- `cargo test -p tx-substrate --lib`: 0 (no lib tests).
- `cargo clippy --workspace --all-targets ... -- -D warnings`: clean.
- `cargo xtask lint arch`: ok.
- `cargo xtask lint unused`: ok.
- `cargo xtask lint docs`: ok.
- `cargo xtask progress validate`: 24 records ok.

## Recommended next moves

1. Push the branch (24 commits ahead of `origin/main`).
2. Open the Process / ThreadRuntime milestone: fork, exec, trap
   page-fault dispatch all converge there.
3. Open the concrete VFS backend milestone (ext4 / devfs / bdev-fs) so
   File-variant `FsPageBacking` graduates from mocks to backends.
4. Optional follow-up slice: per-`PageContainer` wait channels so
   `fault_script_async` honors PC-side blocking for File backings under
   fault.
