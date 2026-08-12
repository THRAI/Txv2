# ext4 Transactional Data Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route buffered file writeback, fsync and namespace mutation through
one multi-page, zero-extra-copy, crash-consistent ext4 transaction path while
extracting a reusable pure pager crate.

**Architecture:** PageBacked signs a multi-page `PageDataLease`; a pure
`FileLayoutPlanner` maps opaque lease slices and metadata inputs into
`FileIoPlan<K>`; the Tx ext4 adapter lowers that plan into the existing
`BackendBioGraph`; L4/L6 execute it and return one typed settlement to
PageBacked. Ext4 owns immutable metadata generations through journal commit and
checkpoint.

**Tech Stack:** Rust `no_std`, PageBacked, `tx-ext4-format`, new
`tx-pager-api`/`tx-ext4-pager`, JBD2 ordered mode, `BackendBioGraph`, and
I/O-manager L4/L6.

---

## File structure

- Create `crates/tx-pager-api/`: kernel-neutral ranges, opaque payload keys,
  layout requests/outcomes, metadata-read continuations and transaction DTOs.
- Create `crates/tx-ext4-pager/`: reusable ext4 extent/allocation/mutation
  planner over `tx-ext4-format` and `tx-pager-api`.
- Modify `crates/tx-subsystems/src/page_backed/`: multi-page lease owner, batch
  writeback admission and vector generation settlement.
- Modify `crates/tx-subsystems/src/fs_iface/plan.rs`: compatibility projection
  and typed graph buffer/completion values.
- Modify `crates/tx-subsystems/src/io_manager/`: graph validation, barrier
  domain, priority inheritance and terminal bundle routing.
- Modify `crates/tx-ext4/src/`: Tx adapter, transaction aggregation, namespace
  mutation, frozen metadata and production cutover.
- Modify `crates/tx-ext4-format/src/`: retain format codecs, replace ordinary
  payload bytes in mutation values with opaque plan references.

### Task 1: Generalize `PageLease` into multi-page `PageDataLease`

**Files:**
- Create: `crates/tx-subsystems/src/page_backed/data_lease.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`
- Test: `crates/tx-subsystems/src/fs_iface/plan.rs`

- [ ] **Step 1: Add failing owner/view tests**

Test a three-page lease with different generations, slicing across page
boundaries, rollback before admission, retention until one terminal settlement,
stale per-page completion, and unforgeable IDs. Target shape:

```rust
pub struct PageDataLease {
    id: PageDataLeaseId,
    container: PageContainerIdentity,
    file_range: FileByteRange,
    pages: Box<[LeasedPage]>,
}

pub struct PageDataView {
    pub id: PageDataLeaseId,
    pub file_range: FileByteRange,
    pub generations: Box<[PageGeneration]>,
    pub segments: Box<[PageSegmentRef]>,
}
```

Only PageBacked constructs/releases the capability. `PageDataView` carries no
release, dirty or resident-publication authority.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-subsystems --lib page_data_lease -- --test-threads=1
```

Expected: tests do not compile because the current lease is scalar and
writeback sources are restricted to one page.

- [ ] **Step 3: Implement the batch owner/view and compatibility adapters**

Move scalar `PageLease` liveness into `LeasedPage`. Keep current
`IoDataSource::PageCache`/`IoDataTarget::PageCache` as one-segment compatibility
views. Add batch variants backed by `PageDataLeaseId` and checked slice ranges.
The registry retains the real lease bundle; copying a DTO cannot extend
lifetime. Failed admission returns the bundle to PageBacked and restores every
submitted generation.

- [ ] **Step 4: Verify lease lifetime and zero-copy representation**

```sh
cargo test -p tx-subsystems --lib page_data_lease -- --test-threads=1
cargo test -p tx-subsystems --lib fs_iface::plan -- --test-threads=1
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
```

Expected: all pass; no page payload is embedded in the lease/view DTO.

- [ ] **Step 5: Commit**

```sh
git add crates/tx-subsystems/src/page_backed crates/tx-subsystems/src/fs_iface
git commit -m "feat(page-backed): add multi-page data leases"
```

### Task 2: Aggregate one fsync frontier into one ext4 transaction

**Files:**
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/page_backed/fsync_submission.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-ext4/src/planner.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`
- Test: `crates/tx-ext4/tests/journal_prepared_transaction.rs`

- [ ] **Step 1: Add failing multi-page frontier tests**

