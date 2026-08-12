# ext4 Tier 1 Lifecycle Convergence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the bounded ext4 Tier 1 surface through three lifecycle
primitives so every request, transaction, and mount frontier has exactly one
owner and exactly one terminal settlement path.

**Architecture:** PageBacked owns the `OwnedFileIoRequest` lifecycle record and
its terminal route; after graph admission L4 owns the transferred payload
bundle until a terminal completion returns that bundle to PageBacked. The
mounted ext4 runtime owns `MutationHandle`; Mount owns `MountSettlementOp` and
the shared runtime cell it serializes. The existing `BackendBioGraph` remains
the only I/O DAG. Tier 1 serializes one mutation per mount through ordered data,
commit, checkpoint, tail reclamation, cache settlement, and publication;
already-mapped buffered writes may return after dirty publication, while
extending writes, namespace, setattr, durability, and detach operations settle
before success.

**Tech Stack:** Rust `no_std`, Tx v3 `StepOp`, PageBacked/PageSlot,
`tx-ext4-format`, `tx-ext4`, JBD2 ordered mode, I/O-manager L4/L6,
virtio-blk, QEMU RV64, e2fsprogs, xfstests, and Rust `xtask` host tooling.

---

## Outcome Boundary

This plan is the only active ext4 Tier 1 delivery ledger. It replaces the
remaining delivery semantics of:

- `docs/progress/plans/2026-07-13-ext4-io-manager-write-path.json`;
- `docs/progress/plans/2026-07-23-rsext4-full-migration.json`; and
- `docs/progress/plans/2026-07-25-memory-io-ext4-repair.json`.

Their completed rows remain historical evidence. Their uncompleted rows are
canceled, not treated as implementation facts.

In scope: the pinned 4 KiB, ordered-JBD2, `metadata_csum` Tier 1 profile;
read/lookup; buffered write and coherent writable mmap; create, mkdir, link,
symlink, unlink, rmdir, rename; truncate, chmod, chown, timestamps; classic
orphan handling; regular and directory fsync, fdatasync, syncfs, sync, normal
umount, and lazy-detach lifetime.

Out of scope until G0-G7 pass: Tier 2 features, `orphan_file`, arbitrary-depth
extent/htree growth, xattr/ACL/quota/fallocate/direct-I/O/DAX/fast-commit,
multiple concurrently active transactions, background checkpoint overlap,
memory-pressure tuning, and native RV64 rustc performance promotion.

## Canonical Gate

No implementation worker may introduce a new public filesystem type, variant,
trait method, field, or module path without matching one row below. A proposed
item absent from this table requires an active design update before code.

| Proposed item | Owner and visibility | Authority | Current anchor |
|---|---|---|---|
| `OwnedFileIoRequest`, `SubmitFailure`, `FileIoTerminalResult` | PageBacked-private lifecycle record; L4 owns the admitted payload bundle and returns it through the terminal route | `EXT4-LIFECYCLE-OWNED-FILE-IO-REQUEST-1`; `IO-MANAGER-L4-PAGE-SUBMISSION-1` | `page_backed/mod.rs` request tables and manual cleanup at submission, resume, and completion |
| `PageDataLease`, `PageDataLeaseProjection` | PageBacked-issued move-only capability; only the filesystem-neutral projection crosses into the adapter | `MEMORY-IO-PAGE-DATA-LEASE-1`; `IO-MANAGER-L4-PAGE-SUBMISSION-1` | scalar `PageLease` and `file_io_leases` |
| `MutationHandle`, `MutationPhase`, `GraphCustodyToken` | mounted ext4 runtime; phase enum and graph custody are private | `EXT4-LIFECYCLE-MUTATION-HANDLE-1`; `EXT4-LIFECYCLE-POSTCOMMIT-FAILURE-1` | `JournalFsyncSourceState`, `JournalTransactionState`, `JournalMutationRuntime` |
| `FrozenMetadataToken`, `JournalExtentToken`, `AllocatorClaimToken` | ext4-private owned cross-yield tokens | `EXT4-LIFECYCLE-OWNERSHIP-1`; `MEMORY-IO-FROZEN-METADATA-LEASE-1`; `YIELD-8` | `PreparedJournalTransaction`, `JournalRingReservation`, `BlockClaim` |
| `RevokeRecord`, `DeferredFreeClaim` | immutable format-plan values; tokens become ext4-owned at admission | `EXT4-LIFECYCLE-DURABILITY-SEQUENCE-1`; Tier 1 revoke/orphan contract | `Ext4MutationPlan`, JBD2 revoke codec |
| `MountRuntimeState`, `MountRuntimeCell`, `MountSettlementOp`, `SettlementScope` | Mount-owned shared state cell and lifecycle operation; the cell is the admission/quiesce authority | `EXT4-LIFECYCLE-MOUNT-SETTLEMENT-1`; `MOUNT-STEP-UMOUNT-NORMAL-BUSY-CHECK-1` | `MountPayload`, `MountNamespace::umount`, syscall fsync/sync/umount paths |
| `ErrorSeq`, `ErrorCursor` | filesystem-neutral payload values; page, file, and mount cursors are stored by their respective owners | `EXT4-LIFECYCLE-MOUNT-SETTLEMENT-1` | no current errseq; current writeback errors live only in PageSlot terminal state |
| `BlockDurabilityCapabilities`, `BlockWriteOptions` | device-owned capability/value types | `EXT4-LIFECYCLE-DURABILITY-SEQUENCE-1`; `IO-MANAGER-L6-BLOCK-SUBMISSION-1` | `BlockFlags`, `BlockDeviceOps`, `BlockDeviceDispatchAdapter` |
| `FsOps::shutdown` | defaulted backend hook used only by detach settlement | `MOUNT_v1` section 5.3 payload reclamation; `EXT4-LIFECYCLE-MOUNT-SETTLEMENT-1` | `FsOps`; current unmount has no backend settlement hook |
| `MutationWaiter`, `MutationWaitQueue` | mounted ext4 runtime-private owned wait records; normal callers queue and wake, try-admission may return `EBUSY` internally | `EXT4-LIFECYCLE-MUTATION-HANDLE-1`; `WAIT_PROTOCOL` | no current owner queue; current admission is a scalar busy check |
| `CapabilityProfileHash` | generated profile binding stored in the mount admission record and acceptance receipt | `EXT4-LIFECYCLE-TIER1-CAPABILITY-1`; `EXT4-LIFECYCLE-ACCEPTANCE-1` | no current parity/hash binding |
| `RunWorkspace` | host-only `xtask` owner; never a kernel primitive | `EXT4-LIFECYCLE-ACCEPTANCE-1` | absent `cargo xtask ext4`; duplicated historical host cleanup remains outside current HEAD |

The following names are explicitly forbidden: a universal cleanup registry, a
second BIO graph, public phase-typestate structs, an ext4-owned ordinary file
cache, a `PageDataLease` duplicated inside `MutationHandle`, or a borrowed
guard/reservation/witness stored across a yield.

## File Structure

- Modify `crates/tx-subsystems/src/page_backed/lifecycle.rs`: own
  `PageDataLease`, its neutral projection, the PageBacked owner token,
  `OwnedFileIoRequest`, terminal settlement, and PageSlot cleanup helpers.
- Modify `crates/tx-subsystems/src/page_backed/slot.rs`: make raw terminal
  transitions private to the PageBacked lifecycle facade; no production caller
  outside that facade may invoke them.
- Modify `crates/tx-subsystems/src/page_backed/mod.rs`: keep the PageContainer
  facade and replace parallel lease/target maps with one owner-token table;
  L4 retains the admitted payload bundle until terminal return.
- Modify `crates/tx-subsystems/src/fs_iface/plan.rs`: define the
  filesystem-neutral `PageDataLeaseProjection` with no PageSlot cleanup rights.
- Create `crates/tx-subsystems/src/page_backed/error_seq.rs`: small packed
  error sequence and observer cursor shared by file and mount reporting.
- Create `crates/tx-subsystems/src/mount/settlement.rs`: `MountRuntimeState`,
  `MountRuntimeCell`, `SettlementScope`, `MountSettlementOp`, visible-mount
  snapshot, normal/lazy detach sequencing, and the mount settlement driver
  queue.
- Modify `crates/tx-subsystems/src/page_backed/fs_page_backing.rs` and
  `crates/tx-subsystems/src/vfs/execution.rs`: route file/mount settlement and
  add the default backend shutdown hook without adding an ext4 dependency.
- Keep `crates/tx-ext4/src/journal.rs` focused on JBD2 record leases, graph
  construction, and ring mechanics.
- Create `crates/tx-ext4/src/mutation_lifecycle.rs`: move mount-local mutation
  admission and all post-admission phase ownership out of `journal.rs`.
- Create `crates/tx-ext4/src/settlement.rs`: ext4 backend implementation used
  by file, mount, and detach scopes; it drives the active `MutationHandle`.
- Modify `crates/tx-ext4-format/src/mutation.rs`, `ondisk.rs`, `pager.rs`, and
  `journal_replay.rs`: pure complete after-images, checksums, revoke/deferred
  free, recovery-state admission, and cache refresh inputs.
- Modify `crates/tx-subsystems/src/device.rs` and the RV64 virtio block bridge:
  consume FUA/flush capability and flags at the real device boundary.
