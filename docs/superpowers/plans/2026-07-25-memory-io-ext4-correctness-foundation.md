# Memory/File-I/O Correctness Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove the ownership, reclaim and pre-commit journal correctness
blockers that make later ext4 transaction and memory-pressure work unsafe.

**Architecture:** Make `PageSlot` the single file-page state authority, perform
generation-checked reclaim through an owner token, canonicalize ext4 file
`PageContainer`s by mount/object identity, make file-I/O runtimes weak and
retirable, and settle journal transactions according to their durability
phase.

**Tech Stack:** Rust `no_std`, PageBacked, Zone `Cap`/`Weak`, EBR guards,
`StepOutcome`, ext4 JBD2 runtime, and xtask invariant lints.

---

## File structure

- Modify `crates/tx-subsystems/src/page_backed/slot.rs`: add the owner-only
  reclaim claim/finish/abort state transitions.
- Modify `crates/tx-subsystems/src/page_backed/mod.rs`: remove semantic
  dirty/writeback marks, route withdrawal through `PageSlot`, and expose
  bounded owner reclaim.
- Modify `crates/tx-subsystems/src/page_backed/slot_tests.rs` and
  `core_tests.rs`: state, stale-claim, refetch and actual-free tests.
- Modify `crates/tx-ext4/src/read_backend.rs` and `namespace.rs`: mount-scoped
  canonical file-PC registry.
- Modify `crates/tx-ext4/src/mount.rs` and `tests_v3.rs`: lifecycle and duplicate
  materialization tests.
- Modify `crates/tx-subsystems/src/device.rs`: weak file-I/O runtime ownership,
  idempotent registration and retirement.
- Modify `crates/tx-ext4/src/journal.rs` and journal integration tests:
  durability-phase-aware abort/quarantine and exactly-once ring settlement.
- Create `xtask/src/lint_invariants_memory_io.rs`; modify `xtask/src/lib.rs` and
  `xtask/src/lint.rs`: ratchet duplicate dirty authority and direct ext4 home
  mutations after each owner migration.

### Task 1: Make `PageSlot` the only dirty/writeback authority

**Files:**
- Modify: `crates/tx-subsystems/src/page_backed/slot.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Test: `crates/tx-subsystems/src/page_backed/slot_tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Add failing authority tests**

Add tests proving that dirty, writeback and redirty classification comes only
from `PageSlotSnapshot`, including stale completion rejection. The core test
must construct a resident entry, dirty it, begin writeback, redirty it, complete
the old generation, and assert that replacement metadata contains only
`referenced` and `no_reclaim`.

```rust
#[test]
fn replacement_marks_do_not_own_dirty_or_writeback() {
    let slot = resident_slot(Ppn::new(7));
    let dirty = slot.mark_dirty().unwrap();
    let submitted = slot.begin_writeback().unwrap();
    slot.mark_dirty().unwrap();
    let done = slot.complete_writeback(submitted.generation, Ok(())).unwrap();
    assert!(matches!(done.state, PageSlotState::Dirty { .. }));
    assert!(dirty.generation < done.generation);
}
```

- [ ] **Step 2: Run the RED tests**

Run:

```sh
cargo test -p tx-subsystems --lib page_backed::slot_tests -- --test-threads=1
cargo test -p tx-subsystems --lib replacement_marks_do_not_own_dirty_or_writeback -- --test-threads=1
```

Expected: the replacement-mark test fails while `PageMarks` still contains
`dirty`/`writeback`, or the new API does not compile.

- [ ] **Step 3: Remove the duplicate authority**

Reduce the replacement value to this semantic shape:

```rust
pub struct PageMarks {
    pub referenced: bool,
    pub no_reclaim: bool,
}
```

Replace all `PageCacheMark::{Dirty, Writeback}` reads and writes with
`PageSlot::snapshot`, `begin_writeback`, `complete_writeback`, and
`abort_writeback`. Dirty-page enumeration must iterate slots and then resolve
the matching resident binding; it must not infer state from the sparse index.
No compatibility boolean may remain in `PageCacheEntry` or `FrameMeta`.

- [ ] **Step 4: Verify the authority transition**

Run:

```sh
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
rg -n 'PageCacheMark::(Dirty|Writeback)|marks\.(dirty|writeback)' crates/tx-subsystems/src/page_backed
```

Expected: PageBacked tests pass and the scan has no production match.

- [ ] **Step 5: Commit**

```sh
git add crates/tx-subsystems/src/page_backed
git commit -m "fix(page-backed): make PageSlot the file-page state authority"
```

### Task 2: Add generation-safe clean reclaim and refetch

**Files:**
- Modify: `crates/tx-subsystems/src/page_backed/slot.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Test: `crates/tx-subsystems/src/page_backed/slot_tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Add failing reclaim/refetch and race tests**