Test 2, 17 and 256 dirty pages; discontiguous pages; redirty during I/O; one
page planning failure; queue backpressure; and two concurrent fsync callers on
the same object. Assert that one captured frontier creates one transaction ID,
one commit durability point and a vector of submitted generations.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-subsystems --lib multi_page_fsync -- --test-threads=1
cargo test -p tx-ext4 --test journal_prepared_transaction multi_page -- --test-threads=1
```

Expected: ext4 rejects `page_count != 1` or the second scalar transaction sees
`Busy`.

- [ ] **Step 3: Add range/batch admission**

Introduce one request value:

```rust
pub struct WritebackBatchRequest {
    pub object: FsObjectKey,
    pub frontier: Box<[(PageIndex, PageGeneration)]>,
    pub lease: PageDataView,
    pub durability: WritebackDurability,
}
```

PageBacked captures/freeze-admits the entire bounded batch atomically. Ext4
plans all data runs and coalesces metadata after-images by home block before one
journal reservation. A configurable batch ceiling bounds work; a larger fsync
frontier is processed as a sequence of transactions whose final captured
frontier is not reported complete until every transaction commits. Concurrent
fsync callers may join a covering transaction frontier; they do not start a
second active mutation.

- [ ] **Step 4: Apply vector completion**

Terminal settlement carries the submitted generation vector. PageBacked applies
completion per page: matching clean pages become resident, redirtied pages stay
dirty, stale entries are ignored for semantic mutation but all lease resources
are settled. One page failure makes the batch result fail without cleaning any
unconfirmed generation.

- [ ] **Step 5: Verify progress and failure rollback**

```sh
cargo test -p tx-subsystems --lib multi_page_fsync -- --test-threads=1
cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1
cargo test -p tx-ext4 --lib -- --test-threads=1
```

Expected: all pass; no normal multi-page fsync returns `EBUSY` solely because a
prior page from the same frontier owns the active transaction.

- [ ] **Step 6: Commit**

```sh
git add crates/tx-subsystems/src/page_backed crates/tx-ext4/src crates/tx-ext4/tests
git commit -m "feat(ext4): aggregate fsync writeback transactions"
```

### Task 3: Route namespace mutation through one atomic JBD2 transaction

**Files:**
- Modify: `crates/tx-ext4-format/src/mutation.rs`
- Modify: `crates/tx-ext4-format/src/pager.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Test: `crates/tx-ext4-format/tests/pager_mock.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`
- Create: `crates/tx-ext4/tests/namespace_transaction.rs`

- [ ] **Step 1: Add failing namespace transaction tests**

For create, mkdir, unlink, rmdir, link and rename, assert that no home write
occurs before admission and that all affected inode, directory, bitmap, group
descriptor, superblock and orphan updates are represented in one mutation plan.
For rename test same-directory, cross-directory, replace, directory-parent link
counts, and rollback when a destination step fails.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-ext4 --test namespace_transaction -- --test-threads=1
cargo test -p tx-ext4-format --test pager_mock namespace -- --test-threads=1
```

Expected: current namespace methods perform direct pager home mutations and
rename exposes intermediate states.

- [ ] **Step 3: Add pure namespace mutation plans**

Extend `MutationOrigin` with typed namespace operations and return a single
`Ext4MutationPlan` containing expected versions, allocations/frees, revokes and
all metadata after-images. Planning reads a consistent immutable input set; it
does not call `BlockImage::write_block`. Mutation publication follows:

```text
observe versions -> reserve blocks/inodes/journal -> freeze after-images
-> admit graph -> durable commit -> publish VFS-visible cache facts
-> asynchronous checkpoint
```

If preconditions changed, return retry before disk or VFS publication. For
unlinked-but-open files, record the orphan transition rather than freeing live
payload immediately.

- [ ] **Step 4: Replace namespace direct calls with a StepOp admission path**

`FsOps` namespace methods construct an owned operation state that may yield on
metadata reads, journal space, graph completion and checkpoint backpressure. No
epoch guard or pager lock crosses yield. Update lookup/dir/inode caches only
after admission/commit according to the operation's Linux-visible point.

- [ ] **Step 5: Verify namespace atomicity**

```sh
cargo test -p tx-ext4 --test namespace_transaction -- --test-threads=1
cargo test -p tx-ext4-format --test pager_mock -- --test-threads=1
cargo test -p tx-ext4 --lib -- --test-threads=1
```

Expected: all pass; production namespace methods have no direct multi-step home
mutation sequence.

- [ ] **Step 6: Commit**

```sh
git add crates/tx-ext4-format/src crates/tx-ext4-format/tests crates/tx-ext4/src crates/tx-ext4/tests
git commit -m "feat(ext4): journal atomic namespace mutations"
```

### Task 4: Create kernel-neutral pager APIs and extract `tx-ext4-pager`

**Files:**
- Create: `crates/tx-pager-api/Cargo.toml`
- Create: `crates/tx-pager-api/src/lib.rs`
- Create: `crates/tx-pager-api/src/range.rs`
- Create: `crates/tx-pager-api/src/layout.rs`
- Create: `crates/tx-pager-api/src/transaction.rs`
- Create: `crates/tx-ext4-pager/Cargo.toml`
- Create: `crates/tx-ext4-pager/src/lib.rs`
- Create: `crates/tx-ext4-pager/src/read.rs`
- Create: `crates/tx-ext4-pager/src/write.rs`
- Create: `crates/tx-ext4-pager/src/namespace.rs`
- Create: `crates/tx-ext4-pager/src/allocation.rs`
- Modify: `Cargo.toml`
- Modify: `crates/tx-ext4/Cargo.toml`
- Modify: `crates/tx-ext4/src/planner.rs`

- [ ] **Step 1: Add failing crate-boundary tests**

Add compile tests proving that a caller-defined `PayloadKey` round-trips
unchanged, metadata misses return a resume token, holes are explicit, and plans
contain no Tx kernel resource. Add a lint fixture that rejects imports of
`tx-subsystems`, PPN, `BioVec`, reactor or device queues from either new crate.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-pager-api
cargo test -p tx-ext4-pager
cargo xtask lint invariants memory-io-ownership
```

