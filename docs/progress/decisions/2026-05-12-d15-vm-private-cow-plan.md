# D15 — VM-private COW for fork (plan)

Date: 2026-05-12
Status: planned

## Why

`busybox sh` segfaults at PC=0 after the first fork (visible as
`/ # ls /` in `tools/shell-tests/busybox-prompt.txt`). Root cause is
in commit `a592d9e`: `vm::execution::fork_aspace` copies recipes from
parent into child but never duplicates the underlying anon-page
contents. The child's first PrivateAnon write fault allocates a fresh
zero page, losing every saved register musl spilled to the stack
before the clone-syscall. The first function epilogue then loads
`ra = 0` and `ret`s to PC=0.

## What the architecture commits to

(Per docs/design — restated for grep convenience.)

1. **No stacked PageContainers.** MAP_PRIVATE COW is a VmEntry / pmap
   problem, not a PC problem. A write to a private mapping creates a
   fresh private Frame and does **not** install it back into the
   source PC.
2. **Two COW paths, separately:**
   - *VM-private COW* (MAP_PRIVATE file/tmpfs/memfd, fork COW of
     anon/private, /dev/zero MAP_PRIVATE) — frame lives in VmEntry's
     private-frame tracking; recipe + pmap consult it on fault.
   - *PC-level shared-frame COW* (reflink) — `pc.pages.install_if_match`
     replaces one radix entry in one PC; source PC is untouched.
3. **Reflink is not a PC method.** Filesystem code owns policy; PC
   only allows multi-PC pointers at the same Frame.
4. **PTE install must revalidate the recipe / VmEntry binding** before
   publishing. PTEs are derived; recipes are authoritative.

## Slice scope (this D15)

VM-private COW only. PC-level reflink COW is out of scope.

### Sub-slices

1. **Conditional radix ops on the page store.** Before any COW lands,
   `PageContainer.pages` (or the equivalent private-frame store) needs:
   - `install_if_absent(off, frame, guard) → InstallResult`
   - `install_if_match(off, expected, new, guard) → InstallResult`
   - `withdraw_if_match(off, expected, guard) → InstallResult`

2. **Frame refcount triple.** `FrameMeta { map_count, cache_ref,
   pin_count, state }`. Frame freed only when all three are zero.
   - `cache_ref`: PC radix / VmEntry private-pages references.
   - `map_count`: PTE references.
   - `pin_count`: DMA / user pin references.

3. **VmEntry private-frame tracking.** Either a per-VmEntry radix or
   a per-aspace `(UserRange, offset) → Frame` index that survives
   recipe re-publish. Likely lives next to `recipes` on
   `AddressSpace`, not inside `VmEntry` (which is value-typed for EBR
   tree clones).

4. **Fault path.** `handle_private_write_fault`:
   1. `lookup` in private-frame tracking; if hit, install writable PTE
      after revalidating VmEntry.
   2. On miss: materialize source from backing PC at AccessKind::Read,
      `frame_alloc_zeroed`, `copy_frame`, `install_if_absent` in
      private-frame tracking, install writable PTE.

5. **fork_aspace.** For each private recipe:
   1. Walk parent's private-frame tracking AND parent's pmap for
      page-backed reads still present in parent's pmap.
   2. Allocate a fresh Frame for the child.
   3. `copy_frame_contents(parent_ppn, child_ppn)`.
   4. `install_if_absent` in child's private-frame tracking.
   5. Tear down parent's PTEs (existing behaviour); child does NOT
      pre-install PTEs — they materialise on first fault from the
      private-frame tracking entry.

6. **Teardown.** When a recipe range goes away (unmap, exit_group),
   walk private-frame tracking entries in that range, drop them
   (decrementing `cache_ref` to zero where applicable).

### Acceptance tests

(Per the design guidance.)

- MAP_PRIVATE file mapping: parent writes mapped page, file content
  unchanged via fd, PC radix entry unchanged.
- MAP_SHARED file mapping: parent writes, fd-read sees new content,
  dirty bit set.
- fork anon COW: parent allocates page, fork, child writes — parent
  still sees old bytes, child sees new bytes, no stacked PC.
- Concurrent COW race: two writers fault the same shared PC page,
  exactly one `install_if_match` wins, loser retries.
- Truncate vs fault: PTE either succeeds pre-truncate or
  fails/retries post-truncate; no stale PTE survives.
- Reclaim vs read: clean file page reclaimed concurrently with read;
  either reader uses pinned old Frame or rematerialises; no freed
  Frame is ever observed.

## What `a592d9e` already lands

The diagnostic infrastructure that makes this slice verifiable:

- `FAULT_SIGSEGV_{ADDR,ACCESS,HITS,PID}` — confirm init.pid=1 segfaults
  at `addr=0 access=Execute`.
- `SYS_CLONE_PARENT_PC_{ENTRY,EXIT}` — confirm clone preserves parent
  saved-context PC (1106836 → 1106836).
- `tx-hal-riscv64-qemu-virt --features trap-trace` — every userspace
  entry/trap with PC/a0/sp; the boot log is a full user-mode trace.

Re-running `cargo xtask shell-test --target rv64-qemu --script
tools/shell-tests/busybox-prompt.txt` after the slice lands should
walk `true / echo hello-v3 / pwd / ls / / echo pipe-ok | cat / true
&& echo done` end-to-end with no SIGSEGV.

## Out of scope for D15

- PC-level reflink COW (`install_if_match` on a PC radix where two
  PCs share a Frame).
- True share-RO CoW with PTE downgrade (this slice is eager-copy
  semantics for private-frame tracking — still correct fork
  semantics, just allocates upfront).
- Dirty/writeback path for shared file mappings.
- Reclaim under memory pressure.

## Estimated breakdown

- Sub-slice 1 (conditional radix ops): ~half day.
- Sub-slice 2 (frame refcount triple): ~half day; touches
  `page_allocator` and every materialize call site.
- Sub-slice 3 (private-frame tracking on AddressSpace): ~half day.
- Sub-slice 4 (fault-path rewrite): ~half day.
- Sub-slice 5 (fork_aspace populate): ~few hours.
- Sub-slice 6 (teardown): ~few hours.
- Tests + retire diagnostic instrumentation: ~half day.

Total: ~3 person-days.