Cover: clean resident withdrawal, refetch after withdrawal, dirty refusal,
pinned refusal, stale candidate refusal, access racing with withdrawal, and
actual allocator free-count accounting. A successful claim must be represented
by an owner token:

```rust
pub struct PageReclaimClaim {
    page: PageIndex,
    ppn: Ppn,
    generation: PageGeneration,
}
```

The central regression is:

```rust
#[test]
fn clean_reclaim_transitions_slot_to_empty_then_refetches_once() {
    let pc = file_pc_with_resident_page(0);
    let generation = pc.page_slot_snapshot(PageIndex::new(0)).generation;
    let claim = pc.try_claim_clean_page(PageIndex::new(0), generation).unwrap();
    assert_eq!(pc.finish_clean_reclaim(claim).unwrap().state, PageSlotState::Empty);
    assert!(matches!(pc.begin_file_page_fetch(PageIndex::new(0)), FilePageFetchStart::Owner(_)));
}
```

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-subsystems --lib clean_reclaim -- --test-threads=1
```

Expected: the refetch test fails because the current sweep removes only the
resident-index entry and leaves the slot `Resident`.

- [ ] **Step 3: Implement the owner protocol**

Add an internal `PageSlotState::Reclaiming { ppn }` state or an equivalent
generation-checked reservation. Implement:

```rust
impl PageSlot {
    fn try_begin_reclaim(
        &self,
        expected: PageGeneration,
        expected_ppn: Ppn,
    ) -> Result<PageReclaimClaim, PageReclaimError>;

    fn finish_reclaim(
        &self,
        claim: &PageReclaimClaim,
    ) -> Result<PageSlotSnapshot, PageReclaimError>;

    fn abort_reclaim(
        &self,
        claim: &PageReclaimClaim,
    ) -> Result<PageSlotSnapshot, PageReclaimError>;
}
```

Under the PageContainer owner lock: validate clean slot/generation, validate
`no_reclaim` and liveness facts, enter reclaiming, remove the expected resident
binding, then finish to `Empty`. If index removal fails, abort to `Resident`. A
hit observing `Reclaiming` retries or waits; it cannot install a mapping from
the withdrawing binding. Report bindings withdrawn and allocator frames freed
as different values.

- [ ] **Step 4: Verify reclaim state and lifetime**

```sh
cargo test -p tx-subsystems --lib clean_reclaim -- --test-threads=1
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
```

Expected: all tests pass; the stale-claim test leaves the newer resident page
untouched; actual-free may be zero while another map or DMA pin remains.

- [ ] **Step 5: Commit**

```sh
git add crates/tx-subsystems/src/page_backed
git commit -m "fix(page-backed): reclaim clean pages through PageSlot claims"
```

### Task 3: Canonicalize ext4 file-PC identity and retire runtimes

**Files:**
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4/src/mount.rs`
- Modify: `crates/tx-ext4/src/tests_v3.rs`
- Modify: `crates/tx-subsystems/src/device.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Add failing identity and lifecycle tests**

Test that two materializations of one `(mount, inode)` return the same PC
identity, different mounts do not alias, registration is idempotent, a closed
and unreferenced file PC can die after epoch drain, and an unmounted runtime no
longer receives kicks. Expose only test counters, not production registry
internals.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-ext4 --lib canonical_file_page_container -- --test-threads=1
cargo test -p tx-subsystems --lib file_io_runtime_retires -- --test-threads=1
```

Expected: duplicate materialization produces distinct PCs or the runtime count
does not return to baseline.

- [ ] **Step 3: Add mount-private weak identity and weak runtime custody**

Add an ext4 mount registry keyed by `FsObjectId`:

```rust
struct FilePageRegistry {
    rows: BTreeMap<FsObjectId, Weak<PageContainer>>,
}

impl FilePageRegistry {
    fn get_or_create(
        &mut self,
        object: FsObjectId,
        guard: &Guard<'_>,
        create: impl FnOnce() -> Result<Cap<PageContainer>, Errno>,
    ) -> Result<(Cap<PageContainer>, bool), Errno>;
}
```

Construct through the canonical PageBacked file-PC helper so the object is
visible to later provider registration. Change the runtime registry to store a
`Weak<PageContainer>` plus a stable registration ID. Each service turn upgrades
once, drops the EBR guard before any wait, and exits/retire-cleans the row if the
upgrade fails. Registering the same PC/device pair returns the existing handle.
Unmount removes mount-owned identity rows and kicks runtime cleanup.

- [ ] **Step 4: Verify identity and no retention cycle**

```sh
cargo test -p tx-ext4 --lib -- --test-threads=1
cargo test -p tx-subsystems --lib file_io_runtime -- --test-threads=1
```

Expected: the same mount/object shares one PC; after dropping RNode/open-file
owners and draining EBR, weak upgrade fails and runtime count returns to the
starting value.

