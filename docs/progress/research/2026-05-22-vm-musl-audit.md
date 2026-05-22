---
date: 2026-05-22
topic: "VM subsystem audit against VM_v1_2, musl-facing mmap/mremap behavior, and code smells"
status: complete
active_docs:
  - docs/design/03_memory-vm/VM_v1_2.md
  - docs/design/03_memory-vm/PAGE_BACKED_v1.md
  - docs/Txv3/03_STEP_MODEL_v2.md
code:
  - crates/tx-subsystems/src/vm
  - crates/tx-subsystems/src/page_backed
  - crates/tx-shims/src/linux_syscall/vm.rs
external_refs:
  - https://man7.org/linux/man-pages/man2/mremap.2.html
  - https://man7.org/linux/man-pages/man2/munmap.2.html
  - https://git.musl-libc.org/cgit/musl/tree/src/malloc/mallocng/realloc.c
---

# VM / musl audit

## Verdict

Ready: mostly for the active VM/PageBacked structure, no for full musl/Linux
VM syscall compatibility.

The current VM core is no longer a stub: `AddressSpace` has recipes, pmap, and
`RangeLock`; recipes publish through an EBR-pinned `AtomicPtr<BTreeMap>`; range
reservation uses the v3 `StepOutcome` shape; private CoW and pmap publication
are implemented enough for host tests. The remaining blockers are not in the
basic structure, but in Linux syscall semantics and a few publication/fault
edge cases that musl or OSComp-style tests will hit.

## Spec-aligned pieces

- `AddressSpace` owns exactly the VM_v1_2 fields: `recipes`, `pmap`,
  `range_lock`, and stats (`crates/tx-subsystems/src/vm/structure/address_space.rs:39`).
- Recipe publication is lock-free for readers under an epoch guard, with a
  writer mutex for replacement and EBR retirement of old trees
  (`crates/tx-subsystems/src/vm/structure/recipe.rs:19`).
- `RangeLock` is explicitly the bounded v1 interval-reservation set, exposes
  step-shaped acquire/pair-acquire, and wakes via a wait-source channel
  (`crates/tx-subsystems/src/vm/structure/range_lock.rs:1`,
  `crates/tx-subsystems/src/vm/structure/range_lock.rs:139`).
- The VM fault loop follows VM_v1_2's drop-reservation-before-wait shape:
  observe under Materializer, materialize outside the reservation, reacquire,
  and revalidate before pmap publish
  (`crates/tx-subsystems/src/vm/execution.rs:251`,
  `crates/tx-subsystems/src/vm/checks.rs:32`).
- Basic syscall dispatch coverage exists for anonymous private `mmap`,
  file-backed PageBacked `mmap`, `munmap`, `mprotect`, and `madvise`
  (`crates/tx-shims/src/linux_syscall/tests/vm_syscalls.rs`).

## Blocking musl / Linux compatibility gaps

### 1. `mremap` is not Linux-shaped enough for musl `realloc`

musl mallocng uses `mremap(old, old_len, new_len, MREMAP_MAYMOVE)` for large
reallocs when both old and new sizes are mmap-worthy. Linux `mremap` permits
growth/shrink with `MREMAP_MAYMOVE`; the fifth `new_address` argument is only
meaningful with `MREMAP_FIXED`, and `MREMAP_FIXED` itself must also include
`MREMAP_MAYMOVE`.

Current Tx shim reads but ignores flags, always parses `new_addr`, and always
builds a two-range `VmRemapRequest`:

- `crates/tx-shims/src/linux_syscall/vm.rs:473` reads `_flags` and ignores it.
- `crates/tx-shims/src/linux_syscall/vm.rs:491` requires `new_addr` page aligned
  even when `MREMAP_FIXED` is absent.
- `crates/tx-shims/src/linux_syscall/vm.rs:505` always requests
  `VmRemapRequest::new(old_range, new_range)`.
