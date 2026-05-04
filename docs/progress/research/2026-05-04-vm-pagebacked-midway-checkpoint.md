---
date: 2026-05-04
topic: "VM/PageBacked v1 completion midway checkpoint"
status: complete
plan: docs/progress/plans/2026-05-04-vm-pagebacked-v1-completion.json
prior: docs/progress/research/2026-05-04-vm-pagebacked-gap-update.md
---

# VM/PageBacked v1 Completion Midway Checkpoint

## Question

After the first eight implementation slices of the
`2026-05-04-vm-pagebacked-v1-completion` plan landed, where does the
implementation stand against the active VM_v1_2 / PAGE_BACKED_v1 contracts,
and what concretely blocks the remaining seven slices?

## Summary

10 of 17 plan steps are complete. VM/PageBacked has moved from the
post-resync ~45% structure / ~30% behavior to roughly 60% structure /
55% behavior. PageBacked-owned content is substantially complete: byte-
accurate user-buffer transfer, partial-page truncate-tail zeroing,
`step_fallocate`, and `step_copy_file_range` are landed; mincore reads the
new `VmPmap::walk_range`; PC.size feeds VM fault SIGBUS-style rejection.

Remaining slices fall into three categories. One (`persistent-epoch-recipes`)
is a structural rewrite that is correctness-equivalent to today and warrants
its own session. Four (`mmap-script-async`, `munmap-mprotect-mremap-scripts`,
`brk-script`, `fault-script-async`) require an unbuilt
`WaitToken → reactor::Channel` resolver before the async wrappers can
honor VM_v1_2 §3.6 cross-async-wait discipline. One
(`reflink-cow-scaffold`) depends on `persistent-epoch-recipes`. One
(`ledger-and-status-final`) closes the plan once the others land.

## Done

| Plan step | Commit | What landed |
|---|---|---|
| `gap-ledger-refresh` | `cea4d25` | 2026-05-04 follow-up note + plan activation |
| `user-buffer-byte-copy` | `4cf0b37` | `step_read_to_user<H>` / `step_write_from_user<H>`, substrate `FrameKernelAddr` hook, `Errno::EFAULT` |
| `vm-fault-pc-size-checks` | `1755c5c` | `VmFaultError::PageBeyondSize`, fault rejection past `pc.size_bytes()` |
| `vm-pmap-walk-protect-surface` | `a880398` | `VmPmap::walk_range`, teardown protect-via-refault rustdoc |
| `wait-aware-step-outcome` | (audit, `a880398`) | Existing five-variant `StepOutcome` already matches STEP_MODEL_v1 §2 |
| `madvise-msync-mincore` | `e4b46cd` | `AddressSpace::mincore` over walk_range, `MadviseAdvice` no-op enum, `msync` File-backed dedup-by-Cap |
| `pagebacked-fallocate` | `08267a5` | `step_fallocate` for File/Anon, EINVAL for Device + capacity-overflow, no-op for shrink/equal |
| `partial-page-byte-fidelity` | `cb844bd` | `zero_partial_eof_tail` on `step_truncate` shrink, end-to-end shrink-then-grow zero readback |
| `cross-variant-scripts` | `fa6d9f5` | `step_copy_file_range` page-by-page over PageBacked variants |
| `mock-fs-pagebacking` | (closure, `fa6d9f5`) | Closed-as-redundant; existing `LifecycleFs`/`RecordingFs`/`BlockingFs` covered the dispatcher surface |

## Test counts at the checkpoint

- `cargo test -p tx-kernel page_backed -- --test-threads=1`: 43
- `cargo test -p tx-kernel vm -- --test-threads=1`: 60
- `cargo test -p tx-kernel --lib -- --test-threads=1`: 109
- All workspace clippy/lint/format/progress-validate gates green.

## Remaining slices and what blocks them

### `persistent-epoch-recipes`

Replace the current `BTreeMap<UserVirtAddr, VmEntry>` published under a
small `SpinMutex` (where readers clone the tree) with a persistent
epoch-snapshot range index. The current implementation already satisfies
the VM_v1_2 §1.2 publication rule (readers see either all-old or all-new),
so this is an architecture upgrade rather than a correctness fix:

- Lock-free reads under `epoch::Guard`.
- Mutators path-copy nodes; old versions retire via `epoch::retire`.
- Guard-scoped `IdentRef`-shaped witnesses replace the staged-witness shims
  in `vm::checks`.

Doing this well needs a persistent BTree choice (custom path-copy, or a
crate; tx-substrate's `index` and `mutation` modules may be the right
substrate), epoch integration, and migration of every reader/writer in
`vm/structure/recipe.rs`, `vm/checks.rs`, and `vm/structure/range_lock.rs`.
Multi-session work and warrants its own plan or sub-plan; not appropriate
to start mid-session and abandon.

### Async script wave: `mmap-script-async`, `munmap-mprotect-mremap-scripts`, `brk-script`, `fault-script-async`

The plan envisioned these as `async fn` wrappers around the existing
synchronous `aspace.map_script` / `unmap` / `protect` / `resolve_fault`
helpers. To genuinely honor VM_v1_2 §3.6 (drop reservations across async
waits, re-acquire on resume), the wrappers need:

1. **`WaitToken → reactor::Channel` resolver.** `WaitToken(carrier,
   interest)` is opaque. The reactor's `WaitFuture` requires a
   `&Channel`. Without a registry mapping carrier ids to channels, an
   async wrapper has no way to convert a `Blocked(token)` outcome into a
   future to await. Building this resolver is a tx-reactor-side slice in
   its own right.
2. **`RangeLock::WouldBlock` async waiter.** The current sync helpers
   collapse `WouldBlock` to a `WouldBlock` error; the async wrappers must
   instead wait on the blocking range and retry. That requires
   `RangeLock` to expose a wait token tied to its release.

Without (1) and (2), these slices would only produce syntactic stubs
(`async fn x() -> Result { sync_x() }`) with no behavior change — pure
plan box-ticking and not useful. The right move is to spawn the resolver
slice first, then the four wrappers become genuinely valuable.

### `reflink-cow-scaffold`

Depends on `persistent-epoch-recipes` per the plan, since reflink share/CoW
counters interact with the epoch-snapshot lifecycle.

### `ledger-and-status-final`

Plan closure. Should run after `persistent-epoch-recipes` and the script
wave land; firing it at the midway point is premature.

## Recommended next moves

1. Push the current 17-commit branch as a remote checkpoint.
2. Spawn a focused tx-reactor slice for the `WaitToken → Channel` resolver
   plus `RangeLock` wait integration. This unblocks the entire async script
   wave.
3. Spawn a dedicated multi-session slice for `persistent-epoch-recipes`,
   probably with a persistent BTree design decision recorded as its own
   note before implementation.
4. After both prerequisites land, the four script wrappers and
   `reflink-cow-scaffold` become straightforward to land.
5. Close the plan with `ledger-and-status-final`.

## Verification For This Note

- `cargo xtask progress validate`
- `cargo xtask lint docs`
- `git diff --check`