- [ ] **Step 5: Commit**

```sh
git add crates/tx-ext4/src crates/tx-subsystems/src/device.rs
git commit -m "fix(ext4): canonicalize file page containers and runtime lifetime"
```

### Task 4: Settle journal failures by durability phase

**Files:**
- Modify: `crates/tx-ext4/src/journal.rs`
- Test: `crates/tx-ext4/tests/journal_transaction_state.rs`
- Test: `crates/tx-ext4/tests/journal_prepared_transaction.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`

- [ ] **Step 1: Add failing failure-matrix tests**

Cover these exact outcomes:

| Failure point | Required state | Ring action | Mount action |
|---|---|---|---|
| before graph admission | aborted | release without advance | writable |
| ordered-data graph error | aborted | release without advance | writable |
| commit graph build error before submission | aborted | release without advance | writable |
| commit I/O error after submission | commit-unknown | retain/quarantine | read-only, recovery required |
| checkpoint error after durable commit | committed, checkpoint-pending | retain for retry | writable only if policy allows retry |
| checkpoint success | released | advance cursor/sequence | writable |

The data-error test must successfully reserve the next transaction. The
commit-unknown test must reject reuse and report the recovery-required state.

- [ ] **Step 2: Run the RED tests**

```sh
cargo test -p tx-ext4 --test journal_transaction_state -- --test-threads=1
cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1
```

Expected: the data/commit failure paths leave the current reservation in the
single active slot without the required phase distinction.

- [ ] **Step 3: Implement exactly-once settlement**

Represent terminal disposition explicitly:

```rust
enum JournalFailureDisposition {
    AbortPreCommit,
    CommitUnknown,
    RetryCheckpoint,
}

struct JournalSettlement {
    transaction: Option<PreparedJournalTransaction>,
    reservation: Option<(Arc<JournalRing>, JournalRingReservation)>,
}
```

Add one state-owned method that atomically takes the transaction and reservation
for pre-commit abort, drops the state lock, then calls
`ring.complete(&reservation, false)` exactly once. A commit-submitted error must
not release or advance the ring; publish a mount recovery-required/read-only
fact and retain the record range. A checkpoint error keeps the committed
transaction and reservation available for retry. Duplicate completions are
idempotent and cannot double-drop leases.

- [ ] **Step 4: Verify the full failure matrix**

```sh
cargo test -p tx-ext4 --test journal_transaction_state -- --test-threads=1
cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1
cargo test -p tx-ext4 --lib journal -- --test-threads=1
```

Expected: all cases pass; the next reservation succeeds only for safely aborted
pre-commit failures or a successfully checkpointed transaction.

- [ ] **Step 5: Commit**

```sh
git add crates/tx-ext4/src/journal.rs crates/tx-ext4/tests crates/tx-ext4/src/tests_v3.rs
git commit -m "fix(ext4): settle journal reservations by durability phase"
```

### Task 5: Add active memory/I-O ratchets and close the foundation

**Files:**
- Create: `xtask/src/lint_invariants_memory_io.rs`
- Modify: `xtask/src/lib.rs`
- Modify: `xtask/src/lint.rs`
- Modify: `docs/design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md`
- Modify: `docs/progress/plans/2026-07-25-memory-io-ext4-repair.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Add failing lint tests**

The `memory-io-ownership` rule scans production Rust for:

- semantic `dirty` or `writeback` fields in `PageMarks`/`FrameMeta`;
- ext4 regular-file construction that bypasses the canonical helper;
- a strong `Cap<PageContainer>` in the global file-I/O runtime registry; and
- direct namespace home writes after the transactional namespace cutover marker
  is enabled by Plan B.

Use temporary fixture trees for one failing and one clean case.

- [ ] **Step 2: Run the RED lint test**

```sh
cargo test -p xtask lint_invariants_memory_io -- --nocapture
```

Expected: compilation or fixture assertions fail before the rule is registered.

- [ ] **Step 3: Implement and register the rule**

Add `memory-io-ownership` to `cargo xtask lint invariants`, its help text, and
the `all` rule list. Scope exemptions to tests and a grep-stable, reason-bearing
allow comment; do not add a repository-wide blanket exception.

- [ ] **Step 4: Run the foundation gate**

```sh
cargo test -p xtask lint_invariants_memory_io -- --nocapture
cargo xtask lint invariants memory-io-ownership
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
cargo test -p tx-ext4 --lib -- --test-threads=1
cargo -q xtask unit
cargo xtask progress validate
git diff --check
```

Expected: all pass. Update the progress record so Tasks B0-B4 are complete and
name Plan B as the next executable plan.

- [ ] **Step 5: Commit**

```sh
git add xtask/src docs/design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md docs/progress
git commit -m "test(memory-io): ratchet ownership and reclaim invariants"
```