- `crates/tx-subsystems/src/vm/checks.rs:70` rejects overlap and size changes.
- `crates/tx-subsystems/src/vm/structure/recipe.rs:407` repeats the equal-size,
  disjoint-only rewrite.

Impact: a musl large `realloc` may fail, or worse, be interpreted as a forced
move to whatever value is in the fifth syscall argument even when the caller
only passed `MREMAP_MAYMOVE`. This is a top-priority libc blocker.

Expected fix shape:

- Decode `MREMAP_MAYMOVE`, `MREMAP_FIXED`, and unknown bits explicitly.
- For flags without `MREMAP_FIXED`, ignore `new_addr`.
- Support in-place shrink/grow where possible; return Linux-shaped `ENOMEM`
  when growth cannot fit and `MREMAP_MAYMOVE` is absent.
- Support may-move allocation of a fresh destination when `MREMAP_MAYMOVE` is
  present.
- Keep the current disjoint fixed move as the `MREMAP_FIXED | MREMAP_MAYMOVE`
  lane.

### 2. `MAP_SHARED | MAP_ANONYMOUS` succeeds but faults fail

Linux supports `MAP_ANONYMOUS` together with `MAP_SHARED`. Current Tx accepts
that flag shape, sets `VmEntryFlags.shared = true`, but still assigns
`VmBacking::PrivateAnon`:

- Shared/private decode: `crates/tx-shims/src/linux_syscall/vm.rs:164`.
- Anonymous backing selection: `crates/tx-shims/src/linux_syscall/vm.rs:184`.
- Shared fault path rejects `PrivateAnon`:
  `crates/tx-subsystems/src/vm/structure/types.rs:663`.

Impact: `mmap(MAP_SHARED | MAP_ANONYMOUS)` can return success, but the first
fault returns `BackingMismatch`. That violates Linux semantics and the
PageBacked spec's direction that anonymous mmap should create a fresh
PageContainer (`docs/design/03_memory-vm/PAGE_BACKED_v1.md:407`,
`docs/design/03_memory-vm/PAGE_BACKED_v1.md:939`).

Expected fix shape:

- Either reject shared anonymous mmap until implemented, or preferably route
  it to a shared anonymous `PageContainer`.
- Keep private anonymous optimized through `VmBacking::PrivateAnon` only if the
  design explicitly accepts that as the private-anon implementation strategy.

### 3. File-backed fault waits are still collapsed instead of yielded

VM_v1_2 and PAGE_BACKED_v1 say file-backed fault materialization may block and
the fault script must drop the reservation, wait, and retry. The current code
documents the gap, but the synchronous bridge collapses non-Done file fetch
outcomes to `MissingPage` or `EAGAIN`:

- Gap comment: `crates/tx-subsystems/src/vm/execution.rs:171`.
- `materialize_page_for_fault` treats non-Done file materialization as
  `MissingPage`: `crates/tx-subsystems/src/page_backed/mod.rs:407`.
- The wait-preserving lower function exists, but the VM fault path does not
  consume it as a yield: `crates/tx-subsystems/src/page_backed/mod.rs:489`.

Impact: anonymous and already-cached paths can work, but true file-backed
faults from ext4/tmpfs can degrade into fault errors instead of scheduler
waits. This matters for loader, shared object, and mmap-heavy tests once the
backing filesystem can genuinely block.

Expected fix shape:

- Add a wait-preserving VM-facing PageBacked materialization API.
- Let `fault_script_with_ufd_dispatch` translate PageBacked
  `Yield { OnWaitSource }` into the same drop-wait-retry loop it already uses
  for `RangeLock`.

## Code smells and correctness risks

### Fork CoW demotion ignores pmap errors

`fork_aspace` shares/slices private page state and then demotes parent PTEs, but
it discards `protect_range` errors:

- `crates/tx-subsystems/src/vm/execution.rs:111`
- `crates/tx-subsystems/src/vm/execution.rs:130`
- `crates/tx-subsystems/src/vm/pmap.rs:301`