Expected: the new crates do not yet exist.

- [ ] **Step 3: Implement the pure interface**

Use the semantic contract:

```rust
pub trait FileLayoutPlanner {
    type PayloadKey: Copy + Eq;
    fn plan(
        &self,
        request: FileLayoutRequest<Self::PayloadKey>,
    ) -> Result<LayoutOutcome<Self::PayloadKey>, FileLayoutError>;
    fn resume(
        &self,
        token: LayoutResumeToken,
        metadata: MetadataReadResult,
    ) -> Result<LayoutOutcome<Self::PayloadKey>, FileLayoutError>;
}
```

`FileIoPlan<K>` contains logical/physical ranges, `LeaseSlice<K>`, allocation
reservations, metadata version preconditions, frozen after-image descriptions,
durability domains and dependency edges. It contains no PPN, `BioVec`, queue
tag, waiter, `Guard`, callback or submission authority. Move pure extent,
allocation and namespace planning from `tx-ext4-format`/`tx-ext4` while leaving
wire codecs in `tx-ext4-format`.

- [ ] **Step 4: Add the Tx lowering adapter**

`tx-ext4` binds opaque keys to retained `PageDataView`s and mount-private
metadata leases, validates plan preconditions, then lowers ranges/edges into the
existing `BackendBioGraph`. Preserve `BackendPlanner` as a migration facade; it
must not become the reusable pager API.

- [ ] **Step 5: Verify dependency direction and parity**

```sh
cargo test -p tx-pager-api
cargo test -p tx-ext4-pager
cargo test -p tx-ext4-format
cargo test -p tx-ext4 --lib -- --test-threads=1
rg -n 'tx_subsystems|PageContainer|PageDataLease|Ppn|BioVec|tx_reactor' crates/tx-pager-api crates/tx-ext4-pager
```

Expected: tests pass and the scan has no production import/reference.

- [ ] **Step 6: Commit**

```sh
git add Cargo.toml Cargo.lock crates/tx-pager-api crates/tx-ext4-pager crates/tx-ext4-format crates/tx-ext4
git commit -m "refactor(ext4): extract reusable pure pager planning"
```

### Task 5: Replace ordinary payload sealing with opaque lease slices

**Files:**
- Modify: `crates/tx-ext4-format/src/mutation.rs`
- Modify: `crates/tx-ext4-pager/src/write.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Test: `crates/tx-ext4/tests/journal_prepared_transaction.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`

- [ ] **Step 1: Add a failing no-payload-in-plan test**

Build a write plan for nonzero page bytes and assert that the pure plan size and
contents do not contain the 4096-byte payload; lowering must resolve the exact
lease slice to the final BIO. Add counters/assertions for zero plan payload copy.

- [ ] **Step 2: Run the RED test**

```sh
cargo test -p tx-ext4 no_payload_bytes_in_file_io_plan -- --test-threads=1
```

Expected: the legacy `SealedDataWrite { bytes: Page4K }` shape fails the test.

- [ ] **Step 3: Replace ordinary data values**

Replace ordinary data writes with `LeaseSlice<K> { key, offset, len }`. Keep
metadata after-images and journal records in their separate metadata accounting
domain. Delete the zero-filled placeholder in `read_backend.rs`. Validate that
all slices are within the retained lease and exactly cover the corresponding LBA
run before graph admission.

- [ ] **Step 4: Verify the normal payload path**

```sh
cargo test -p tx-ext4 --lib -- --test-threads=1
cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1
rg -n 'SealedDataWrite|bytes: Page4K' crates/tx-ext4-format crates/tx-ext4-pager crates/tx-ext4
```

Expected: all tests pass; any remaining `Page4K` bytes are format metadata,
journal records or test fixtures, not ordinary file payload.

- [ ] **Step 5: Commit**

```sh
git add crates/tx-ext4-format crates/tx-ext4-pager crates/tx-ext4
git commit -m "fix(ext4): plan ordinary data through retained lease slices"
```

### Task 6: Add `FrozenMetadataLease` and durability-domain graph values

**Files:**
- Create: `crates/tx-ext4/src/frozen_metadata.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs`
- Modify: `crates/tx-subsystems/src/io_manager/backend/graph.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/service.rs`
- Test: `crates/tx-ext4/tests/journal_prepared_transaction.rs`
- Test: `crates/tx-subsystems/src/io_manager/backend/graph.rs`

- [ ] **Step 1: Add failing lease-lifecycle and illegal-merge tests**

Test `Prepared -> Frozen -> JournalSubmitted -> CommitDurable ->
CheckpointSubmitted -> CheckpointComplete -> Released`, pre-commit abort, COW
generation separation, checkpoint retry, and reuse of one frozen after-image for
journal/checkpoint. Graph tests reject merging across dependency, barrier,
transaction, device, operation or completion-domain boundaries.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-ext4 --test journal_prepared_transaction frozen_metadata -- --test-threads=1
cargo test -p tx-subsystems --lib io_manager::backend::graph -- --test-threads=1
```

