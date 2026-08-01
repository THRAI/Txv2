# VM Waitable Prefault Design

**Status:** approved for implementation planning on 2026-08-01

**Goal:** make eager user-range reserve/prefault a real waitable `StepOp`,
preserve every RangeLock and file-page wait as `Yield(OnWaitSource)`, and prove
the cold file-backed VMA path from initial block through PTE publication.

## Contracts

- `Blocked` is the retired v1 spelling. New code represents the same condition
  as `StepOutcome::Yield { progress, shape: OnWaitSource { ... } }`.
- No epoch guard, `IdentRef`, RangeLock reservation, or page reservation may be
  retained across a yield.
- A wake is only a retry hint. Resume reacquires a fresh `Materializer`
  reservation and proves that the authoritative recipe still permits the
  publication before publishing a PTE.
- `PageContainer` owns file-page residency and fetch state. VM owns recipes,
  RangeLock coordination, `MapPin`, and PTE publication.
- Eager prefault is not a range snapshot. The later copy-user operation remains
  authoritative if a concurrent mapping change invalidates a prefaulted PTE.

## ReserveUserRangeOp

`ReserveUserRangeOp` becomes a stateful, multi-step operation with a page
cursor. It uses `PageProgress`, removes both `OneShotStepOp` implementations,
and exposes a constructor so callers cannot initialize the cursor incorrectly.

Each invocation processes one page:

1. If the current PTE already permits the requested access, advance the cursor
   and return `Continue(PageProgress::new(1))`.
2. Acquire a `Materializer` reservation and observe the authoritative recipe.
   A RangeLock conflict returns its exact release wait source as `Yield`.
3. Drop the reservation and materialize the backing. A cold file page returns
   the exact PageBacked/file-I/O wait source as `Yield`.
4. Reacquire `Materializer`, revalidate the recipe/materialization pair through
   the generation fast path or field-comparison slow path, and publish the PTE
   plus `MapPin`. A publication-side RangeLock conflict yields the RangeLock
   source and retries the same page.
5. Advance the cursor only after an adequate PTE already exists or publication
   commits. Return `Continue(PageProgress::new(1))`; the next invocation returns
   `Done(())` after the cursor reaches the end.

The op retains only `&AddressSpace`, range/access values, a numeric cursor, and
an optional owned `VmFaultOutcome` across yields. The outcome contains an owned
recipe snapshot and generation stamp; it is not a guard, witness, RangeLock
reservation, or page reservation.

## Recipe Revalidation Fast Path

`RecipeIndex` gains a monotonic `AtomicU64` publication sequence. It is a full
machine word, not a changed bit: a bit would permit ABA after two rewrites and
cannot be consumed safely by multiple waiters.

The sequence uses a seqlock-shaped protocol:

1. A reader loads the sequence, observes the recipe under its epoch guard, and
   loads the sequence again. Equal even values produce a stamped
   `VmFaultOutcome`; an interleaving writer produces an unstamped owned outcome
   that is still usable but must take the slow path. The reader never spins on
   an odd value inside a bounded step.
2. A writer prepares the replacement before touching the sequence, changes the
   sequence from even to odd, commits the already-prepared root, then publishes
   the next even value with release ordering. The mutation lock serializes
   writers, and the root commit remains the recipe visibility point.
3. After a wait, publication reacquires `Materializer` and compares a captured
   stable sequence, when present, with the current even sequence.
4. Equal stable sequences skip the recipe-tree lookup but still validate the local
   outcome/materialization backing and page-index relationship.
5. Different sequences run the existing full target-entry comparison. If the
   target entry is still equivalent, an unrelated VMA changed and publication
   continues. If it differs, the owned materialization is dropped and the
   operation restarts from recipe resolution.

This makes the normal file-completion path one generation comparison after
reservation reacquisition. It does not use a published-root pointer as the
token because EBR reclamation and allocator reuse would permit pointer ABA.

## Syscall Integration