If demotion fails after the child recipe is committed, the parent can retain a
writable mapping to a frame that should now be CoW-shared. This should be a
hard error or a staged operation with rollback-before-publication semantics.

### Fault publication does not revalidate private CoW source identity

`require_fault_publication` compares `entry != outcome.entry`, but
`VmEntry::PartialEq` deliberately ignores the `private` `Cap<PrivatePageSet>`:

- `crates/tx-subsystems/src/vm/structure/types.rs:337`
- `crates/tx-subsystems/src/vm/checks.rs:40`
- `crates/tx-subsystems/src/vm/execution.rs:323`

If a fault drops the Materializer reservation to materialize and a concurrent
writer replaces the VMA with an equivalent semantic range/prot/backing but a
fresh private set, publication can pass the equality check while using a page
derived from the old private set. This needs a focused concurrency test. The
fix may be as small as comparing a stable private-set identity in the
publication witness, without making all ordinary `VmEntry` equality depend on
the cap.

### User pointer loops wrap addresses

`read_user_cstr`, `copy_in`, and `copy_out` use `wrapping_add` while walking
user buffers:

- `crates/tx-subsystems/src/vm/user_access.rs:283`
- `crates/tx-subsystems/src/vm/user_access.rs:339`
- `crates/tx-subsystems/src/vm/user_access.rs:398`

Impact: a pointer near the top of the user range can wrap to a low address and
copy from/to the wrong mapping instead of returning `EFAULT`. Replace with
checked addition and reject overflow or crossing the user-VA cap.

### `page_align_up` can overflow

`page_align_up` adds `PAGE_SIZE - 1` without checking:

- `crates/tx-subsystems/src/vm/execution.rs:32`
- Used by `brk_script`: `crates/tx-subsystems/src/vm/execution.rs:534`.

Impact: a near-`usize::MAX` requested break can wrap before range validation.
Use `checked_next_multiple_of(USER_PAGE_SIZE)` or a checked helper returning
`VmMapError::InvalidRange`.

### `MAP_SHARED_VALIDATE` and flag policy are underspecified

Tx has constants for many `MAP_*` bits, but not a real
`MAP_SHARED_VALIDATE` lane. On Linux, `MAP_SHARED_VALIDATE` has shared-mapping
semantics and rejects unknown flags; Tx currently treats `MAP_SHARED |
MAP_PRIVATE` as invalid and therefore cannot express the validate lane.

Impact: not a core musl malloc blocker, but a Linux-compatibility gap for
`MAP_SYNC` / DAX-style callers and a smell in the current flag decoder
contract.

## Musl readiness summary

- Basic private anonymous mmap syscalls are in good shape for simple libc code.
- `brk` has a syscall path and targeted tests, but still needs the checked
  alignment fix above.
- Large musl `realloc` is blocked by `mremap` semantics.
- Shared anonymous mappings are broken at first fault.
- File-backed mmap faults are not yet wait-preserving for real blocking
  backends.
- `pthread_create` remains primarily blocked outside VM by Process/Thread
  shared-state work, but its stack allocation path still depends on robust
  `mmap` semantics.

## Verification

- Passed: `cargo test -p tx-subsystems --lib vm -- --test-threads=1`
  (106 tests on merged main).
- Passed: `cargo test -p tx-shims --lib linux_syscall::tests::vm_syscalls -- --test-threads=1`
  (20 tests).
- Passed: `cargo xtask progress validate` (27 records).
- Passed: `git diff --check`.
- Not rerun: full workspace `cargo fmt --check` / broad CI. There are
  unrelated dirty files outside this VM audit scope, so leave the broader sweep
  for the owners of those changes.

## Next steps

1. Rerun full workspace formatting and broader CI once the unrelated dirty
   files in sibling syscall/process test surfaces are ready to include.
2. Use the new `mremap` and shared-anonymous mmap tests as regression coverage
   when wiring more musl allocation and pthread paths through OSComp.
3. Continue hardening true file-backed PageBacked faults against blocking
   filesystem backends as ext4/tmpfs coverage grows.