- [ ] **Step 3: Implement frozen metadata ownership**

The lease binds mount/transaction generation, journal reservation, home block,
role, before-version, immutable after-image storage and settlement phase. A later
mutation of the same home block receives a new generation/COW page. Journal and
checkpoint BIOs refer to one retained metadata lease when no encoding transform
is needed; descriptor/revoke/escaped/checksummed/commit pages remain independent
`JournalRecordLease`s.

- [ ] **Step 4: Extend the existing graph in place**

Add `IoBufferRef`, `BarrierDomain`, inherited priority and typed completion
cookie to existing graph nodes. Preserve old constructors as adapters during the
migration. L4 owns the terminal bundle; L6 completion cannot release leases or
change PageSlot state.

- [ ] **Step 5: Verify lifecycle and execution**

```sh
cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1
cargo test -p tx-ext4 --test journal_transaction_plan -- --test-threads=1
cargo test -p tx-subsystems --lib io_manager -- --test-threads=1
```

- [ ] **Step 6: Commit**

```sh
git add crates/tx-ext4/src crates/tx-ext4/tests crates/tx-subsystems/src/fs_iface crates/tx-subsystems/src/io_manager
git commit -m "feat(ext4): retain frozen metadata through checkpoint"
```

### Task 7: Cut over production and retire compatibility mutation paths

**Files:**
- Modify: `crates/tx-ext4/src/mount.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4/src/pager.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-fs/src/tx_ext4_bridge.rs`
- Modify: `xtask/src/lint_invariants_memory_io.rs`
- Modify: `docs/progress/plans/2026-07-13-ext4-io-manager-write-path.json`
- Modify: `docs/progress/plans/2026-07-23-rsext4-full-migration.json`

- [ ] **Step 1: Add production-reachability ratchets**

Fail if a read-write mount routes file read/write/truncate/fsync or supported
namespace mutation through direct `BlockImage::write_block`, legacy fixed-block
journal helpers, or a non-admitted mutation path. Test-only compatibility paths
must be explicitly `cfg(test)`.

- [ ] **Step 2: Run the RED lint**

```sh
cargo xtask lint invariants memory-io-ownership
```

Expected: current compatibility callsites are reported.

- [ ] **Step 3: Cut over one operation family at a time**

Order: mapped read, hole read, mapped write, allocation write, truncate/setattr,
fsync/fdatasync, create/mkdir, link/unlink/rmdir, rename, checkpoint/clean
unmount. After each family, run its host parity tests before deleting the old
production call. Keep read-only format inspection helpers if they do not submit
I/O or mutate home blocks.

- [ ] **Step 4: Close inherited open work**

Update the 2026-07-13 plan to mark the graph/runtime work consumed and the
superseded mutation/cutover steps as replaced by this plan. Update the rsext4
plan only for phases completed by these commits; leave unimplemented Tier-2
features pending.

- [ ] **Step 5: Run the data-plane gate**

```sh
cargo test -p tx-pager-api
cargo test -p tx-ext4-pager
cargo test -p tx-ext4-format
cargo test -p tx-ext4 --lib -- --test-threads=1
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
cargo test -p tx-subsystems --lib io_manager -- --test-threads=1
cargo xtask lint invariants memory-io-ownership
cargo -q xtask unit
cargo xtask progress validate
git diff --check
```

- [ ] **Step 6: Commit**

```sh
git add crates/tx-ext4 crates/tx-fs crates/tx-subsystems xtask/src docs/progress
git commit -m "refactor(ext4): cut production I/O over to transactional pager plans"
```