The async PageBacked paths for buffered read, buffered write, and direct I/O
drive `ReserveUserRangeOp` through `tx_scripts::drive` in `DriveMode::Waiting`.
User-page fault resolution is allowed to wait even when the file descriptor is
`O_NONBLOCK`; descriptor nonblocking policy applies to the file operation, not
to the CPU/user-memory page-fault mechanism.

The synchronous PageBacked `writev` optimization remains a hot-only lane:

- it claims an iovec only when the iovec metadata and payload pages already
  have adequate PTEs;
- otherwise it returns `None` so `dispatch_full_syscall` uses the existing
  async `writev` path;
- a destination PageContainer yield before any bytes are committed also falls
  through to async dispatch;
- after partial progress it returns the partial byte count, preserving Linux
  `writev` semantics and avoiding duplicate writes.

No wait is translated to `EAGAIN` merely because the optimization was entered.

## Demand-Fault Wait Routing

`fault_script` currently preserves the PageBacked `WaitToken` but waits on the
RangeLock endpoint unconditionally. The corrected script distinguishes:

- the AddressSpace RangeLock source: wait directly on its owned endpoint;
- a registered subsystem wait source/raw queue/raw port: resolve it through the
  registered-source router;
- a substrate `WaitSource` registered by PageBacked/file I/O: resolve it by its
  exact `WaitSourceId` through the VM wait adapter;
- an unknown source: fail closed as `VmFaultError::WouldBlock`; never wait on a
  different carrier.

After the exact source wakes, the script retains the owned `VmFaultOutcome` and
retries materialization/publication directly. An unchanged recipe sequence uses
the fast path. A sequence mismatch invokes full target-entry revalidation; only
an incompatible target recipe restarts the outer loop at recipe resolution.

## Tests

Tests use a stateful file-backing fixture with a real registered wait source,
an atomic ready flag, and a fetch counter.

- Reserve on a cold shared file-backed VMA yields the fixture's exact source,
  leaves the PageContainer nonresident, and publishes no PTE.
- Firing that source and retrying the same op reaches `Done`, installs one
  resident page, and publishes a readable PTE.
- A two-page range retains first-page progress while the second page yields;
  resume does not refetch or republish the first page.
- A conflicting writer yields the RangeLock release source; releasing it lets
  the same op complete.
- Unmapped and protection-mismatch ranges remain terminal `EFAULT` cases.
- A cold demand-fault future stays pending until the file source fires, then
  retries and publishes the PTE. Firing only the RangeLock source must not
  complete it.
- An unchanged sequence after file completion skips the recipe-tree slow path;
  an unrelated VMA rewrite takes the slow path but still publishes; a target
  rewrite is rejected and restarts resolution; two rewrites cannot produce an
  ABA false match.
- Shim/lint tests prove all async prefault callsites use `drive`, and the
  synchronous `writev` lane falls back instead of returning `EAGAIN`.

## Copy And Zero-Copy Boundary

This change removes false terminal errors and preserves waitability; it does
not turn ordinary buffered I/O into zero-copy I/O.

- File-backed `mmap` is the PC-to-userspace zero-copy path: the PTE maps the
  PageContainer frame directly.
- `O_DIRECT` is the device-to-user-page zero-copy path: prefault establishes
  PTEs, `DmaPin` retains user pages, and `BioVec` describes them to the device.
- Buffered `read/write` retain one semantic memcpy between a PageContainer
  frame and a user frame. Replacing an arbitrary user PTE would change its VMA
  backing, COW behavior, partial-page contents, and post-syscall independence.
- The existing same-physical-page `copy_nonoverlapping` alias risk is a
  separate correctness task: exact self-copy can be a no-op and partial overlap
  needs overlap-safe copying, but neither is a general zero-copy design.

## Non-Goals

- File readahead or adjacent-PTE prefault policy.
- A new remap/loan userspace ABI.
- Converting bootstrap user-copy helpers outside the reserve/prefault and
  `writev` paths.
- Changing MAP_PRIVATE COW, PageContainer ownership, or O_DIRECT alignment
  rules.