- Create `xtask/src/ext4/{mod.rs,run_workspace.rs,receipt.rs,tests.rs}` and
  `tools/ext4/tier1/`: one public Tier 1 runner, three pinned authority inputs,
  and one immutable receipt.

## Execution Discipline

Before Task 1, use `superpowers:using-git-worktrees` to create an isolated
`codex/ext4-tier1-lifecycle-convergence` worktree. One worker owns one task and
one listed write set. The coordinator alone edits shared traits, this plan,
and the JSON ledger. `docs/progress/STATUS.md` is an additive catch-up only and
is not an exclusive plan write set. Tasks 7 and 8 change shared
device/image traits; those trait edits are coordinator-owned checkpoints and
must not be run in parallel with another worker's trait changes.

Every Rust behavior change follows RED -> focused GREEN -> subsystem regression
-> `cargo -q xtask unit`. Every task ends with `cargo fmt --check`,
`git diff --check`, a scoped progress update, and one commit with no coauthor.

## Spec Coverage

| Active design anchor | Executable plan coverage |
|---|---|
| `EXT4-LIFECYCLE-DECISION-1` | Outcome boundary, Tasks 2, 5, 13, and 14 |
| `EXT4-LIFECYCLE-OWNERSHIP-1` | Canonical Gate, Tasks 3-5, and G0 ownership lint |
| `EXT4-LIFECYCLE-OWNED-FILE-IO-REQUEST-1` | Tasks 3-4 and stale/duplicate terminal tests |
| `EXT4-LIFECYCLE-MUTATION-HANDLE-1` | Tasks 5-6 and the serialized admission queue |
| `EXT4-LIFECYCLE-PRECOMMIT-FAILURE-1` | Task 6 abort drain and graph-custody matrix |
| `EXT4-LIFECYCLE-POSTCOMMIT-FAILURE-1` | Tasks 6 and 8 retry, `CommitUnknown`, and `RecoveryOnly` |
| `EXT4-LIFECYCLE-DURABILITY-SEQUENCE-1` | Tasks 7-9 plus D0-D12 crash catalog |
| `EXT4-LIFECYCLE-MOUNT-SETTLEMENT-1` | Task 13 shared cell, queue driver, errseq, and syscall routing |
| `EXT4-LIFECYCLE-SYSCALL-PROJECTION-1` | Task 13 fsync/fdatasync/syncfs/sync/umount matrix |
| `EXT4-LIFECYCLE-TIER1-CAPABILITY-1` | Task 2 JSON authority, generated Rust profile, and hash parity |
| `EXT4-LIFECYCLE-PRODUCTION-CONVERGENCE-1` | Task 14 single RW path and three G0 lints |
| `EXT4-LIFECYCLE-ACCEPTANCE-1` | Tasks 15-16 RunWorkspace, immutable receipt, and G0-G7 |
| `EXT4-LIFECYCLE-IMPLEMENTATION-ORDER-1` | Dependency summary and the 16 sequential tasks |
| `EXT4-LIFECYCLE-DEFERRED-OPTIMIZATIONS-1` | Outcome boundary exclusions and no performance promotion gate |

### Task 1: Reconcile HEAD and Historical Journal Slices

**Files:**
- Create: `docs/progress/research/2026-07-30-ext4-lifecycle-slice-reconciliation.md`
- Modify: `docs/progress/plans/2026-07-30-ext4-tier1-lifecycle-convergence.json`
- Inspect only: commits `1cee426a`, `9f39f58f`, `555e4a40`

- [ ] **Step 1: Capture the candidate and baseline**

```sh
git rev-parse HEAD
git status --short
cargo test -p tx-ext4-format
cargo test -p tx-ext4 --lib --no-default-features
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
```

Expected: record the exact HEAD and results. A failing baseline is a blocker
row with the failing test and error; it is not silently normalized.

- [ ] **Step 2: Inspect each historical patch without cherry-picking it**

```sh
git show --stat --oneline 1cee426a
git show --stat --oneline 9f39f58f
git show --stat --oneline 555e4a40
git show --format=fuller --no-ext-diff --no-renames 1cee426a -- crates/tx-ext4-format
git show --format=fuller --no-ext-diff --no-renames 9f39f58f -- crates/tx-ext4
git show --format=fuller --no-ext-diff --no-renames 555e4a40 -- crates/tx-ext4-format crates/tx-ext4 crates/tx-subsystems/src/device.rs crates/tx-subsystems/src/page_backed
```

Expected: no worktree mutation. The research note contains this exact table:

```markdown
| commit | verified invariant | tests worth importing | code disposition | target task |
|---|---|---|---|---|
| 1cee426a | metadata_csum after-image preservation | host_tools and pager_mock checksum cases | reapply only checksum helpers matching current ondisk APIs | Task 10/12 |
| 9f39f58f | namespace publication waits for checkpoint | publication-order tests | express through MutationHandle terminal publication, not its old callback shape | Task 5/12 |
| 555e4a40 | recovery gate, cache refresh, and request cleanup witnesses | recovery, device, PageBacked error-path tests | split by owner; never import the 17-file patch wholesale | Task 3/4/7/8 |
```

- [ ] **Step 3: Update the ledger and commit the reconciliation note**

```sh
cargo xtask progress validate
git add docs/progress/research/2026-07-30-ext4-lifecycle-slice-reconciliation.md docs/progress/plans/2026-07-30-ext4-tier1-lifecycle-convergence.json
git diff --cached --check
git commit -m "docs(ext4): reconcile lifecycle implementation slices"
```

### Task 2: Install the Tier 1 Admission Gate and Fail Closed

**Files:**
- Create: `crates/tx-ext4-format/src/capability.rs`
- Create: `tools/ext4/tier1/capability-ledger.json`
- Modify: `crates/tx-ext4-format/src/lib.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4/src/mount.rs`
- Test: `crates/tx-ext4-format/tests/host_tools.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`

- [ ] **Step 1: Write RED tests for profile admission and zero-write rejection**

```rust
#[test]
fn tier1_rejects_unsupported_shape_before_mutation() {
    let profile = Tier1Capabilities::generated();
    assert_eq!(profile.admit(Tier1Request::HtreeSplit), Err(Tier1Reject::Unsupported));
}

#[test]
fn generated_capability_profile_matches_authority_ledger() {
    assert_eq!(Tier1Capabilities::generated().profile_hash(),
        capability_ledger_sha256_for_test());
}

#[test]
fn production_namespace_without_mutation_owner_writes_nothing() {
    let (fs, writes) = writable_ext4_without_mutation_runtime();
    assert_errno(fs.create_for_test(2, b"x"), Errno::EOPNOTSUPP);
    assert_eq!(writes.load(Ordering::Acquire), 0);
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4-format --test host_tools tier1_ -- --test-threads=1
cargo test -p tx-ext4 --lib production_namespace_without_mutation_owner -- --test-threads=1
```

Expected: unresolved `Tier1Capabilities` and a current direct home write.

- [ ] **Step 3: Add the closed admission value and guard**

```rust
pub struct Tier1Capabilities {
    profile_hash: [u8; 32],
    feature_bits: Tier1FeatureBits,
    geometry: Tier1Geometry,
}

impl Tier1Capabilities {
    pub const fn generated() -> Self {
        GENERATED_TIER1_PROFILE
    }

    pub const fn profile_hash(self) -> [u8; 32] {
        self.profile_hash
    }

    pub const fn admit(self, request: Tier1Request) -> Result<(), Tier1Reject> {
        match request {
            Tier1Request::DepthOneExtent
            | Tier1Request::LinearDirectory
            | Tier1Request::NonSplittingHtree
            | Tier1Request::ClassicOrphan => Ok(()),
            Tier1Request::ExtentDepthGrowth
            | Tier1Request::HtreeSplit
            | Tier1Request::OrphanFile
            | Tier1Request::DirectIo => Err(Tier1Reject::Unsupported),
        }
    }
}
```

`tools/ext4/tier1/capability-ledger.json` is the sole profile authority. A
small host generator emits `GENERATED_TIER1_PROFILE` and its SHA-256 into
`capability.rs`; the RED/GREEN parity test hashes the exact ledger bytes and
rejects a Rust/JSON drift before a mount is admitted. The mounted instance
stores `CapabilityProfileHash` and rejects a request whose feature bits,
geometry, or shape bounds do not match that generated profile.

Before every current pager mutation in production `FsOps`, require a mounted
mutation owner. Until the matching vertical slice lands, return
`EOPNOTSUPP` before calling `with_pager` or changing caches.

- [ ] **Step 4: Run GREEN and commit**

```sh
cargo test -p tx-ext4-format --test host_tools tier1_ -- --test-threads=1
cargo test -p tx-ext4 --lib production_namespace_without_mutation_owner -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-ext4-format/src/capability.rs crates/tx-ext4-format/src/lib.rs crates/tx-ext4/src/read_backend.rs crates/tx-ext4/src/namespace.rs crates/tx-ext4/src/mount.rs crates/tx-ext4-format/tests/host_tools.rs crates/tx-ext4/src/tests_v3.rs tools/ext4/tier1/capability-ledger.json
git commit -m "fix(ext4): fail closed outside Tier 1 mutation ownership"
```

### Task 3: Add `PageDataLease` and `OwnedFileIoRequest`

**Files:**
- Modify: `crates/tx-subsystems/src/page_backed/lifecycle.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Test: `crates/tx-subsystems/src/page_backed/lifecycle_tests.rs`

- [ ] **Step 1: Write RED exact-once ownership tests**

```rust
#[test]
fn owned_request_submit_failure_returns_the_complete_bundle() {
    let request = prepared_writeback_request(PageIndex::new(4));
    let failure = request.submit(failing_graph()).expect_err("submission fails");
    let SubmitFailure { request, error: _ } = failure;
    assert_eq!(request.lease_page_count_for_test(), 1);
    let payload = request.take_payload_for_test();
    let settled = request.finish(FileIoTerminalResult::Error {
        payload,
        error: Errno::EIO,
    });
    assert!(settled.released_payload());
    assert!(settled.restored_generation());
}

#[test]
fn stale_completion_releases_old_lease_without_cleaning_new_generation() {
    let (request, slot) = prepared_then_redirtied_writeback();
    let payload = request.take_payload_for_test();
    let settled = request.finish(FileIoTerminalResult::Success {
        payload,
        frame: None,
    });
    assert!(settled.stale());
    assert_eq!(slot.snapshot().state, PageSlotState::Dirty { redirtied: false });
    assert_eq!(live_lease_count(), 0);
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-subsystems --lib owned_request_ -- --test-threads=1
cargo test -p tx-subsystems --lib stale_completion_releases_old_lease -- --test-threads=1
```

Expected: unresolved `OwnedFileIoRequest`, `PageDataLease`, and terminal result.

- [ ] **Step 3: Implement the move-only owner**

```rust
pub(super) struct PageDataLease {
    id: IoDataLeaseId,
    pages: Box<[LeasedPage]>,
}

// crates/tx-subsystems/src/fs_iface/plan.rs
pub struct PageDataLeaseProjection {
    segments: Box<[IoDataSource]>,
}

impl PageDataLeaseProjection {
    pub fn into_sources(self) -> Box<[IoDataSource]> {
        self.segments
    }
}

// crates/tx-subsystems/src/page_backed/lifecycle.rs

pub(super) struct OwnedFileIoRequest {
    request: PageIoRequest,
    payload: Option<FileIoPayload>,
    terminal_route: FileIoTerminalRoute,
    terminal: bool,
}

pub(super) struct FileIoOwnerToken {
    request_id: PageIoRequestId,
    generation: PageGeneration,
    terminal_route: FileIoTerminalRoute,
}

pub(super) struct FileIoTerminalRoute {
    page_object: FsObjectId,
    generation: PageGeneration,
}

pub(super) enum FileIoPayload {
    Writeback { lease: PageDataLease },
    Read { target: CachedFrame },
    Control,
}

pub(super) enum FileIoTerminalResult {
    Success { payload: FileIoPayload, frame: Option<PageFrameRef> },
    Error { payload: FileIoPayload, error: Errno },
}

pub(super) struct SubmitFailure {
    pub request: OwnedFileIoRequest,
    pub error: Errno,
}
```

`finish(self, result)` is the only method allowed to call PageSlot completion
or abort, release a fetch target, drop a lease, clear writeback marks, and
record a page error. `Drop` contains only a debug assertion and owner-poison
enqueue; it performs no I/O. `submit(self, graph)` transfers the complete
payload to L4 and returns a `FileIoOwnerToken` to the PageBacked table; L4
returns that payload in `FileIoTerminalResult` when the graph is terminal.
`PageDataLeaseProjection` contains only neutral `IoDataSource` descriptors and
cannot invoke PageSlot cleanup.

Move `PageSlot`'s raw completion/abort methods behind the PageBacked lifecycle
facade (`slot.rs` is no longer a production-callable cleanup API). The RED
tests must assert both that the L4 in-flight table owns the payload after
admission and that the PageBacked owner token is the only route back.

- [ ] **Step 4: Run GREEN and regression**

```sh
cargo test -p tx-subsystems --lib owned_request_ -- --test-threads=1
cargo test -p tx-subsystems --lib page_data_lease -- --test-threads=1
cargo test -p tx-subsystems --lib page_backed::slot_tests -- --test-threads=1
cargo -q xtask unit
```

- [ ] **Step 5: Commit**

```sh
cargo fmt --check
git diff --check
git add crates/tx-subsystems/src/page_backed/lifecycle.rs crates/tx-subsystems/src/page_backed/mod.rs crates/tx-subsystems/src/page_backed/lifecycle_tests.rs
git commit -m "feat(page-backed): own file I/O request lifecycles"
```

### Task 4: Migrate PageBacked Submission, Resume, Completion, and Fsync

**Files:**
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/page_backed/lifecycle.rs`
- Modify: `crates/tx-subsystems/src/page_backed/fsync_submission.rs`
- Modify: `crates/tx-subsystems/src/vfs/execution.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/lifecycle_tests.rs`

- [ ] **Step 1: Write RED tests for every duplicated branch**

```rust
#[test]
fn backend_resume_queue_error_terminalizes_once() {
    let pc = page_container_with_resume_then_queue_error();
    pc.drive_file_io_service_once_owned(ServiceBudget::new(1), |_| true);
    assert_eq!(pc.owned_file_request_count_for_test(), 0);
    assert_eq!(pc.file_io_lease_count_for_test(), 0);
    assert_dirty_not_writeback(&pc, PageIndex::new(0));
}

#[test]
fn duplicate_completion_has_no_second_terminal_action() {
    let (pc, completion) = completed_writeback_fixture();
    pc.apply_file_io_completion_route(&completion);
    pc.apply_file_io_completion_route(&completion);
    assert_eq!(pc.terminal_action_count_for_test(completion.completion.id), 1);
}
```

Cover initial planning `None`, initial queue error, resume planning `None`,
resume queue error, device error, stale generation, duplicate completion,
read-target mismatch, and fsync control completion.

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-subsystems --lib backend_resume_queue_error_terminalizes_once -- --test-threads=1
cargo test -p tx-subsystems --lib duplicate_completion_has_no_second_terminal_action -- --test-threads=1
```

Expected: the resume queue-error test exposes the current missing cleanup; the
owner-table assertions do not compile.

- [ ] **Step 3: Replace the parallel cleanup maps**

```rust
struct PageContainerState {
    // existing PageBacked semantic state remains here
    owned_file_requests: BTreeMap<PageIoRequestId, FileIoOwnerToken>,
    fsync_submissions: BTreeMap<PageIoRequestId, FsyncSubmission>,
    // remove file_io_leases and file_io_read_targets
}
```

Prepare one owned request before planner entry. Before graph admission, every
`UnplannedSubmission` and `BackendSubmitError` calls `finish`. After admission,
transfer its payload bundle to L4 and insert only the owner token. Every resume
error and terminal route consumes the token plus the payload returned by L4 and
calls the same `finish` method. A missing payload is an owner-poison failure,
not permission to release the PageSlot independently.
Delete `release_file_io_read_target` and `abort_file_writeback_submission` from
production callsites.

Change `FileFsyncState` to own only the immutable PageBacked generation
frontier. After the frontier is clean, `vfs::FileFsyncOp` calls
`FsPageBacking::fsync_file`; it does not create a second PageIo fsync cleanup
path.

- [ ] **Step 4: Run GREEN, static callsite scan, and commit**

```sh
cargo test -p tx-subsystems --lib backend_resume_queue_error_terminalizes_once -- --test-threads=1
cargo test -p tx-subsystems --lib duplicate_completion_has_no_second_terminal_action -- --test-threads=1
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
rg -n 'abort_writeback|complete_writeback|release_file_io_read_target|abort_file_writeback_submission' crates/tx-subsystems/src/page_backed --glob '*.rs'
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-subsystems/src/page_backed crates/tx-subsystems/src/vfs/execution.rs
git commit -m "refactor(page-backed): centralize file I/O terminal settlement"
```

Expected scan: PageSlot low-level terminal calls occur only through the
PageBacked lifecycle facade and its tests; no ext4, VFS, or I/O-manager caller
can name a raw cleanup method.

### Task 5: Add `MutationHandle` and Owned Cross-Yield Tokens

**Files:**
- Create: `crates/tx-ext4/src/mutation_lifecycle.rs`
- Create: `crates/tx-ext4/tests/mutation_lifecycle.rs`
- Modify: `crates/tx-ext4/src/lib.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-ext4/src/mount.rs`
- Test: `crates/tx-ext4/tests/journal_prepared_transaction.rs`

- [ ] **Step 1: Write RED phase and custody tests**

```rust
#[test]
fn mutation_handle_owns_tokens_but_not_child_page_leases() {
    let handle = admitted_mutation_fixture();
    assert_eq!(handle.phase_for_test(), MutationPhase::Admitted);
    assert_eq!(handle.pending_terminal_count_for_test(), 2);
    assert_eq!(handle.page_data_lease_count_for_test(), 0);
    assert!(handle.has_frozen_metadata_for_test());
    assert!(handle.has_journal_extent_for_test());
}

#[test]
fn second_mutation_waits_for_owner_and_wakes_after_settlement() {
    let runtime = runtime_with_active_handle();
    let waiter = runtime.admit_or_queue(second_plan()).expect("queued");
    assert!(waiter.is_queued());
    runtime.settle_active_for_test().unwrap();
    assert!(runtime.woken_waiter_for_test(waiter.id()).is_some());
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4 --test mutation_lifecycle mutation_handle_ -- --test-threads=1
```

Expected: the module and types do not exist.

- [ ] **Step 3: Move lifecycle ownership out of `journal.rs`**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationPhase {
    Admitted,
    Prepared,
    CommitPending,
    CommittedNeedsSettlement,
    CheckpointPending,
    TailReclaimPending,
    AbortRequested,
    AbortDraining,
    CommitUnknown,
    RecoveryOnly,
    Settled,
    RolledBack,
}

pub struct MutationHandle {
    phase: MutationPhase,
    transaction: PreparedJournalTransaction,
    frozen: FrozenMetadataToken,
    journal_extent: Option<JournalExtentToken>,
    claims: Vec<AllocatorClaimToken>,
    graph_custody: GraphCustodyToken,
    pending_terminals: BTreeSet<GraphNodeId>,
    first_error: Option<Errno>,
}
```

`JournalExtentToken` consumes the old reservation and is the only caller of
raw ring release. `FrozenMetadataToken` owns immutable after-images and record
leases. `AllocatorClaimToken` owns admitted allocations or deferred frees.
`GraphCustodyToken` covers data, descriptor, revoke, commit, checkpoint, tail,
flush, and cache-settlement nodes, not only PageBacked child requests. Before
returning any `Yield`, admission has consumed local rollback guards into these
owned values. A normal caller queues a `MutationWaiter` when the mount owner is
busy; `EBUSY` is reserved for an internal try-admission helper.

Move `JournalFsyncSource`, `JournalTransactionState`, and
`JournalMutationRuntime` orchestration into `mutation_lifecycle.rs`. Keep JBD2
codec, record pool, graph, and ring mechanics in `journal.rs`; make raw
`discard` and `JournalRing::complete` `pub(crate)`.

- [ ] **Step 4: Run GREEN and enforce file-size guardrails**

```sh
cargo test -p tx-ext4 --test mutation_lifecycle -- --test-threads=1
cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1
cargo test -p tx-ext4 --lib --no-default-features journal
wc -l crates/tx-ext4/src/journal.rs crates/tx-ext4/src/mutation_lifecycle.rs
cargo xtask lint arch
cargo -q xtask unit
```

Expected: each authored Rust file is at most 1500 lines.

- [ ] **Step 5: Commit**

```sh
cargo fmt --check
git diff --check
git add crates/tx-ext4/src/journal.rs crates/tx-ext4/src/mutation_lifecycle.rs crates/tx-ext4/src/lib.rs crates/tx-ext4/src/mount.rs crates/tx-ext4/tests/mutation_lifecycle.rs crates/tx-ext4/tests/journal_prepared_transaction.rs
git commit -m "feat(ext4): own admitted mutations through settlement"
```

### Task 6: Close Abort Drain, Commit Unknown, and Checkpoint Retry

**Files:**
- Modify: `crates/tx-ext4/src/mutation_lifecycle.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Test: `crates/tx-ext4/tests/mutation_lifecycle.rs`
- Test: `crates/tx-ext4/tests/journal_prepared_transaction.rs`

- [ ] **Step 1: Write the failure matrix as RED parameterized tests**

```rust
#[test]
fn submitted_precommit_error_drains_graph_nodes_before_rollback() {
    let mut handle = handle_with_two_submitted_nodes();
    handle.fail_before_commit(Errno::EIO);
    assert_eq!(handle.phase_for_test(), MutationPhase::AbortDraining);
    handle.complete_node(first_node(), Err(Errno::EIO));
    assert_eq!(handle.phase_for_test(), MutationPhase::AbortDraining);
    handle.complete_node(second_node(), Ok(()));
    assert_eq!(handle.phase_for_test(), MutationPhase::RolledBack);
    assert!(handle.all_resources_released_for_test());
}

#[test]
fn checkpoint_error_retains_committed_authority_for_retry() {
    let mut handle = committed_handle();
    handle.complete_checkpoint(Err(Errno::EIO));
    assert_eq!(handle.phase_for_test(), MutationPhase::CommittedNeedsSettlement);
    assert!(handle.has_journal_extent_for_test());
    handle.retry_checkpoint().expect("retry admitted");
}
```

Also test pre-admission rollback, graph validation failure, data error,
descriptor error, commit error known-not-durable, ambiguous commit completion,
duplicate node completion, stale node completion, checkpoint error, tail error,
cache-settlement error, retry exhaustion to `RecoveryOnly`, and a queued waiter
that is woken after the active handle settles.

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4 --test mutation_lifecycle submitted_precommit_error_drains_graph_nodes -- --test-threads=1
cargo test -p tx-ext4 --test mutation_lifecycle checkpoint_error_retains -- --test-threads=1
```

- [ ] **Step 3: Implement one transition function**

```rust
impl MutationHandle {
    fn transition(&mut self, event: MutationEvent) -> Result<MutationAction, MutationError> {
        match (self.phase, event) {
            (MutationPhase::Prepared | MutationPhase::CommitPending, MutationEvent::PrecommitError(errno)) => {
                self.first_error.get_or_insert(errno);
                self.phase = if self.pending_terminals.is_empty() {
                    MutationPhase::RolledBack
                } else {
                    MutationPhase::AbortDraining
                };
                Ok(MutationAction::DrainOrRollback)
            }
            (MutationPhase::CommitPending, MutationEvent::CommitAmbiguous) => {
                self.phase = MutationPhase::CommitUnknown;
                Ok(MutationAction::EnterRecoveryOnly)
            }
            (MutationPhase::CheckpointPending, MutationEvent::CheckpointFailed(errno)) => {
                self.first_error.get_or_insert(errno);
                self.phase = MutationPhase::CommittedNeedsSettlement;
                Ok(MutationAction::RetryCheckpoint)
            }
            (MutationPhase::CommittedNeedsSettlement
                | MutationPhase::CheckpointPending
                | MutationPhase::TailReclaimPending,
                MutationEvent::RetryExhausted(errno)
                | MutationEvent::TailFailed(errno)
                | MutationEvent::CacheSettlementFailed(errno)) => {
                self.first_error.get_or_insert(errno);
                self.phase = MutationPhase::RecoveryOnly;
                Ok(MutationAction::RetainForRecovery)
            }
            _ => Err(MutationError::InvalidTransition),
        }
    }
}
```

All terminal resource actions are selected by `MutationAction`; callers never
invoke ring completion, transaction discard, claim release, or child cleanup.

- [ ] **Step 4: Run GREEN, matrix regression, and commit**

```sh
cargo test -p tx-ext4 --test mutation_lifecycle -- --test-threads=1
cargo test -p tx-ext4 --test journal_prepared_transaction -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-ext4/src/mutation_lifecycle.rs crates/tx-ext4/src/journal.rs crates/tx-ext4/tests/mutation_lifecycle.rs crates/tx-ext4/tests/journal_prepared_transaction.rs
git commit -m "fix(ext4): drain and retry mutation failures exactly once"
```

### Task 7: Implement Real FUA and Flush Durability

**Files:**
- Modify: `crates/tx-subsystems/src/device.rs`
- Modify: `crates/tx-subsystems/src/io_manager/block/mod.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-ext4/src/mutation_lifecycle.rs`
- Modify: `crates/tx-drivers/src/virtio/blk.rs`
- Modify: `crates/tx-drivers/src/virtio/mmio.rs`
- Test: `crates/tx-subsystems/src/device.rs`
- Test: `crates/tx-ext4/tests/journal_transaction_plan.rs`

- [ ] **Step 1: Write RED device-trace tests**

```rust
#[test]
fn unsupported_fua_commit_gets_post_commit_flush() {
    let graph = transaction_plan().commit_graph_after_data(BlockDurabilityCapabilities {
        fua: false,
        flush: true,
    }).unwrap();
    assert_eq!(trace_graph(&graph), ["journal-body", "flush", "commit", "flush"]);
}

#[test]
fn dispatch_adapter_passes_fua_to_capable_device() {
    let trace = execute_write(BlockFlags::FUA, capable_device());
    assert_eq!(trace, [DeviceTrace::Write { fua: true }]);
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-subsystems --lib dispatch_adapter_passes_fua -- --test-threads=1
cargo test -p tx-ext4 --test journal_transaction_plan unsupported_fua_commit -- --test-threads=1
```

Expected: the adapter currently ignores write flags and the plan has no
capability input.

- [ ] **Step 3: Add capability-aware device admission**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockDurabilityCapabilities {
    pub fua: bool,
    pub flush: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockWriteOptions {
    pub fua: bool,
}
```

Add default `BlockDeviceOps::durability_capabilities() -> NONE` and
`write_blocks_with_options`; the default rejects `fua: true`. The dispatch
adapter maps `BlockFlags::FUA` into options and rejects flush/barrier when the
device did not advertise it. The RV64 virtio bridge advertises actual support;
when virtio FUA is unavailable, the ext4 graph emits the explicit flush
fallback.

Build the full sequence as graph and handle phases:

```text
data -> flush -> descriptor/metadata/revoke -> flush -> commit(FUA|plain)
     -> optional flush -> checkpoint -> flush -> tail(FUA|plain)
     -> optional flush -> cache settlement
```

- [ ] **Step 4: Run GREEN and commit**

```sh
cargo test -p tx-subsystems --lib block_device_dispatch_adapter -- --test-threads=1
cargo test -p tx-ext4 --test journal_transaction_plan -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-subsystems/src/device.rs crates/tx-subsystems/src/io_manager/block/mod.rs crates/tx-ext4/src/journal.rs crates/tx-ext4/src/mutation_lifecycle.rs crates/tx-drivers/src/virtio/blk.rs crates/tx-drivers/src/virtio/mmio.rs crates/tx-ext4/tests/journal_transaction_plan.rs
git commit -m "fix(ext4): enforce device durability flags"
```

### Task 8: Gate Replay on Recovery State and Refresh Caches

**Files:**
- Modify: `crates/tx-ext4-format/src/ondisk.rs`
- Modify: `crates/tx-ext4-format/src/journal_replay.rs`
- Modify: `crates/tx-ext4-format/src/pager.rs`
- Modify: `crates/tx-ext4/src/mount.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-fs/src/tx_ext4_bridge.rs`
- Modify: `crates/tx-fs/src/fat_bridge.rs`
- Test: `crates/tx-ext4-format/tests/jbd2_recovery.rs`
- Test: `crates/tx-ext4-format/tests/pager_mock.rs`
- Test: `crates/tx-ext4-format/tests/host_tools.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`

- [ ] **Step 1: Import/rewrite the RED witnesses from `555e4a40`**

```rust
#[test]
fn stale_journal_is_not_replayed_when_ext4_is_clean() {
    let mut image = clean_fs_with_stale_journal_start();
    let report = recover_if_required(&mut image).unwrap();
    assert_eq!(report, RecoveryReport::NotRequired);
    assert_eq!(image.home_block(), ORIGINAL_HOME);
}

#[test]
fn checkpoint_refreshes_all_format_and_bridge_caches() {
    let mounted = mounted_cache_fixture();
    mounted.checkpoint_after_image(7, UPDATED_BLOCK).unwrap();
    assert_eq!(mounted.read_via_pager(7), UPDATED_BLOCK);
    assert_eq!(mounted.read_via_bridge(7), UPDATED_BLOCK);
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4-format --test jbd2_recovery stale_journal_is_not_replayed -- --test-threads=1
cargo test -p tx-ext4 --lib checkpoint_refreshes_all_format -- --test-threads=1
```

- [ ] **Step 3: Implement recovery admission and cache settlement**

```rust
pub enum RecoveryReport {
    NotRequired,
    Replayed { transactions: u32, blocks: u32, next_sequence: u32 },
}

pub trait BlockImage {
    fn read_block(&self, block: u64, out: &mut Page4K) -> Result<()>;
    fn write_block(&self, block: u64, data: &Page4K) -> Result<()>;
    fn barrier(&self) -> Result<()>;
    fn invalidate(&self, block: u64);
}
```

Make `barrier` required: production cannot inherit a successful no-op.
`mount_ext4_read_write_with_discovered_journal` reads ext4 recovery state,
replays only when required, validates JBD2 checksum/sequence, refreshes pager
superblock/GDT and lookup/inode/directory/bridge caches, then durably sets
recovery-required before allowing the first RW mutation. Ordinary transaction
settlement never clears global recovery state.

Update every `BlockImage` implementation in the same checkpoint, including the
Tx ext4 and FAT bridges plus `host_tools`, `pager_mock`, `jbd2_recovery`, and
`tests_v3`. Their invalidation behavior must be visible in the pager and bridge
fixtures; no implementation may retain stale mount-time superblock or GDT data
after settlement.

- [ ] **Step 4: Run GREEN and commit**

```sh
cargo test -p tx-ext4-format --test jbd2_recovery -- --test-threads=1
cargo test -p tx-ext4-format --test pager_mock -- --test-threads=1
cargo test -p tx-ext4 --lib recovery -- --test-threads=1
cargo test -p tx-fs tx_ext4_bridge -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-ext4-format/src/ondisk.rs crates/tx-ext4-format/src/journal_replay.rs crates/tx-ext4-format/src/pager.rs crates/tx-ext4/src/mount.rs crates/tx-ext4/src/read_backend.rs crates/tx-fs/src/tx_ext4_bridge.rs crates/tx-fs/src/fat_bridge.rs crates/tx-ext4-format/tests/jbd2_recovery.rs crates/tx-ext4-format/tests/pager_mock.rs crates/tx-ext4-format/tests/host_tools.rs crates/tx-ext4/src/tests_v3.rs
git commit -m "fix(ext4): gate recovery and settle caches"
```

### Task 9: Add Revoke and Deferred-Free Ownership

**Files:**
- Modify: `crates/tx-ext4-format/src/mutation.rs`
- Modify: `crates/tx-ext4-format/src/journal.rs`
- Modify: `crates/tx-ext4/src/journal.rs`
- Modify: `crates/tx-ext4/src/mutation_lifecycle.rs`
- Test: `crates/tx-ext4-format/tests/jbd2_transaction_image.rs`
- Test: `crates/tx-ext4/tests/journal_transaction_plan.rs`
- Test: `crates/tx-ext4/tests/mutation_lifecycle.rs`

- [ ] **Step 1: Write RED revoke and reuse tests**

```rust
#[test]
fn freeing_plan_emits_revoke_and_retains_deferred_free() {
    let plan = truncate_free_plan(33);
    assert_eq!(plan.revokes, vec![RevokeRecord { physical_block: 33 }]);
    assert_eq!(plan.deferred_frees, vec![DeferredFreeClaim { physical_block: 33 }]);
}

#[test]
fn allocator_cannot_reuse_before_tail_reclaim() {
    let mut handle = committed_freeing_handle(33);
    assert_eq!(handle.try_reuse_for_test(33), Err(Errno::EBUSY));
    handle.complete_checkpoint(Ok(())).unwrap();
    assert_eq!(handle.try_reuse_for_test(33), Err(Errno::EBUSY));
    handle.complete_tail_reclaim(Ok(())).unwrap();
    assert_eq!(handle.try_reuse_for_test(33), Ok(()));
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4-format --test jbd2_transaction_image freeing_plan_emits_revoke -- --test-threads=1
cargo test -p tx-ext4 --test mutation_lifecycle allocator_cannot_reuse -- --test-threads=1
```

- [ ] **Step 3: Extend immutable plans and journal layout**

```rust
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RevokeRecord {
    pub physical_block: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DeferredFreeClaim {
    pub physical_block: u64,
}

pub struct Ext4MutationPlan {
    // existing origin/object/fsync/data/metadata/allocation fields
    pub revokes: Vec<RevokeRecord>,
    pub deferred_frees: Vec<DeferredFreeClaim>,
}
```

Sort and deduplicate revoke blocks before encoding. Reserve descriptor,
metadata, revoke, and commit record space as one `JournalExtentToken`.
`MutationHandle` keeps deferred frees unavailable until checkpoint, safe tail
advance, and extent reclamation all succeed.

- [ ] **Step 4: Run GREEN and commit**

```sh
cargo test -p tx-ext4-format --test jbd2_transaction_image -- --test-threads=1
cargo test -p tx-ext4 --test journal_transaction_plan -- --test-threads=1
cargo test -p tx-ext4 --test mutation_lifecycle -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-ext4-format/src/mutation.rs crates/tx-ext4-format/src/journal.rs crates/tx-ext4/src/journal.rs crates/tx-ext4/src/mutation_lifecycle.rs crates/tx-ext4-format/tests/jbd2_transaction_image.rs crates/tx-ext4/tests/journal_transaction_plan.rs crates/tx-ext4/tests/mutation_lifecycle.rs
git commit -m "feat(ext4): retain revoke and deferred-free claims"
```

### Task 10: Migrate Setattr as the First Complete Vertical Slice

**Files:**
- Modify: `crates/tx-ext4-format/src/mutation.rs`
- Modify: `crates/tx-ext4-format/src/ondisk.rs`
- Modify: `crates/tx-ext4-format/src/pager.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4/src/mutation_lifecycle.rs`
- Test: `crates/tx-ext4-format/tests/host_tools.rs`
- Test: `crates/tx-ext4-format/tests/pager_mock.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`

- [ ] **Step 1: Import/rewrite checksum RED cases from `1cee426a`**

```rust
#[test]
fn setattr_plan_preserves_unknown_fields_and_updates_inode_checksum() {
    let fixture = metadata_csum_inode_fixture();
    let plan = fixture.plan_setattr(SetAttr::Mode(0o100755)).unwrap();
    let inode = plan.single_inode_after_image();
    assert_eq!(inode.mode(), 0o100755);
    assert_eq!(inode.unknown_bytes(), fixture.original_unknown_bytes());
    assert!(inode.checksum_is_valid(fixture.uuid(), fixture.inode_no()));
}

#[test]
fn setattr_is_not_published_before_terminal_settlement() {
    let mounted = setattr_failure_fixture(DurabilityCut::BeforeCommit);
    assert_errno(mounted.chmod(12, 0o755), Errno::EIO);
    assert_eq!(mounted.project_mode(12), 0o644);
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4-format --test pager_mock setattr_plan_preserves -- --test-threads=1
cargo test -p tx-ext4 --lib setattr_is_not_published -- --test-threads=1
```

- [ ] **Step 3: Implement pure setattr planning and handle publication**

```rust
pub enum MutationOrigin {
    FlushPage,
    SetAttr,
    Create,
    Mkdir,
    Link,
    Symlink,
    Unlink,
    Rmdir,
    Rename,
    Truncate,
    Orphan,
}

pub enum SetAttr {
    Mode(u16),
    Owner { uid: Option<u32>, gid: Option<u32> },
    Times { atime_ns: Option<u64>, mtime_ns: Option<u64>, ctime_ns: u64 },
}
```

The planner reads the complete inode block, preserves unknown fields, writes
all affected timestamp/mode/owner fields, and recomputes metadata checksum.
`serialize_inode_meta`, chmod, chown, and utimens submit the plan through the
mount's `MutationHandle`; RNode and inode caches update only after checkpoint,
tail, and cache settlement. Retire production use of
`write_inode_meta_journaled`.

- [ ] **Step 4: Verify host format, failure cuts, and commit**

```sh
cargo test -p tx-ext4-format --test host_tools setattr -- --test-threads=1
cargo test -p tx-ext4-format --test pager_mock setattr -- --test-threads=1
cargo test -p tx-ext4 --lib setattr -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-ext4-format/src/mutation.rs crates/tx-ext4-format/src/ondisk.rs crates/tx-ext4-format/src/pager.rs crates/tx-ext4/src/namespace.rs crates/tx-ext4/src/mutation_lifecycle.rs crates/tx-ext4-format/tests/host_tools.rs crates/tx-ext4-format/tests/pager_mock.rs crates/tx-ext4/src/tests_v3.rs
git commit -m "feat(ext4): settle setattr through mutation ownership"
```

### Task 11: Migrate Buffered Write, Bounded Extents, and Truncate

**Files:**
- Modify: `crates/tx-subsystems/src/page_backed/lifecycle.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-ext4-format/src/mutation.rs`
- Modify: `crates/tx-ext4-format/src/pager.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/pager.rs`
- Modify: `crates/tx-ext4/src/mutation_lifecycle.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`
- Test: `crates/tx-ext4-format/tests/pager_mock.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`

- [ ] **Step 1: Write RED multi-page and free/reuse tests**

```rust
#[test]
fn file_settlement_batches_the_captured_multi_page_frontier() {
    let pc = dirty_file_pages([0, 1, 2]);
    let frontier = pc.snapshot_file_fsync_frontier().unwrap();
    let requests = pc.admit_frontier_for_test(&frontier).unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].lease_page_count_for_test(), 3);
}

#[test]
fn truncate_frees_extent_only_after_revoke_tail_settlement() {
    let mounted = allocated_file_fixture(4);
    mounted.truncate_for_test(1).unwrap();
    assert!(mounted.deferred_free_for_test(old_last_block()));
    mounted.settle_for_test().unwrap();
    assert!(!mounted.deferred_free_for_test(old_last_block()));
}

#[test]
fn extending_write_preflight_rejects_before_dirty_publication() {
    let mounted = bounded_profile_fixture();
    assert_errno(mounted.write_beyond_supported_extent(8), Errno::EOPNOTSUPP);
    assert!(!mounted.page_is_dirty_for_test(8));
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-subsystems --lib file_settlement_batches -- --test-threads=1
cargo test -p tx-ext4 --lib truncate_frees_extent_only -- --test-threads=1
```

- [ ] **Step 3: Implement bounded Tier 1 data planning**

One `OwnedFileIoRequest` may carry one multi-page `PageDataLease`. The
PageBacked lifecycle first creates a `PageDataLeaseProjection`; the ext4
adapter consumes only its neutral `IoDataSource` segments without copying
payload bytes and records only the child request ID in `MutationHandle`.

The format plan covers data mapping plus every changed inode, extent node,
block bitmap, group descriptor, superblock accounting field, checksum, revoke,
and deferred free. Multiple logical edits to one home block are applied to one
builder before `push_metadata`, so the final plan has one after-image per home
block.

Already-mapped writes may publish a dirty PageSlot and join a later fsync
frontier. An extending or allocating write must first run the bounded extent,
bitmap, checksum, revoke, and journal-space preflight; unsupported depth
growth or a second depth-1 child returns `EOPNOTSUPP` before any claim, dirty
mark, PageSlot transition, or home-block write.

Ordinary buffered write returns after PageSlot dirty publication. File fsync
captures a generation frontier once and settles all matching data and metadata;
later redirty belongs to a later frontier.

- [ ] **Step 4: Run GREEN, zero-copy representation checks, and commit**

```sh
cargo test -p tx-subsystems --lib file_settlement_batches -- --test-threads=1
cargo test -p tx-subsystems --lib page_backed -- --test-threads=1
cargo test -p tx-ext4-format --test pager_mock writeback -- --test-threads=1
cargo test -p tx-ext4 --lib writeback -- --test-threads=1
cargo test -p tx-ext4 --lib truncate -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-subsystems/src/page_backed/lifecycle.rs crates/tx-subsystems/src/page_backed/mod.rs crates/tx-subsystems/src/page_backed/core_tests.rs crates/tx-ext4-format/src/mutation.rs crates/tx-ext4-format/src/pager.rs crates/tx-ext4-format/tests/pager_mock.rs crates/tx-ext4/src/read_backend.rs crates/tx-ext4/src/pager.rs crates/tx-ext4/src/mutation_lifecycle.rs crates/tx-ext4/src/tests_v3.rs
git commit -m "feat(ext4): settle buffered data and truncate atomically"
```

### Task 12: Migrate Namespace and Classic Orphan Operations

**Files:**
- Modify: `crates/tx-ext4-format/src/mutation.rs`
- Modify: `crates/tx-ext4-format/src/pager.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/mutation_lifecycle.rs`
- Test: `crates/tx-ext4-format/tests/pager_mock.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`
- Test: `crates/tx-ext4/tests/mutation_lifecycle.rs`

- [ ] **Step 1: Write RED operation-matrix tests**

```rust
#[test]
fn rename_overwrite_is_one_complete_mutation() {
    let plan = fixture().plan_rename(2, b"old", 3, b"new").unwrap();
    assert_eq!(plan.origin, MutationOrigin::Rename);
    assert!(plan.covers_directory(2));
    assert!(plan.covers_directory(3));
    assert!(plan.covers_replaced_inode());
    assert!(plan.has_revoke_for_replaced_blocks());
}

#[test]
fn unlinked_open_inode_survives_until_last_close_then_orphan_settles() {
    let mounted = unlinked_open_fixture();
    mounted.unlink_for_test().unwrap();
    assert!(mounted.read_open_file_for_test().is_ok());
    mounted.close_last_open_for_test().unwrap();
    assert!(mounted.inode_reclaimed_after_settlement_for_test());
}
```

Add exact fixtures for create, mkdir, link, inline/block symlink, unlink,
rmdir, same-directory rename, cross-directory rename, rename-overwrite,
unlinked-open close, and crash-truncate orphan cleanup.

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4-format --test pager_mock namespace_plan_ -- --test-threads=1
cargo test -p tx-ext4 --lib rename_overwrite_is_one -- --test-threads=1
cargo test -p tx-ext4 --lib unlinked_open_inode_survives -- --test-threads=1
```

- [ ] **Step 3: Replace direct pager mutation with complete plans**

Each planner returns a single `Ext4MutationPlan` containing all directory,
inode, bitmap, GDT, superblock, link-count, timestamp, checksum, orphan,
revoke, and deferred-free after-images. `rename` never performs
remove-destination, add-new, remove-old as separate writes. VFS namespace and
cache publication occurs only when `MutationHandle` reaches `Settled`.

Linear and non-splitting htree shapes are admitted. A split/rebalance request
returns `EOPNOTSUPP` before claims or publication. Classic orphan-chain updates
are journaled; `orphan_file` is rejected by the Tier 1 admission gate.

- [ ] **Step 4: Run GREEN, image checks, and commit**

```sh
cargo test -p tx-ext4-format --test pager_mock namespace_plan_ -- --test-threads=1
cargo test -p tx-ext4 --lib namespace -- --test-threads=1
cargo test -p tx-ext4 --lib orphan -- --test-threads=1
cargo test -p tx-ext4 --test mutation_lifecycle free_reuse -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-ext4-format/src/mutation.rs crates/tx-ext4-format/src/pager.rs crates/tx-ext4-format/tests/pager_mock.rs crates/tx-ext4/src/namespace.rs crates/tx-ext4/src/read_backend.rs crates/tx-ext4/src/mutation_lifecycle.rs crates/tx-ext4/src/tests_v3.rs crates/tx-ext4/tests/mutation_lifecycle.rs
git commit -m "feat(ext4): journal namespace and orphan lifecycles"
```

### Task 13: Add `MountSettlementOp`, Errseq, and Syscall Routing

**Files:**
- Create: `crates/tx-subsystems/src/page_backed/error_seq.rs`
- Create: `crates/tx-subsystems/src/mount/settlement.rs`
- Create: `crates/tx-ext4/src/settlement.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/page_backed/fs_page_backing.rs`
- Modify: `crates/tx-subsystems/src/mount/mod.rs`
- Modify: `crates/tx-subsystems/src/vfs/execution.rs`
- Modify: `crates/tx-subsystems/src/vfs/structure.rs`
- Modify: `crates/tx-ext4/src/lib.rs`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-shims/src/linux_syscall/fs_basic/dir_sync.rs`
- Modify: `crates/tx-shims/src/linux_syscall/fs_mut.rs`
- Test: `crates/tx-subsystems/src/mount/settlement.rs`
- Test: `crates/tx-shims/src/linux_syscall/tests/fd_ops_wave2.rs`

- [ ] **Step 1: Write RED scope, error, and detach tests**

```rust
#[test]
fn syncfs_reports_old_unobserved_error_once() {
    let (mount, mut cursor) = mount_with_error(Errno::EIO);
    assert_eq!(mount.observe_error(&mut cursor), Some(Errno::EIO));
    assert_eq!(mount.observe_error(&mut cursor), None);
}

#[test]
fn normal_umount_settles_before_topology_withdrawal() {
    let trace = run_normal_umount_fixture();
    assert_eq!(trace, ["busy_check", "quiesce", "checkpoint", "tail", "clean", "flush", "detach"]);
}

#[test]
fn normal_umount_busy_check_prevents_partial_quiesce() {
    let fixture = mount_with_active_payload_user();
    assert_errno(fixture.normal_umount(), Errno::EBUSY);
    assert_eq!(fixture.payload_state(), MountRuntimeState::Open);
}

#[test]
fn lazy_detach_retains_payload_until_background_settlement() {
    let fixture = run_lazy_detach_with_open_user_fixture();
    assert!(!fixture.topology_visible());
    assert_eq!(fixture.payload_state(), MountRuntimeState::DetachedPending);
    assert!(!fixture.settlement_started());
    fixture.release_last_payload_user();
    fixture.drive_background_mount_settlement_once();
    fixture.finish_background_settlement();
    assert_eq!(fixture.payload_state(), MountRuntimeState::Detached);
}
```

Also test regular and directory fsync, fdatasync as stronger fsync, mount
frontier capture for syncfs, all-visible-mount capture for sync, sync's
no-error return contract, normal umount error propagation, unsupported
`MNT_FORCE`, and lazy-detach payload pin retention.

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-subsystems --lib syncfs_reports_old_unobserved_error -- --test-threads=1
cargo test -p tx-subsystems --lib normal_umount_settles -- --test-threads=1
cargo test -p tx-shims --lib lazy_detach_retains_payload -- --test-threads=1
```

- [ ] **Step 3: Implement the single mount lifecycle operation**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MountRuntimeState {
    Open,
    Quiescing,
    RecoveryOnly,
    DetachedPending,
    Detached,
}

pub struct MountRuntimeCell {
    state: MountRuntimeState,
    active_payload_users: u32,
    active_settlement: Option<SettlementId>,
    error_seq: ErrorSeq,
}

pub enum SettlementScope {
    File { object: FsObjectId, generation_frontier: FileFsyncFrontier },
    Mount { transaction_frontier: u64 },
    Detach,
}

pub struct MountSettlementOp {
    payload: MountPayloadPin,
    scope: SettlementScope,
    phase: SettlementPhase,
    page_error_cursor: ErrorCursor,
    file_error_cursor: ErrorCursor,
    mount_error_cursor: ErrorCursor,
}
```

`MountPayload` owns one `MountRuntimeCell`; all mutation admission and
settlement operations atomically claim the cell before inspecting or changing
`Open`, `Quiescing`, `RecoveryOnly`, or `DetachedPending`. `MountSettlementOp`
owns its payload pin across yields. For file scope it drives the PageBacked
generation frontier then ext4 settlement. Mount scope captures the backend
transaction frontier once. Normal umount runs
`no_active_payload_users` before changing the cell to `Quiescing`; a failed
busy check leaves the mount `Open`.

`MNT_DETACH` withdraws topology but does not start shutdown while external
payload users remain. The cell stays `DetachedPending`; the mount settlement
queue registers the pinned payload only after the last user releases, wakes the
mount settlement driver, and requeues the same `MountSettlementOp` on retryable
failure. The queue is a scheduler bridge, not a fourth lifecycle owner.

Add packed `ErrorSeq`/`ErrorCursor`; `OpenFile` samples the mount sequence at
open and retains page and file cursors. `fsync`/`fdatasync` observe page, file,
and mount errors; `syncfs` observes only the mount cursor. `sync()` records
errors for all captured mounts but returns Linux success.

Add default `FsOps::shutdown` returning `Done(())`; ext4 overrides it by
routing to detach scope. Add `MountNamespace::snapshot_payload_pins()` so sync
captures the root plus all visible mounted payloads without exposing the
private topology vector.

- [ ] **Step 4: Route syscalls and run GREEN**

`fsync` and `fdatasync` use file scope; directory fsync captures namespace
transactions. `syncfs` uses mount scope. `sync` drives a captured vector of
visible mount payload pins. Normal `umount2(..., 0)` first checks
`no_active_payload_users`, then settles before calling `MountNamespace::umount`.
`MNT_DETACH` withdraws topology first and leaves the pinned payload in the
mount settlement queue until the last user releases; the queue driver owns
retry/wake registration. `MNT_FORCE` returns `EOPNOTSUPP`.

```sh
cargo test -p tx-subsystems --lib mount_settlement -- --test-threads=1
cargo test -p tx-shims --lib fsync -- --test-threads=1
cargo test -p tx-shims --lib syncfs -- --test-threads=1
cargo test -p tx-shims --lib umount -- --test-threads=1
cargo -q xtask unit
cargo fmt --check
git diff --check
```

- [ ] **Step 5: Commit**

```sh
git add crates/tx-subsystems/src/page_backed/error_seq.rs crates/tx-subsystems/src/page_backed/mod.rs crates/tx-subsystems/src/page_backed/fs_page_backing.rs crates/tx-subsystems/src/mount/mod.rs crates/tx-subsystems/src/mount/settlement.rs crates/tx-subsystems/src/vfs/execution.rs crates/tx-subsystems/src/vfs/structure.rs crates/tx-ext4/src/lib.rs crates/tx-ext4/src/read_backend.rs crates/tx-ext4/src/settlement.rs crates/tx-shims/src/linux_syscall/fs_basic/dir_sync.rs crates/tx-shims/src/linux_syscall/fs_mut.rs crates/tx-shims/src/linux_syscall/tests/fd_ops_wave2.rs
git commit -m "feat(ext4): settle file mount and detach frontiers"
```

### Task 14: Cut Production to One RW Path and Add G0 Lints

**Files:**
- Modify: `crates/tx-ext4/src/mount.rs`
- Modify: `crates/tx-ext4/src/namespace.rs`
- Modify: `crates/tx-ext4-format/src/pager.rs`
- Modify: `crates/tx-fs/src/tx_ext4_bridge.rs`
- Modify: `crates/tx-kernel/src/init.rs`
- Modify: `xtask/src/lint.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`
- Test: `xtask/src/lint.rs`

- [ ] **Step 1: Write RED static and reachability tests**

```rust
#[test]
fn production_rw_mount_requires_discovered_journal_runtime() {
    assert!(mount_ext4_read_write(test_image()).is_err());
    assert!(mount_ext4_read_write_with_discovered_journal(
        test_image(), geometry(), device(), pool()
    ).is_ok());
}

#[test]
fn ext4_direct_home_write_lint_rejects_namespace_callsite() {
    let source = "pager.create_regular_file(parent, name, mode, uid, gid, 0)";
    assert!(check_ext4_no_direct_home_write(source, "crates/tx-ext4/src/namespace.rs").is_err());
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p tx-ext4 --lib production_rw_mount_requires -- --test-threads=1
cargo test -p xtask ext4_direct_home_write_lint -- --test-threads=1
```

- [ ] **Step 3: Remove production bypasses and add three lint rules**

Keep read-only and host-oracle constructors. Remove or gate production RW
constructors that lack discovered journal, durability capability, recovery
admission, and mutation ownership. Move direct pager mutation helpers behind
`#[cfg(test)]` or an explicit host-oracle module.

Add these hard rules:

```text
ext4-lifecycle-ownership   raw PageSlot/ring cleanup outside owner modules
ext4-no-direct-home-write production tx-ext4 calls to format pager write helpers
ext4-durability-flags      production commit/tail writes without FUA or flush fallback
```

Kernel boot and dynamic mount select only
`mount_ext4_read_write_with_discovered_journal` for RW.

- [ ] **Step 4: Run G0 and commit**

```sh
cargo test -p xtask ext4_ -- --test-threads=1
cargo xtask lint invariants ext4-lifecycle-ownership
cargo xtask lint invariants ext4-no-direct-home-write
cargo xtask lint invariants ext4-durability-flags
cargo xtask lint arch
cargo xtask lint unused
cargo -q xtask unit
cargo fmt --check
git diff --check
git add crates/tx-ext4/src/mount.rs crates/tx-ext4/src/namespace.rs crates/tx-ext4/src/tests_v3.rs crates/tx-ext4-format/src/pager.rs crates/tx-fs/src/tx_ext4_bridge.rs crates/tx-kernel/src/init.rs xtask/src/lint.rs
git commit -m "refactor(ext4): enforce one production durability path"
```

### Task 15: Add One `cargo xtask ext4 tier1` Runner

**Files:**
- Create: `xtask/src/ext4/mod.rs`
- Create: `xtask/src/ext4/run_workspace.rs`
- Create: `xtask/src/ext4/receipt.rs`
- Create: `xtask/src/ext4/tests.rs`
- Create: `tools/ext4/tier1/xfstests-selection.json`
- Create: `tools/ext4/tier1/crash-cuts.json`
- Create: `tools/shell-tests/ext4-tier1.scn`
- Modify: `xtask/src/lib.rs`
- Modify: `.agents/skills/tx-xtask/SKILL.md`
- Modify: `xtask/Cargo.toml` only if receipt hashing needs one audited dependency

- [ ] **Step 1: Write RED command and workspace-lifecycle tests**

```rust
#[test]
fn run_workspace_finalizes_once_and_cleans_temporary_state() {
    let root = temp_root();
    let mut run = RunWorkspace::create(&root, "test-run").unwrap();
    run.record_artifact("scratch", root.join("scratch.img")).unwrap();
    let receipt = run.finalize().unwrap();
    assert!(receipt.exists());
    assert!(!run.temporary_path_for_test().exists());
}

#[test]
fn run_workspace_failure_kills_children_writes_receipt_and_cleans_on_drop() {
    let root = temp_root();
    let mut run = RunWorkspace::create(&root, "failed-run").unwrap();
    let child = run.spawn_test_child("exit 17").unwrap();
    run.record_child(child);
    run.mark_failed_for_test("child-exit");
    drop(run);
    assert!(root.join("failed-run/failed-receipt.json").exists());
    assert!(!root.join("failed-run/.tmp").exists());
}

#[test]
fn tier1_rejects_missing_or_stale_authority_inputs() {
    let error = parse_tier1_args(&["tier1", "--dry-run"], fixture_without_cut_catalog())
        .expect_err("authority is mandatory");
    assert!(error.contains("tx.ext4.crash_cut_catalog.v1"));
}
```

- [ ] **Step 2: Run RED**

```sh
cargo test -p xtask ext4 -- --test-threads=1
cargo xtask ext4 tier1 --dry-run
```

Expected: module/command missing.

- [ ] **Step 3: Implement one public command and one internal owner**

```rust
pub(crate) fn ext4(root: &Path, args: Vec<String>) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("tier1") => run_tier1(root, &args[1..]),
        _ => Err("ext4 command needs subcommand: tier1".into()),
    }
}

pub(crate) struct RunWorkspace {
    run_id: String,
    temporary: PathBuf,
    final_dir: PathBuf,
    child_processes: Vec<Child>,
    artifacts: BTreeMap<String, PathBuf>,
    finalized: bool,
}
```

`RunWorkspace` creates fresh TEST/SCRATCH/WORKLOAD images, owns every child
QEMU/e2fsck/xfstests process, copies immutable crash images, finalizes by
atomic directory rename, and cleans temporary state on all pre-finalization
exits. Its `Drop` implementation kills and waits every still-live child,
writes a failed receipt if no final receipt exists, and removes only its own
temporary directory; process failure, panic unwinding, and explicit command
errors all use this path. On failure it writes a failed receipt before cleanup.
Users and CI never invoke provision/bundle/availability/shell-env/metadata/
cleanup subcommands.

The public sequence is fixed:

```text
build -> fresh images -> guest matrix -> deterministic cuts -> replay
      -> immutable copies -> e2fsck -fn -> pinned xfstests -> receipt
```

`tools/ext4/tier1/crash-cuts.json` enumerates the stable D0-D12 durability
families from the active design and expands each free-capable mutation with
revoke-durability and first-reuse cuts. Development may select a reduced set,
but the product runner rejects a catalog that omits any stable family and
requires 1000 deterministic expanded cuts.

The receipt is not a summary-only DTO. It records the candidate commit and
tool revisions, SHA-256 values for the capability ledger, crash catalog,
xfstests selection and shell scenario, distinct TEST/SCRATCH/WORKLOAD image
identities, one result row and one successful `e2fsck -fn` artifact hash per
immutable image, and the selected xfstests result projection. `RunWorkspace`
rejects finalization when any binding is missing or when two role-image hashes
are equal.

- [ ] **Step 4: Run GREEN and command-contract regression**

```sh
cargo test -p xtask ext4 -- --test-threads=1
cargo xtask ext4 tier1 --dry-run
cargo fmt --check --package xtask
cargo -q xtask unit
cargo xtask lint docs
git diff --check
```

Expected dry-run: one ordered action list and resolved authority hashes; no
image, process, or final receipt is created.

- [ ] **Step 5: Commit**

```sh
git add xtask/src/ext4 xtask/src/lib.rs xtask/Cargo.toml tools/ext4/tier1/xfstests-selection.json tools/ext4/tier1/crash-cuts.json tools/shell-tests/ext4-tier1.scn .agents/skills/tx-xtask/SKILL.md
git commit -m "feat(xtask): run ext4 Tier 1 acceptance as one lifecycle"
```

### Task 16: Close G0-G7 with Fresh Product Evidence

**Files:**
- Modify: `tools/ext4/tier1/capability-ledger.json`
- Modify: `tools/ext4/tier1/xfstests-selection.json`
- Modify: `tools/ext4/tier1/crash-cuts.json`
- Create: `docs/progress/research/2026-07-30-ext4-tier1-acceptance.md`
- Modify: `docs/progress/plans/2026-07-30-ext4-tier1-lifecycle-convergence.json`
- Modify: `docs/progress/STATUS.md`
- Generated, not committed: `target/ext4/tier1/<run-id>/acceptance-receipt.json`

- [ ] **Step 1: Run the fast preflight gates**

```sh
cargo fmt --check
cargo -q xtask unit
cargo xtask lint arch
cargo xtask lint unused
cargo xtask lint docs
cargo xtask lint invariants ext4-lifecycle-ownership
cargo xtask lint invariants ext4-no-direct-home-write
cargo xtask lint invariants ext4-durability-flags
cargo xtask progress validate
```

Expected: all commands pass. Existing docs warnings may remain warnings only
when their count and exact text match the recorded baseline.

- [ ] **Step 2: Run focused G1-G3 host evidence**

```sh
cargo test -p tx-subsystems --lib owned_request_ -- --test-threads=1
cargo test -p tx-ext4-format
cargo test -p tx-ext4 --lib --no-default-features
cargo test -p tx-ext4 --test mutation_lifecycle -- --test-threads=1
cargo test -p tx-ext4 --test journal_transaction_plan -- --test-threads=1
```

Expected: lifecycle failure matrix, format/checksum fixtures, and recorded
device ordering all pass.

- [ ] **Step 3: Run the complete Tier 1 campaign**

```sh
cargo xtask ext4 tier1 --run-id 2026-07-30-tier1-candidate
```

The command must execute all 1000 deterministic crash cuts. Every immutable
post-cut image runs `e2fsck -fn`. The guest matrix covers data, setattr,
namespace, orphan, durability, detach, remount, and execute-from-SCRATCH. The
pinned Tier 1 xfstests set has no timeout, skip, crash, or not-run result.

- [ ] **Step 4: Validate the immutable receipt**

```sh
jq -e '
  .schema == "tx.ext4.tier1_acceptance_receipt.v1" and
  (.candidate.commit | type) == "string" and (.candidate.commit | length) == 40 and
  (.authorities.capability_ledger_sha256 | test("^[0-9a-f]{64}$")) and
  (.authorities.crash_cut_catalog_sha256 | test("^[0-9a-f]{64}$")) and
  (.authorities.xfstests_selection_sha256 | test("^[0-9a-f]{64}$")) and
  (.authorities.shell_scenario_sha256 | test("^[0-9a-f]{64}$")) and
  (.role_images.test.sha256 != .role_images.scratch.sha256) and
  (.role_images.test.sha256 != .role_images.workload.sha256) and
  (.role_images.scratch.sha256 != .role_images.workload.sha256) and
  (.e2fsck.immutable_images | length) == .crash_cuts.completed and
  ([.e2fsck.immutable_images[].exit_code] | all(. == 0)) and
  ([.e2fsck.immutable_images[].image_sha256] | all(test("^[0-9a-f]{64}$"))) and
  .gates.G0 == "passed" and .gates.G1 == "passed" and
  .gates.G2 == "passed" and .gates.G3 == "passed" and
  .gates.G4 == "passed" and .gates.G5 == "passed" and
  .gates.G6 == "passed" and .gates.G7 == "passed" and
  .crash_cuts.completed == 1000 and
  .e2fsck.failures == 0 and
  .xfstests.skipped == 0 and .xfstests.not_run == 0
' target/ext4/tier1/2026-07-30-tier1-candidate/acceptance-receipt.json
```

Expected: `jq` exits 0. A missing field or partial campaign keeps the plan
active and records the first blocker; it never promotes product status.

- [ ] **Step 5: Close progress and commit evidence pointers**

Update the acceptance research note with candidate commit, receipt path and
hash, command results, next Tier 2 entry point, and blockers. Change every JSON
step to `complete`, the plan to `complete`, and verification rows to `passed`.

```sh
cargo xtask progress validate
cargo xtask lint docs
git diff --check
git add tools/ext4/tier1 docs/progress/research/2026-07-30-ext4-tier1-acceptance.md docs/progress/plans/2026-07-30-ext4-tier1-lifecycle-convergence.json docs/progress/STATUS.md
git commit -m "test(ext4): promote verified Tier 1 lifecycle"
```

## Task Dependency Summary

```text
1 reconcile
  -> 2 fail-closed capability gate
     -> 3 OwnedFileIoRequest
        -> 4 PageBacked migration
           -> 5 MutationHandle
              -> 6 failure lifecycle
                 -> 7 FUA/flush
                    -> 8 recovery/cache
                       -> 9 revoke/deferred-free
                          -> 10 setattr
                             -> 11 data/truncate
                                -> 12 namespace/orphan
                                   -> 13 MountSettlementOp/syscalls
                                      -> 14 production cutover/lints
                                         -> 15 one Tier 1 runner
                                            -> 16 G0-G7 promotion
```

No task may be marked complete from type existence, host DTO tests, reused
serial logs, or a fail-closed `EOPNOTSUPP` result. Completion means the stated
production callsites migrated and the task's named behavioral evidence passed.
