# I/O SubmissionManager And File-Data Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:subagent-driven-development` (recommended) or
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking. Use TDD for every behavior change,
> preserve unrelated dirty-worktree changes, and do not run the full ext4
> crash campaign before Task 17.

**Goal:** replace the executable but coarse filesystem-I/O staging path with
independent L4/L6 submission owners, lock-independent resident lookup, real
multi-page reads and readahead, and candidate-bound correctness/performance
receipts.

**Architecture:** `PageContainer` retains file-data and PageSlot authority but
holds only `ResidentDomain`, `PageStateDomain`, `RangeDomain`, and typed manager
handles. One L4 manager owns admitted page requests and terminal aggregation;
one L6 manager per physical device owns limits, queues, tags, barriers, and
completion. `Published<ResidentRoot>` accelerates resident hits only; all other
semantic state stays mutable under its owner.

**Tech Stack:** Rust 2024 `no_std`, `tx-substrate` epoch/publication and page
allocator tokens, `tx-subsystems` PageBacked/VFS/device/I/O manager,
`tx-ext4` JBD2 planning and settlement, bdev-fs, `tx-observe`, RV64 QEMU SMP4,
Python receipt tooling, `cargo xtask`, e2fsprogs, and pinned ext4 Tier 1.

---

## Authoritative Inputs

- Approved design:
  `docs/superpowers/specs/2026-08-05-io-submission-manager-performance-design.md`.
- Active I/O owner contract:
  `docs/design/05_filesystem/IO_MANAGER_v1.md`.
- Active cross-layer memory/I/O contract:
  `docs/design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md`.
- PageBacked and device contracts:
  `docs/design/03_memory-vm/PAGE_BACKED_v1.md` and
  `docs/design/06_devices/DEVICE.md`.
- Ext4 durability contract:
  `docs/design/05_filesystem/EXT4_LIFECYCLE_v1.md`.
- Tier 1 acceptance baseline:
  `docs/progress/research/2026-07-30-ext4-tier1-acceptance.md`.

Active `docs/design/` contracts remain authoritative. Task 1 moves the
approved concrete protocol into those docs before code behavior changes.

## File Map

- `crates/tx-substrate/src/epoch/{bag,domain,mod}.rs`: bounded retire-slot
  reservation, same-CPU reservation ownership, and cancellation.
- `crates/tx-substrate/src/publication/mod.rs` and
  `crates/tx-substrate/tests/publication.rs`: strengthened prepare/commit
  contract and forced-backpressure tests.
- `crates/tx-subsystems/src/page_backed/mod.rs`: PageContainer facade and
  temporary migration callsites; shrink this file as owner modules land.
- `crates/tx-subsystems/src/page_backed/resident.rs`: persistent root,
  `ResidentBinding`, install/withdraw writer protocol, and guard-scoped lookup.
- `crates/tx-subsystems/src/page_backed/state_domain.rs`: stable PageSlot index
  and publication/withdrawal transition claims.
- `crates/tx-subsystems/src/page_backed/range_domain.rs`: independent logical
  reservation owner.
- `crates/tx-subsystems/src/page_backed/lifecycle.rs`: direction-neutral
  multi-page `PageDataLease`, PageBacked resource bundle, and settlement.
- `crates/tx-subsystems/src/io_manager/page/{manager,readahead}.rs`: neutral
  generic L4 custody, service ownership, stream state, and optional work.
- `crates/tx-subsystems/src/io_manager/page/{mod,service,completion}.rs`: reuse
  existing request, graph, scheduling, and completion values through re-exports.
- `crates/tx-subsystems/src/io_manager/block/manager.rs`: generic L6 custody,
  pre-enqueue split, merge, depth, tags, barriers, and completion.
- `crates/tx-subsystems/src/io_manager/block/mod.rs`: existing `BioPlan`,
  `BlockQueue`, tags, and compatibility re-exports.
- `crates/tx-subsystems/src/io_manager/runtime/observe.rs`: bounded counters,
  histograms, and `IoSubmissionSnapshot`.
- `crates/tx-subsystems/src/fs_iface/plan.rs`: extend existing lease
  projections, completion list, and `BackendBioGraph`; add no parallel graph.
- `crates/tx-subsystems/src/device.rs`: `BlockIoLimits`, device-scoped manager
  registry, and exactly-once service runtime registration.
- `crates/tx-kernel/src/init.rs` and `init/tests.rs`: claim and submit one
  manager service runtime rather than one mutable runtime per PageContainer.
- `crates/tx-ext4/src/{mutation_lifecycle,journal,pager,namespace,planner}.rs`:
  C0 frontier settlement, range mapping, holes, and multi-page graph lowering.
- `crates/tx-ext4/tests/mutation_lifecycle.rs` and `src/tests_v3.rs`: frontier,
  fsync, multi-page, and failure-path tests.
- `crates/tx-fs/src/bdevfs/mod.rs`: page-range-to-LBA graph lowering.
- `crates/tx-subsystems/src/vfs/structure.rs`: stable per-open-file
  `ReadaheadStreamId`.
- `crates/tx-shims/src/linux_syscall/io.rs` and
  `tests/high_stakes_syscalls.rs`: real `readahead(2)` admission.
- `schema/txobserve.toml` plus generated observation artifacts: I/O request,
  block dispatch, resident retry, and readahead event families.
- `tools/shell-tests/io-submission-witness.c`: deterministic hot, sequential,
  random, concurrent, fsync, and raw-block guest modes.
- `tools/io-submission-perf.py` and
  `tools/tests/test_io_submission_perf.py`: artifact binding, matched A/B,
  threshold evaluation, and immutable receipt verification.
- `tools/io-submission-baseline.json` and
  `tools/io-submission-candidate.json`: fixed matched geometry and explicit
  baseline/candidate feature selections. A conditional
  `tools/io-submission-multiqueue.json` is created only after the P9 trigger.
- `xtask/src/test.rs`: existing `cargo xtask test` lane for the guest witness.
- `docs/progress/research/2026-08-05-io-submission-performance-baseline.md`:
  fixed geometry and baseline/candidate evidence.
- `docs/progress/plans/2026-08-05-io-submission-manager-performance.json` and
  `docs/progress/STATUS.md`: durable task state and blockers.

## Fixed Contracts Used By Every Task

```rust
pub const DEFAULT_PAGE_BATCH_PAGES: u32 = 16;
pub const MAX_PAGE_BATCH_PAGES: u32 = 64;
pub const DEFAULT_READAHEAD_PAGES: u32 = 4;
pub const MAX_READAHEAD_PAGES: u32 = 64;
pub const DEMAND_PLUG_MAX_BIOS: u16 = 8;
pub const BACKGROUND_PLUG_MAX_BIOS: u16 = 32;
pub const IO_HISTOGRAM_BUCKETS: usize = 32;
```

These ownership results are invariant:

```text
L4 reject  -> PageBacked receives the entire OwnedPageIoSubmission
L4 accept  -> L4 owns all resources until one OwnedPageIoSettlement
L6 reject  -> graph executor receives the entire OwnedBioSubmission
L6 accept  -> L6 returns one node completion to the graph executor
L6 complete -> never mutates PageSlot and never releases PageDataLease
```

No lock, epoch guard, writer claim, or DMA/map pin borrow crosses a yield,
filesystem planner call, device call, or wait. The required resident reader
order is root lookup, active/generation observation, owned `MapPin` acquisition,
then active/generation/PPN revalidation.

## Task 1: Align The Canonical I/O Contracts

**Files:**

- Modify: `docs/design/05_filesystem/IO_MANAGER_v1.md`
- Modify: `docs/design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md`
- Modify: `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- Modify: `docs/design/06_devices/DEVICE.md`
- Modify: `docs/design/05_filesystem/EXT4_LIFECYCLE_v1.md`
- Test: `docs/superpowers/specs/2026-08-05-io-submission-manager-performance-design.md`

- [ ] **Step 1: Add grep-stable protocol tags**

Add these exact tags to their owning sections:

```markdown
<!-- txdoc:IO-SUBMISSION-ATOMIC-ADMISSION-1 -->
<!-- txdoc:IO-SUBMISSION-EXACT-SETTLEMENT-1 -->
<!-- txdoc:IO-SUBMISSION-BLOCK-LIMITS-1 -->
<!-- txdoc:PAGEBACKED-RESIDENT-PUBLICATION-1 -->
<!-- txdoc:PAGEBACKED-PUBLICATION-CLAIMS-1 -->
<!-- txdoc:DEVICE-IO-LIMITS-1 -->
<!-- txdoc:EXT4-MOUNT-FRONTIER-1 -->
<!-- txdoc:MEMORY-IO-PERF-RECEIPT-1 -->
```

- [ ] **Step 2: Pin owner and failure semantics**

Copy the approved Canonical Gate decisions into the active owner sections.
State explicitly that admission rejection returns custody, successful L4
admission yields exactly one terminal settlement, L6 completion cannot mutate
PageSlot, and `Publishing`/`Withdrawing` are exclusive internal PageSlot
transition claims rather than new semantic authorities.

- [ ] **Step 3: Pin device-limit and publication semantics**

Document finite conservative defaults for every unknown `BlockIoLimits` field,
split-before-enqueue, merge-within-limits, and one physical-device manager.
Strengthen the publication contract so prepare reserves retirement capacity
and successful commit performs no drain, allocation, wait, or failure.

- [ ] **Step 4: Pin evidence and crash-run policy**

Document the receipt schema/path, fixed geometry, required attribution,
five-pair A/B rule, conditional multi-queue thresholds, one fresh final Tier 1
campaign, and failed-cut resume policy.

- [ ] **Step 5: Verify active documentation**

Run:

```bash
cargo xtask lint docs
rg -n "IO-SUBMISSION-ATOMIC-ADMISSION-1|PAGEBACKED-RESIDENT-PUBLICATION-1|DEVICE-IO-LIMITS-1|EXT4-MOUNT-FRONTIER-1|MEMORY-IO-PERF-RECEIPT-1" docs/design
git diff --check -- docs/design
```

Expected: docs lint passes; every listed tag appears exactly once; diff check
prints nothing.

- [ ] **Step 6: Commit the canonical contract alignment**

```bash
git add docs/design/05_filesystem/IO_MANAGER_v1.md \
  docs/design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md \
  docs/design/03_memory-vm/PAGE_BACKED_v1.md \
  docs/design/06_devices/DEVICE.md \
  docs/design/05_filesystem/EXT4_LIFECYCLE_v1.md
git commit -m "docs: align I/O submission ownership contracts"
```

## Task 2: Close The Ext4 Fsync And Mount-Frontier Prerequisite

**Files:**

- Modify: `crates/tx-ext4/src/mutation_lifecycle.rs:81-280`
- Modify: `crates/tx-ext4/src/journal.rs:1550-1557`
- Modify: `crates/tx-ext4/src/pager.rs:296-305`
- Modify: `crates/tx-ext4/src/namespace.rs:789-808`
- Modify: `crates/tx-ext4/src/settlement.rs:20-82`
- Test: `crates/tx-ext4/tests/mutation_lifecycle.rs`
- Test: `crates/tx-ext4/src/tests_v3.rs`

- [ ] **Step 1: Write failing persistent-frontier tests**

Extend the existing sequence-7 fixture with these assertions:

```rust
#[test]
fn runtime_frontier_survives_terminal_checkpoint() {
    let (runtime, request) = active_runtime_with_sequence_7();
    complete_data_commit_and_checkpoint(&runtime, request);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        MountTransactionFrontier::new(7)
    );
    assert!(runtime.has_settled(MountTransactionFrontier::new(7)));
}

#[test]
fn later_admission_advances_frontier_monotonically() {
    let runtime = settled_runtime_with_sequence_7();
    admit_and_settle_sequence(&runtime, 8);
    assert_eq!(
        runtime.snapshot_transaction_frontier(),
        MountTransactionFrontier::new(8)
    );
    assert!(runtime.has_settled(MountTransactionFrontier::new(8)));
}
```

Implement the three named fixture helpers in the same test file by composing
the existing `ring()`, `mutation()`, `data_request()`, `plan_data`,
`complete_data`, `plan_fsync`, `complete_fsync`, `take_checkpoint_graph`, and
`complete_checkpoint_result` calls. They must not bypass runtime state.

- [ ] **Step 2: Run the frontier tests to verify RED**

Run:

```bash
cargo test -p tx-ext4 --test mutation_lifecycle runtime_frontier_ -- --nocapture
cargo test -p tx-ext4 --test mutation_lifecycle later_admission_ -- --nocapture
```

Expected: FAIL because terminal release resets the observed frontier and
`has_settled` does not exist.

- [ ] **Step 3: Persist admitted and settled frontiers**

Add monotonic state under `JournalFsyncSourceState`:

```rust
#[derive(Debug)]
struct JournalFsyncSourceState {
    mutation: Option<MutationHandle>,
    checkpoint_submitted: bool,
    recovery_only: bool,
    mount_error: Option<Errno>,
    settlement_observer: Option<Weak<dyn JournalSettlementObserver>>,
    admitted_frontier: MountTransactionFrontier,
    settled_frontier: MountTransactionFrontier,
}

impl JournalFsyncSource {
    pub fn active_transaction_frontier(&self) -> MountTransactionFrontier {
        self.state.lock().admitted_frontier
    }

    pub fn has_settled(&self, frontier: MountTransactionFrontier) -> bool {
        self.state.lock().settled_frontier.raw() >= frontier.raw()
    }
}
```

In `begin_handle`, reject sequence regression and advance
`admitted_frontier`. Advance `settled_frontier` only after the durable commit
and checkpoint/tail-reclaim path has completed successfully. An ambiguous
commit never advances it and preserves recovery-only state.

- [ ] **Step 4: Write failing production fsync/settle tests**

Add tests using the mutation-journal mount helper:

```rust
#[test]
fn mutation_journal_fsync_does_not_return_enosys() {
    let mounted = open_mutation_journal_fs();
    stage_one_buffered_page(&mounted, 12, 0);
    let guard = epoch::guard();
    assert_eq!(
        <Ext4FsInstance<MemImage> as FsPageBacking>::fsync_file(
            &mounted.backend,
            FsObjectId::new(12),
            &guard,
        ),
        V3::<(), NoProgress>::done(())
    );
}

#[test]
fn mount_settlement_rejects_an_unreached_frontier() {
    let mounted = open_mutation_journal_fs();
    let guard = epoch::guard();
    assert_eq!(
        mounted.backend.settle_mount(MountTransactionFrontier::new(99), &guard),
        V3::<(), NoProgress>::err(V3Errno::EAGAIN)
    );
}
```

Create `open_mutation_journal_fs` from the existing in-memory registered block
device and discovered-journal fixtures; `stage_one_buffered_page` must call the
real `prepare_write_range` and `flush_page` path.

- [ ] **Step 5: Run the fsync tests to verify RED**

```bash
cargo test -p tx-ext4 mutation_journal_fsync_does_not_return_enosys -- --nocapture
cargo test -p tx-ext4 mount_settlement_rejects_an_unreached_frontier -- --nocapture
```

Expected: first FAILS with `ENOSYS`; second FAILS because current
`settle_mount` ignores the frontier.

- [ ] **Step 6: Route ext4 settlement through the journal runtime**

Change production behavior to:

```rust
fn fsync_file(
    &self,
    _fs_object_id: FsObjectId,
    _guard: &Guard<'_>,
) -> StepOutcome<(), NoProgress> {
    if let Some(runtime) = self.metadata_mutation_runtime() {
        return match self.settle_metadata_mutation(&runtime) {
            Ok(()) => StepOutcome::done(()),
            Err(errno) => StepOutcome::err(errno.into()),
        };
    }
    if self.legacy_writeback_enabled() {
        StepOutcome::done(())
    } else {
        StepOutcome::err(Errno::EOPNOTSUPP.into())
    }
}
```

Make `settle_metadata_mutation` return success when no active mutation exists
and no mount error is pending. Implement `snapshot_mount_transaction_frontier`
in ext4 `FsOps` by calling the runtime snapshot. Implement `settle_mount` by
settling an active mutation, checking `has_settled(frontier)`, and returning
`EAGAIN` when the requested nonzero frontier is still unreachable.

- [ ] **Step 7: Run focused and full ext4 host tests**

```bash
cargo test -p tx-ext4 --test mutation_lifecycle -- --nocapture
cargo test -p tx-ext4 --lib -- --nocapture
cargo test -p tx-subsystems mount_settlement -- --nocapture
cargo -q xtask unit
```

Expected: all pass; production mutation-journal fsync no longer returns
`ENOSYS`; existing error/commit-unknown tests still retain the journal extent.

- [ ] **Step 8: Verify the existing Tier 1 baseline receipt only**

```bash
cargo xtask ext4 tier1 --verify-receipt target/ext4/tier1/task16-live-retry-20260804/acceptance-receipt.json
```

Expected: receipt integrity passes. Record explicitly that this verifies the
baseline artifact, not the new candidate. Do not start a fresh crash campaign.

- [ ] **Step 9: Commit C0**

```bash
git add crates/tx-ext4/src/mutation_lifecycle.rs \
  crates/tx-ext4/src/journal.rs crates/tx-ext4/src/pager.rs \
  crates/tx-ext4/src/namespace.rs crates/tx-ext4/src/settlement.rs \
  crates/tx-ext4/tests/mutation_lifecycle.rs crates/tx-ext4/src/tests_v3.rs
git commit -m "fix(ext4): persist and settle transaction frontiers"
```

## Task 3: Add Bounded I/O Observation Primitives

**Files:**

- Create: `crates/tx-subsystems/src/io_manager/runtime/observe.rs`
- Modify: `crates/tx-subsystems/src/io_manager/runtime/mod.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/service.rs`
- Modify: `crates/tx-subsystems/src/io_manager/block/mod.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `schema/txobserve.toml`
- Test: `crates/tx-subsystems/src/io_manager/runtime/observe.rs`
- Test: generated observation schema checks

- [ ] **Step 1: Write failing snapshot and histogram tests**

```rust
#[test]
fn snapshot_separates_queue_delay_from_service_time() {
    let counters = IoSubmissionCounters::new();
    counters.record_queue_delay_ns(8_000);
    counters.record_service_time_ns(64_000);
    let snapshot = counters.snapshot();
    assert_eq!(snapshot.queue_delay_ns.total, 8_000);
    assert_eq!(snapshot.service_time_ns.total, 64_000);
    assert_eq!(snapshot.trace_lost, 0);
}

#[test]
fn histogram_bucket_saturates_without_allocation() {
    let histogram = IoHistogram::new();
    histogram.record(u64::MAX);
    assert_eq!(histogram.snapshot().buckets[IO_HISTOGRAM_BUCKETS - 1], 1);
}
```

- [ ] **Step 2: Run observation tests to verify RED**

```bash
cargo test -p tx-subsystems --lib io_submission_observe -- --nocapture
```

Expected: FAIL because `observe` module and types do not exist.

- [ ] **Step 3: Implement fixed-size counters and histograms**

Use atomics only on record paths:

```rust
pub const IO_HISTOGRAM_BUCKETS: usize = 32;

pub struct IoHistogram {
    buckets: [AtomicU64; IO_HISTOGRAM_BUCKETS],
    total: AtomicU64,
    samples: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoHistogramSnapshot {
    pub buckets: [u64; IO_HISTOGRAM_BUCKETS],
    pub total: u64,
    pub samples: u64,
}

pub struct IoSubmissionCounters {
    admission_accepted: AtomicU64,
    admission_rejected: AtomicU64,
    split_pieces: AtomicU64,
    merge_success: AtomicU64,
    payload_extra_copy_bytes: AtomicU64,
    bounce_bytes: AtomicU64,
    readahead_requested: AtomicU64,
    readahead_useful: AtomicU64,
    trace_emitted: AtomicU64,
    trace_lost: AtomicU64,
    queue_delay_ns: IoHistogram,
    service_time_ns: IoHistogram,
    request_bytes: IoHistogram,
    sg_segments: IoHistogram,
}
```

Add the remaining design-required counters as explicit atomics: rejection,
merge/split/bounce/plug reasons, queue/tag occupancy, retry/timeout/error,
resident retry/publication pressure, readahead admitted/completed/unused/
canceled/duplicate, unique demand bytes, device bytes, writeback bytes,
journal bytes, and checkpoint bytes. `snapshot()` loads every field with
Acquire and returns an immutable `IoSubmissionSnapshot`.

- [ ] **Step 4: Instrument the current staging path without changing policy**

Record admission, queue, service, SG, merge, completion, copy/bounce, and
resident lock facts at existing L4/L6/PageBacked boundaries. Capture queue
entry and dispatch timestamps separately. Do not allocate, format strings, or
emit one trace record per page unless the bounded trace-window flag is active.

- [ ] **Step 5: Add stable observation schema families**

Add payloads `io_request`, `io_block`, `io_resident`, and `io_readahead`, each
at most 16 bytes. Add canonical event names:

```text
io.page.admit
io.page.terminal
io.block.dispatch
io.block.complete
io.resident.retry
io.readahead.window
```

Regenerate through the repository command; do not edit generated Rust/JSON by
hand.

- [ ] **Step 6: Verify observation code and schema**

```bash
cargo test -p tx-subsystems --lib io_submission_observe -- --nocapture
cargo xtask observe-schema codegen
cargo xtask observe-schema codegen --check
cargo xtask observe-schema check
cargo xtask observe-discipline
```

Expected: tests pass; generated artifacts match schema; discipline reports no
allocating or unbounded hot-path probe.

- [ ] **Step 7: Commit bounded observation**

```bash
git add crates/tx-subsystems/src/io_manager/runtime \
  crates/tx-subsystems/src/io_manager/page/service.rs \
  crates/tx-subsystems/src/io_manager/block/mod.rs \
  crates/tx-subsystems/src/page_backed/mod.rs schema/txobserve.toml \
  crates/tx-observe/src/l0_schema/schema_catalog.rs \
  tools/tx-observe-host-catalog.json
git commit -m "feat(io): add bounded submission metrics"
```

## Task 4: Capture The Locked Single-Page Baseline

**Files:**

- Create: `tools/shell-tests/io-submission-witness.c`
- Create: `tools/io-submission-perf.py`
- Create: `tools/tests/test_io_submission_perf.py`
- Create: `tools/io-submission-baseline.json`
- Modify: `xtask/src/test.rs`
- Create: `docs/progress/research/2026-08-05-io-submission-performance-baseline.md`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Write failing receipt-verifier tests**

```python
class IoSubmissionReceiptTests(unittest.TestCase):
    def test_rejects_unknown_trace_loss(self):
        receipt = valid_receipt()
        receipt["metrics"]["trace_lost"] = None
        self.assertEqual(evaluate(receipt)["status"], "failed")

    def test_rejects_geometry_mismatch(self):
        receipt = valid_receipt()
        receipt["candidate"]["geometry_sha256"] = "f" * 64
        self.assertEqual(evaluate(receipt)["status"], "failed")

    def test_accepts_five_interleaved_pairs(self):
        receipt = valid_receipt(pair_count=5)
        self.assertEqual(evaluate(receipt)["status"], "passed")
```

`valid_receipt` must construct a complete
`tx.io_submission.performance_receipt.v1` object with source, dirty-state,
toolchain, geometry, image/workload digests, baseline/candidate runs, metrics,
trace artifacts, and SHA-256 bindings.

- [ ] **Step 2: Run verifier tests to verify RED**

```bash
python3 -m unittest tools.tests.test_io_submission_perf
```

Expected: FAIL because the module does not exist.

- [ ] **Step 3: Implement receipt collection and verification**

Provide these exact subcommands:

```text
python3 tools/io-submission-perf.py collect --config CONFIG --out RUN_DIR
python3 tools/io-submission-perf.py evaluate --baseline BASE --candidate CANDIDATE --out RECEIPT
python3 tools/io-submission-perf.py verify --receipt RECEIPT
python3 tools/io-submission-perf.py multi-queue-trigger --receipt RECEIPT
```

Write JSON through a temporary file, fsync it, rename once, then reject any
attempt to overwrite an existing `acceptance-receipt.json`. Hash every bound
artifact with SHA-256 and compare all geometry fields before evaluating
thresholds.

- [ ] **Step 4: Add deterministic guest witness modes**

The C witness accepts:

```text
io-submission-witness hot FILE BYTES ITERATIONS THREADS
io-submission-witness sequential FILE BYTES
io-submission-witness random FILE BYTES OPERATIONS SEED
io-submission-witness fsync FILE BYTES
io-submission-witness raw DEVICE BYTES
```

Use `pread`, `pthread`, `clock_gettime`, `fsync`, and deterministic xorshift64
offsets. Emit one final line:

```text
TX_IO_WITNESS mode=<mode> bytes=<n> ops=<n> elapsed_ns=<n> errors=<n>
```

No mode prints per-operation output.

- [ ] **Step 5: Add the existing xtask test lane**

Extend `cargo xtask test` with `io-submission-witness`, `--case`, `--smp`, and
`--append-cmdline` forwarding. Retain serial logs under
`target/io-submission/witness/<case>/serial.log` and require the final marker
with `errors=0`.

- [ ] **Step 6: Verify the harness before a long run**

```bash
python3 -m unittest tools.tests.test_io_submission_perf
cargo test -p xtask io_submission_witness -- --nocapture
cargo xtask test io-submission-witness --target rv64-qemu --case hot --smp 4 --timeout-ms 120000
```

Expected: verifier and xtask tests pass; the short guest run emits one valid
marker and retains its serial log.

- [ ] **Step 7: Capture and bind the P0 baseline**

Run the hot, sequential, random, raw, concurrent 1/2/4-hart, fsync, and fixed
clean-build workloads with the exact geometry from the approved design. Use
five repetitions for short workloads and record the current locked resident
root, one-page demand path, single queue, and readahead-disabled mode.

```bash
python3 tools/io-submission-perf.py collect \
  --config tools/io-submission-baseline.json \
  --out target/perf/io-submission/p0-locked-single-page
python3 tools/io-submission-perf.py verify \
  --receipt target/perf/io-submission/p0-locked-single-page/acceptance-receipt.json
```

Expected: artifact verification passes. Performance promotion fields are
`baseline-only`; this task does not claim a speedup.

- [ ] **Step 8: Record baseline and commit harness**

Write image digests, revision, dirty declaration, toolchain, QEMU geometry,
limits, workload commands, counters, trace loss, artifact path, and blockers in
the research note and STATUS.

```bash
git add tools/shell-tests/io-submission-witness.c \
  tools/io-submission-perf.py tools/tests/test_io_submission_perf.py \
  tools/io-submission-baseline.json \
  xtask/src/test.rs \
  docs/progress/research/2026-08-05-io-submission-performance-baseline.md \
  docs/progress/STATUS.md
git commit -m "test(io): capture submission performance baseline"
```

## Task 5: Introduce Neutral L4 Manager Custody

**Files:**

- Create: `crates/tx-subsystems/src/io_manager/page/manager.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/mod.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/admission.rs`
- Test: `crates/tx-subsystems/src/io_manager/page/manager.rs`

- [ ] **Step 1: Write failing atomic-admission tests**

```rust
#[test]
fn rejection_returns_the_complete_bundle() {
    let manager = PageIoSubmissionManager::with_capacity(1);
    manager.try_admit(submission(1, Resource(11))).unwrap();
    let rejected = manager.try_admit(submission(2, Resource(22))).unwrap_err();
    assert_eq!(rejected.submission.resources, Resource(22));
    assert_eq!(manager.snapshot().admitted, 1);
}

#[test]
fn accepted_request_yields_one_terminal_settlement() {
    let manager = PageIoSubmissionManager::with_capacity(2);
    manager.try_admit(submission(1, Resource(11))).unwrap();
    let mut service = manager.try_claim_service().unwrap();
    service.finish_for_test(PageIoRequestId::new(1), page_results());
    assert_eq!(service.pop_settlement().unwrap().resources, Resource(11));
    assert!(service.pop_settlement().is_none());
}
```

- [ ] **Step 2: Run manager tests to verify RED**

```bash
cargo test -p tx-subsystems --lib page_submission_manager -- --nocapture
```

Expected: FAIL because the manager types do not exist.

- [ ] **Step 3: Implement generic owned admission values**

Add neutral generic values so `io_manager` never imports PageBacked:

```rust
pub struct OwnedPageIoSubmission<R> {
    pub request: PageIoRequest,
    pub backend: BackendPageRequest,
    pub resources: R,
}

pub struct PageIoAdmissionRejection<R> {
    pub cause: PageQueueError,
    pub submission: OwnedPageIoSubmission<R>,
}

pub struct OwnedPageIoSettlement<R> {
    pub request: PageIoRequest,
    pub completions: PageCompletionList,
    pub resources: R,
}
```

`R` is move-only manager custody. It has no callback requirement and is never
inspected by the neutral manager.

- [ ] **Step 4: Implement one claimed service owner**

Use one private state lock and an atomic service claim:

```rust
pub struct PageIoSubmissionManager<R> {
    state: SpinMutex<PageIoManagerState<R>>,
    service_claimed: AtomicBool,
    counters: Arc<IoSubmissionCounters>,
}

#[derive(Clone)]
pub struct PageIoSubmissionHandle<R> {
    manager: Arc<PageIoSubmissionManager<R>>,
}

pub struct PageIoSubmissionService<R> {
    manager: Arc<PageIoSubmissionManager<R>>,
}
```

`try_admit` reserves queue, active-map, graph, waiter, and settlement capacity
under one state lock before inserting anything. `try_claim_service` uses
`compare_exchange(false, true, AcqRel, Acquire)` and returns `None` after the
first claim. Service Drop is forbidden in production; test-only Drop releases
the claim.

- [ ] **Step 5: Model success, error, cancellation, and duplicate terminal**

Add tests for planner error, graph error, cancel-before-dispatch, timeout, and
duplicate completion. Every accepted request produces one settlement and all
duplicate terminal events increment an invariant counter without releasing the
resource twice.

- [ ] **Step 6: Run L4 unit tests and neutral dependency scan**

```bash
cargo test -p tx-subsystems --lib page_submission_manager -- --nocapture
cargo test -p tx-subsystems --lib io_manager -- --nocapture
rg -n "page_backed|tx_ext4|tx-ext4|tx_fs|tx-fs|BlockDeviceOps" \
  crates/tx-subsystems/src/io_manager/page/manager.rs
```

Expected: tests pass; dependency scan prints no imports or concrete-owner
references.

- [ ] **Step 7: Commit neutral L4 custody**

```bash
git add crates/tx-subsystems/src/io_manager/page
git commit -m "feat(io): add owned L4 submission manager"
```

## Task 6: Cut Production Page I/O Over To The L4 Manager

**Files:**

- Modify: `crates/tx-subsystems/src/page_backed/lifecycle.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs:760-2725`
- Modify: `crates/tx-subsystems/src/device.rs:671-1050`
- Modify: `crates/tx-kernel/src/init.rs:100-120,770-825`
- Modify: `crates/tx-kernel/src/init/tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/lifecycle_tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Write failing PageBacked ownership tests**

```rust
#[test]
fn page_container_rejection_restores_fetch_and_target_frame() {
    let (pc, manager) = file_pc_with_manager_capacity(0);
    let before = page_allocator_snapshot();
    assert_eq!(pc.admit_file_read_for_test(PageIndex::new(3)), Err(Errno::EAGAIN));
    assert_eq!(pc.file_page_slot_snapshot_for_test(PageIndex::new(3)).unwrap().state,
        PageSlotState::Empty);
    assert_eq!(page_allocator_snapshot(), before);
    assert_eq!(manager.snapshot().admitted, 0);
}

#[test]
fn page_container_consumes_one_manager_settlement() {
    let (pc, manager) = file_pc_with_manager_capacity(4);
    let id = pc.admit_file_read_for_test(PageIndex::new(3)).unwrap();
    manager.complete_read_for_test(id, PageIoResult::Done);
    pc.drive_terminal_settlements_for_test();
    assert!(pc.materialize_cached_page_for_test(PageIndex::new(3)).is_some());
    assert_eq!(pc.owned_file_request_count_for_test(), 0);
}
```

- [ ] **Step 2: Run PageBacked tests to verify RED**

```bash
cargo test -p tx-subsystems --lib page_container_rejection_restores -- --nocapture
cargo test -p tx-subsystems --lib page_container_consumes_one -- --nocapture
```

Expected: FAIL because PageContainer still owns `PageService` and request maps.

- [ ] **Step 3: Replace `OwnedFileIoRequest` with a private resource bundle**

Keep concrete custody in PageBacked:

```rust
pub(super) struct PageBackedIoResources {
    owner: Cap<PageContainer>,
    payload: FileIoPayload,
}

type FilePageIoManager = PageIoSubmissionManager<PageBackedIoResources>;
type FilePageIoHandle = PageIoSubmissionHandle<PageBackedIoResources>;
type FilePageIoSettlement = OwnedPageIoSettlement<PageBackedIoResources>;
```

Build `OwnedPageIoSubmission` only after PageSlot and frame preparation.
Rejection calls one PageBacked rollback function. Settlement consumption calls
one PageBacked generation-validation function. Remove the independent
`owned_file_requests` insertion/removal path.

- [ ] **Step 4: Move page service, graph, waiter, fetch, and direct-I/O custody**

Move `PageService`, active graphs, in-flight file-page fetch records, page
waiters, and direct-I/O admitted/completed request custody behind
`PageIoSubmissionManager`. Keep PageSlot and range transitions in PageBacked.
The manager stores owner keys/resources, never a borrowed PageContainer lock.

- [ ] **Step 5: Register one shared L4 service runtime**

Keep the existing kernel inversion boundary but change registry semantics:

```rust
pub fn register_page_container_file_io_service(
    container: Cap<PageContainer>,
    block: BlockDeviceHandle,
) -> PageIoSubmissionHandle<PageBackedIoResources>;
```

The first registration initializes and claims the shared L4 runtime. Later
registrations bind their PageContainer and block handle to the same manager and
do not create another service owner. Registry locks are released before calling
the kernel spawner. Add tests proving N PageContainers cause one L4 task
submission and N owner bindings.

- [ ] **Step 6: Update the kernel spawner**

Change `CoreInit::submit_file_io_runtime_task_with` to submit the claimed L4
service future once. Preserve current reactor affinity and wake-source behavior.
The future drains bounded completions before requests, rechecks queues before
sleep, and retains no epoch guard across `.await`.

- [ ] **Step 7: Remove embedded L4 fields from `PageContainerState`**

After all callsites use the handle, delete `file_io_service`,
`owned_file_requests`, `background_graphs`, `in_flight_file_pages`,
`file_page_waits`, and direct-I/O request maps from the coarse state. Temporary
block runtime, resident index, slots, and range table remain until later tasks.

- [ ] **Step 8: Run L4 parity and service-registration tests**

```bash
cargo test -p tx-subsystems --lib page_backed -- --nocapture --test-threads=1
cargo test -p tx-subsystems --lib io_manager -- --nocapture --test-threads=1
cargo test -p tx-subsystems --lib file_io_service_runtime -- --nocapture --test-threads=1
cargo test -p tx-kernel-riscv64-qemu-virt file_io_runtime -- --nocapture --test-threads=1
cargo check -p tx-subsystems -p tx-kernel-riscv64-qemu-virt
```

Expected: all pass; exactly-one runtime tests report one L4 service owner; read,
writeback, fsync, direct-I/O scaffolding, planner error, and terminal error tests
retain prior behavior.

- [ ] **Step 9: Commit production L4 ownership**

```bash
git add crates/tx-subsystems/src/page_backed/lifecycle.rs \
  crates/tx-subsystems/src/page_backed/mod.rs \
  crates/tx-subsystems/src/device.rs crates/tx-kernel/src/init.rs \
  crates/tx-kernel/src/init/tests.rs
git commit -m "refactor(io): move page requests behind L4 manager"
```

## Task 7: Add Typed Device I/O Limits

**Files:**

- Modify: `crates/tx-subsystems/src/device.rs:319-530`
- Modify: block driver implementations returned by
  `rg -l "impl BlockDevice for" crates boards`
- Test: `crates/tx-subsystems/src/device.rs`

- [ ] **Step 1: Write failing validation/default tests**

```rust
#[test]
fn unknown_device_limits_are_finite_and_conservative() {
    let limits = BlockIoLimits::conservative(512, BlockDurabilityCapabilities::NONE);
    assert_eq!(limits.max_segments, 1);
    assert_eq!(limits.hardware_queues, 1);
    assert!(limits.max_transfer_bytes >= 512);
    assert!(limits.validate().is_ok());
}

#[test]
fn invalid_device_limits_fail_registration() {
    let limits = BlockIoLimits {
        logical_block_bytes: 4096,
        alignment_bytes: 4096,
        max_transfer_bytes: 2048,
        max_segments: 0,
        max_segment_bytes: 4096,
        segment_boundary_mask: u64::MAX,
        max_outstanding: 0,
        hardware_queues: 0,
        durability: BlockDurabilityCapabilities::NONE,
    };
    assert_eq!(limits.validate(), Err(BlockIoLimitsError::ZeroCapacity));
}
```

- [ ] **Step 2: Run device-limit tests to verify RED**

```bash
cargo test -p tx-subsystems --lib block_io_limits -- --nocapture
```

Expected: FAIL because `BlockIoLimits` does not exist.

- [ ] **Step 3: Implement the immutable capability snapshot**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockIoLimits {
    pub logical_block_bytes: u32,
    pub alignment_bytes: u32,
    pub max_transfer_bytes: u32,
    pub max_segments: u16,
    pub max_segment_bytes: u32,
    pub segment_boundary_mask: u64,
    pub max_outstanding: u16,
    pub hardware_queues: u16,
    pub durability: BlockDurabilityCapabilities,
}

pub trait BlockDevice: BlockDeviceOps {
    fn total_blocks(&self) -> u64;
    fn block_size(&self) -> u32;

    fn io_limits(&self) -> BlockIoLimits {
        BlockIoLimits::conservative(self.block_size(), self.durability_capabilities())
    }
}
```

Validation rejects zero, non-power-of-two logical/alignment sizes, transfer
smaller than one logical block, segment size smaller than alignment, and
overflowing block/byte conversions. Unknown fields use the conservative
constructor and never `u32::MAX`, `u64::MAX`, or zero as unlimited.

- [ ] **Step 4: Report negotiated limits in concrete drivers**

For each driver found by the discovery command, return negotiated transfer,
segment, depth, queue, and durability values when the hardware surface exposes
them. Keep the conservative default where it does not. Add unit tests for each
non-default driver report.

- [ ] **Step 5: Validate limits during static registration**

Make `register_block_devices` reject an invalid registration before publishing
any registry row. `BlockDeviceHandle::io_limits()` returns the immutable
validated snapshot and partition handles do not alter physical device limits.

- [ ] **Step 6: Run device and driver tests**

```bash
cargo test -p tx-subsystems --lib block_device_ -- --nocapture
cargo test -p tx-subsystems --lib block_io_limits -- --nocapture
cargo check -p tx-subsystems
cargo xtask check
```

Expected: all pass; every registered block device reports validated finite
limits.

- [ ] **Step 7: Commit device capabilities**

```bash
git add crates/tx-subsystems/src/device.rs crates boards
git commit -m "feat(device): expose bounded block I/O limits"
```

Before staging, replace `crates boards` with only the concrete driver files
reported by `git diff --name-only`; do not stage unrelated files.

## Task 8: Extract The Device-Scoped L6 Manager

**Files:**

- Create: `crates/tx-subsystems/src/io_manager/block/manager.rs`
- Modify: `crates/tx-subsystems/src/io_manager/block/mod.rs`
- Modify: `crates/tx-subsystems/src/device.rs:590-1118`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs:790-825,1980-2410`
- Modify: `crates/tx-subsystems/src/io_manager/page/service.rs`
- Test: `crates/tx-subsystems/src/io_manager/block/manager.rs`
- Test: `crates/tx-subsystems/src/device.rs`

- [ ] **Step 1: Write failing device-scope and ownership tests**

```rust
#[test]
fn partitions_share_one_physical_device_manager() {
    let whole = BlockDeviceHandle::whole(&BLOCK_REG);
    let part = BlockDeviceHandle::partition(&BLOCK_REG, 32, 64);
    assert!(whole.submission_handle().same_manager(&part.submission_handle()));
}

#[test]
fn l6_rejection_returns_the_complete_node() {
    let manager = BlockSubmissionManager::with_capacity(test_limits(), 1);
    manager.try_admit(owned_bio(1)).unwrap();
    let rejected = manager.try_admit(owned_bio(2)).unwrap_err();
    assert_eq!(rejected.submission.route.node.raw(), 2);
}

#[test]
fn l6_completion_never_settles_page_resources() {
    let manager = BlockSubmissionManager::with_capacity(test_limits(), 2);
    manager.try_admit(owned_bio(1)).unwrap();
    let completion = manager.complete_for_test(BlockTag::new(1), Ok(())).unwrap();
    assert_eq!(completion.route.node.raw(), 1);
    assert_eq!(PAGE_SETTLEMENT_COUNT.load(Ordering::Acquire), 0);
}
```

- [ ] **Step 2: Run L6 tests to verify RED**

```bash
cargo test -p tx-subsystems --lib block_submission_manager -- --nocapture
cargo test -p tx-subsystems --lib partitions_share_one -- --nocapture
```

Expected: FAIL because block runtime remains per PageContainer.

- [ ] **Step 3: Implement generic L6 custody**

```rust
pub struct OwnedBioSubmission<R> {
    pub bio: Bio,
    pub route: R,
}

pub struct BlockAdmissionRejection<R> {
    pub cause: QueueError,
    pub submission: OwnedBioSubmission<R>,
}

pub struct BlockNodeCompletion<R> {
    pub result: Result<(), Errno>,
    pub route: R,
}

pub struct BlockSubmissionManager<R> {
    limits: BlockIoLimits,
    state: SpinMutex<BlockManagerState<R>>,
    service_claimed: AtomicBool,
    counters: Arc<IoSubmissionCounters>,
}
```

Move `BlockQueue`, `QueueDepth`, `BlockTagTable`, graph-node tracker, direct-I/O
tracker, retry, timeout, and barrier state into `BlockManagerState`. The generic
route is returned on completion and is never interpreted as a PageSlot.

- [ ] **Step 4: Build one manager per registered physical device**

Create a side registry row keyed by the exact static
`BlockDeviceRegistration`. `register_block_devices` validates limits, allocates
one manager, then publishes both rows atomically. Whole and partition handles
resolve the same `BlockSubmissionHandle`; partition bounds remain in the
dispatch adapter.

- [ ] **Step 5: Route L4 ready nodes to L6 handles**

Replace direct access to `FileIoBlockRuntime` with
`BlockSubmissionHandle::try_admit(OwnedBioSubmission<GraphNodeRoute>)`.
Rejection returns the graph node to L4 ready state. Completion advances the
existing `BackendGraphExecution`; only graph terminalization can construct an
L4 settlement.

- [ ] **Step 6: Claim one bounded L6 service owner per device**

The existing runtime spawner submits each newly registered physical-device
service once. A service turn processes completions first, dispatches up to its
budget, and sleeps only after queue/tag recheck. No PageContainer registration
creates a new L6 owner.

- [ ] **Step 7: Delete `FileIoBlockRuntime` from PageContainer**

Remove the struct and `file_block_runtime` field after all graph and direct-I/O
paths use the device manager. Keep compatibility helper names as forwarding
functions only for one commit, then remove them before committing.

- [ ] **Step 8: Run L6/device/PageBacked parity**

```bash
cargo test -p tx-subsystems --lib block_submission_manager -- --nocapture
cargo test -p tx-subsystems --lib block_device_ -- --nocapture
cargo test -p tx-subsystems --lib page_backed -- --nocapture --test-threads=1
cargo test -p tx-fs --lib bdevfs -- --nocapture
cargo test -p tx-ext4 --lib -- --nocapture
cargo check -p tx-subsystems -p tx-fs -p tx-ext4
```

Expected: all pass; partition bounds, tags, barriers, merged completion, graph
failure, and direct-I/O scaffolding retain behavior; no per-PC block runtime
remains.

- [ ] **Step 9: Commit device-scoped L6 ownership**

```bash
git add crates/tx-subsystems/src/io_manager/block \
  crates/tx-subsystems/src/io_manager/page/service.rs \
  crates/tx-subsystems/src/device.rs \
  crates/tx-subsystems/src/page_backed/mod.rs
git commit -m "refactor(io): move block submission behind device managers"
```

## Task 9: Split Page State And Range Lock Domains

**Files:**

- Create: `crates/tx-subsystems/src/page_backed/state_domain.rs`
- Create: `crates/tx-subsystems/src/page_backed/range_domain.rs`
- Create: `crates/tx-subsystems/src/page_backed/resident.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs:745-1085`
- Modify: `crates/tx-subsystems/src/page_backed/slot.rs`
- Modify: `crates/tx-subsystems/src/page_backed/range.rs`
- Test: `crates/tx-subsystems/src/page_backed/slot_tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/range_tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Write failing independent-domain tests**

```rust
#[test]
fn slot_lookup_does_not_claim_resident_or_range_lock() {
    let pc = file_pc();
    pc.page_state_for_test().with_slot(PageIndex::new(4), |_| {
        assert!(!pc.resident_for_test().writer_locked());
        assert!(!pc.range_for_test().locked());
    });
}

#[test]
fn range_conflict_does_not_block_an_unrelated_resident_hit() {
    let pc = file_pc_with_resident(PageIndex::new(9));
    let reservation = pc.reserve_range_for_test(PageIoRange::new(1, 2));
    assert!(pc.materialize_cached_page_for_test(PageIndex::new(9)).is_some());
    drop(reservation);
}
```

- [ ] **Step 2: Run domain tests to verify RED**

```bash
cargo test -p tx-subsystems --lib independent_domain -- --nocapture
cargo test -p tx-subsystems --lib unrelated_resident_hit -- --nocapture
```

Expected: FAIL because all fields share `PageContainerStateCell`.

- [ ] **Step 3: Introduce owner wrappers without changing behavior**

```rust
pub(crate) struct ResidentDomain {
    index: SpinMutex<PageCacheIndex>,
}

pub(crate) struct PageStateDomain {
    slots: SpinMutex<BTreeMap<PageIndex, Arc<PageStateCell>>>,
}

pub(crate) struct PageStateCell {
    slot: PageSlot,
}

pub(crate) struct RangeDomain {
    reservations: SpinMutex<RangeReservationTable>,
}
```

`ResidentDomain` remains lock-backed in this task. `PageStateDomain` returns a
stable `Arc<PageStateCell>` while holding its index lock briefly; callers then
release the index before entering the per-slot FSM lock. `RangeDomain` owns
reserve/release/overlap operations and returns owned reservation IDs only.

- [ ] **Step 4: Replace `PageContainerState` with independent fields**

```rust
pub struct PageContainer {
    kind: PageContainerKind,
    page_count: u64,
    size_bytes: AtomicU64,
    resident: ResidentDomain,
    page_state: PageStateDomain,
    ranges: RangeDomain,
    page_io: FilePageIoHandle,
}
```

At this point L4 owns fetch/wait/direct request custody and L6 owns block state,
so no residual coarse state is allowed. Use snapshot-release-recheck when an
operation touches multiple domains; do not nest domain locks.

- [ ] **Step 5: Add lock-order and no-yield ratchets**

Add debug lock marks for each domain and tests that panic on nested resident/
slot-index/range acquisition. Add a source lint in existing invariant tooling
that rejects planner/device/wait calls in closures holding these lock marks.

- [ ] **Step 6: Run PageBacked race and parity tests**

```bash
cargo test -p tx-subsystems --lib page_backed -- --nocapture --test-threads=1
cargo test -p tx-subsystems --lib page_slot_ -- --nocapture
cargo test -p tx-subsystems --lib range_reservation_ -- --nocapture
cargo xtask lint invariants
rg -n "struct PageContainerState|PageContainerStateCell|file_block_runtime|file_io_service" \
  crates/tx-subsystems/src/page_backed
```

Expected: all tests and lints pass; the final `rg` prints no live definitions.

- [ ] **Step 7: Commit the lock-domain split**

```bash
git add crates/tx-subsystems/src/page_backed \
  xtask/src/lint.rs xtask/src/lint_invariants_checks.rs
git commit -m "refactor(pagebacked): split resident slot and range domains"
```

Stage only the invariant file actually selected by the repository dispatcher.

## Task 10: Reserve Publication Retirement Capacity At Prepare Time

**Files:**

- Modify: `crates/tx-substrate/src/epoch/bag.rs`
- Modify: `crates/tx-substrate/src/epoch/domain.rs:102-175`
- Modify: `crates/tx-substrate/src/epoch/mod.rs`
- Modify: `crates/tx-substrate/src/publication/mod.rs:35-140`
- Test: `crates/tx-substrate/tests/publication.rs`
- Test: `crates/tx-subsystems/src/vm/structure/recipe.rs`

- [ ] **Step 1: Write failing retire-pressure tests**

```rust
#[test]
fn prepare_reports_retire_backpressure_before_root_swap() {
    setup_epoch();
    occupy_local_retire_slot_for_test();
    let published = Published::try_new(1_u64).unwrap();
    assert_eq!(
        published.prepare_replace(2).unwrap_err(),
        PublishError::RetireBackpressure
    );
    let guard = epoch::guard();
    assert_eq!(*published.read(&guard), 1);
}

#[test]
fn successful_commit_never_calls_drain() {
    setup_epoch();
    let published = Published::try_new(1_u64).unwrap();
    let reservation = published.prepare_replace(2).unwrap();
    reset_drain_call_count_for_test();
    reservation.commit();
    assert_eq!(drain_call_count_for_test(), 0);
}
```

- [ ] **Step 2: Run publication tests to verify RED**

```bash
cargo test -p tx-substrate --test publication prepare_reports_retire -- --nocapture
cargo test -p tx-substrate --test publication successful_commit_never -- --nocapture
```

Expected: first lacks the error variant; second observes the current emergency
drain path under forced occupancy.

- [ ] **Step 3: Implement same-CPU retire-head reservation**

```rust
pub(crate) struct RetireHeadReservation {
    cpu: CpuId,
    epoch: u64,
    slot: usize,
    consumed: bool,
    _not_send: PhantomData<*const ()>,
}

pub(crate) fn try_reserve_retire_head() -> Result<RetireHeadReservation, EpochError>;

pub(crate) unsafe fn enqueue_reserved_head(
    reservation: &mut RetireHeadReservation,
    head: NonNull<RcuHead>,
);
```

The per-CPU domain marks one exact bag slot reserved. A second reservation on
that CPU returns `RetireBagOccupied`. Drop cancels an unconsumed reservation.
Enqueue verifies the same CPU and slot, samples the already-reserved epoch, and
cannot allocate, drain, wait, or fail. The reservation is `!Send`.

- [ ] **Step 4: Strengthen `Published::prepare_replace`**

Extend `PublishError` with `RetireBackpressure`. `PublishReservation` stores
the retire reservation as well as the allocated next node. Acquire the
per-Published writer before reserving and release it on any prepare failure.
`commit` becomes one AcqRel root swap plus `enqueue_reserved_head`; remove the
retry loop and `drain_with_budget(usize::MAX)`.

- [ ] **Step 5: Add interrupt/nesting/cancel tests**

Prove a nested publication prepare on the same CPU gets backpressure, dropping
an uncommitted reservation frees the slot, wrong-CPU consumption is rejected in
test mode, and a later prepare succeeds after bounded epoch drain by the normal
coordinator path.

- [ ] **Step 6: Run substrate and VM pilot tests**

```bash
cargo test -p tx-substrate --test publication -- --nocapture --test-threads=1
cargo test -p tx-substrate --test epoch -- --nocapture --test-threads=1
cargo test -p tx-subsystems --lib recipe -- --nocapture --test-threads=1
cargo check -p tx-substrate -p tx-subsystems
rg -n "drain_with_budget\(usize::MAX\)" crates/tx-substrate/src/publication
```

Expected: all tests pass; final scan prints nothing.

- [ ] **Step 7: Commit bounded publication prepare**

```bash
git add crates/tx-substrate/src/epoch crates/tx-substrate/src/publication \
  crates/tx-substrate/tests/publication.rs
git commit -m "fix(publication): reserve retire capacity before commit"
```

## Task 11: Build The Persistent Resident Root And Stable Binding

**Files:**

- Modify: `crates/tx-subsystems/src/page_backed/resident.rs`
- Modify: `crates/tx-subsystems/src/page_backed/state_domain.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Test: `crates/tx-subsystems/src/page_backed/resident.rs`

- [ ] **Step 1: Write failing persistent-root tests**

```rust
#[test]
fn insert_path_copies_bounded_nodes() {
    let root = ResidentRoot::new();
    let first = root.insert(PageIndex::new(1), binding(1, 10)).unwrap();
    let second = first.insert(PageIndex::new(1 << 20), binding(2, 11)).unwrap();
    assert_eq!(second.lookup(PageIndex::new(1)).unwrap().generation().raw(), 10);
    assert_eq!(second.lookup(PageIndex::new(1 << 20)).unwrap().generation().raw(), 11);
    assert!(second.last_update_nodes_for_test() <= RESIDENT_RADIX_LEVELS);
}

#[test]
fn removing_range_reuses_unaffected_subtrees() {
    let root = populated_root(&[1, 2, 3, 1 << 30]);
    let far = root.subtree_identity_for_test(PageIndex::new(1 << 30));
    let next = root.without_range(PageIoRange::new(1, 3)).unwrap();
    assert_eq!(next.subtree_identity_for_test(PageIndex::new(1 << 30)), far);
    assert!(next.lookup(PageIndex::new(2)).is_none());
}
```

- [ ] **Step 2: Run root tests to verify RED**

```bash
cargo test -p tx-subsystems --lib resident_root_ -- --nocapture
```

Expected: FAIL because the resident module is still a locked `PageCacheIndex`.

- [ ] **Step 3: Implement a path-copy radix root**

Use a fixed 4-bit radix over all 64 page-index bits:

```rust
const RESIDENT_RADIX_BITS: usize = 4;
const RESIDENT_RADIX_FANOUT: usize = 1 << RESIDENT_RADIX_BITS;
const RESIDENT_RADIX_LEVELS: usize = 64 / RESIDENT_RADIX_BITS;

#[derive(Clone)]
pub(crate) struct ResidentRoot {
    root: Option<Arc<ResidentRadixNode>>,
    len: usize,
}

enum ResidentRadixNode {
    Branch([Option<Arc<ResidentRadixNode>>; RESIDENT_RADIX_FANOUT]),
    Leaf(Arc<ResidentBinding>),
}
```

`insert`, exact-generation `remove`, and `without_range` allocate/copy only
nodes intersecting the changed paths. `without_range` prunes fully covered
subtrees and reuses disjoint subtrees. No method clones or materializes a full
`BTreeMap`.

- [ ] **Step 4: Implement stable binding retention**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ResidentBindingState {
    Installing = 0,
    Active = 1,
    Withdrawn = 2,
}

pub(crate) struct ResidentBinding {
    page: PageIndex,
    generation: PageGeneration,
    ppn: Ppn,
    cache_pin: PageCachePin,
    slot: Arc<PageStateCell>,
    state: AtomicU8,
}
```

Add a safety comment and focused compile/runtime tests for the cross-CPU Drop
contract before the required `unsafe impl Send + Sync`: the binding grants no
mutable frame access, allocator role-counter operations are SMP-safe, root
readers borrow it only under an epoch guard, and owned `MapPin` is acquired
separately. Do not make raw `PageCachePin` generally Send.

- [ ] **Step 5: Implement pure binding transitions**

`activate()` permits only `Installing -> Active` with Release.
`withdraw_exact(generation)` permits only matching `Active -> Withdrawn` with
AcqRel. `try_observe_active()` returns `(generation, ppn)` only from Active.
Every transition has same-generation, stale-generation, and double-transition
tests.

- [ ] **Step 6: Run root/binding tests**

```bash
cargo test -p tx-subsystems --lib resident_root_ -- --nocapture
cargo test -p tx-subsystems --lib resident_binding_ -- --nocapture
cargo check -p tx-subsystems
```

Expected: all pass; update-node counters remain bounded by radix depth and
unaffected subtree identities are retained.

- [ ] **Step 7: Commit the persistent data structure**

```bash
git add crates/tx-subsystems/src/page_backed/resident.rs \
  crates/tx-subsystems/src/page_backed/state_domain.rs \
  crates/tx-subsystems/src/page_backed/mod.rs
git commit -m "feat(pagebacked): add persistent resident root"
```

## Task 12: Publish Resident Install/Hit/Withdraw In Production

**Files:**

- Modify: `crates/tx-subsystems/src/page_backed/resident.rs`
- Modify: `crates/tx-subsystems/src/page_backed/state_domain.rs`
- Modify: `crates/tx-subsystems/src/page_backed/slot.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs:2730-3600,3870-3970`
- Modify: `crates/tx-subsystems/src/page_backed/core_tests.rs`
- Modify: `crates/tx-subsystems/src/page_backed/slot_tests.rs`
- Create: `tools/shell-tests/io-resident-smp-witness.c`
- Modify: `xtask/src/test.rs`

- [ ] **Step 1: Write failing stale-root and pin-race tests**

```rust
#[test]
fn stale_root_cannot_pin_after_withdraw_linearization() {
    let pc = file_pc_with_resident(PageIndex::new(4));
    let guard = epoch::guard();
    let stale = pc.resident_for_test().lookup_binding(PageIndex::new(4), &guard).unwrap();
    pc.withdraw_resident_for_test(PageIndex::new(4)).unwrap();
    assert!(stale.try_map_pin().is_none());
}

#[test]
fn pin_acquired_before_withdraw_survives_root_removal() {
    let pc = file_pc_with_resident(PageIndex::new(4));
    let pin = pc.try_pin_resident_for_test(PageIndex::new(4)).unwrap();
    pc.withdraw_resident_for_test(PageIndex::new(4)).unwrap();
    assert!(pin.confirm().is_ok());
    drop(pin);
}
```

- [ ] **Step 2: Run resident race tests to verify RED**

```bash
cargo test -p tx-subsystems --lib stale_root_cannot_pin -- --nocapture
cargo test -p tx-subsystems --lib pin_acquired_before -- --nocapture
```

Expected: FAIL because the production resident path is lock-backed.

- [ ] **Step 3: Add exclusive publication/withdrawal claims to PageSlot**

Extend the internal transition representation without changing the public
semantic states:

```rust
enum PageSlotTransitionClaim {
    Publishing { generation: PageGeneration, binding: u64 },
    Withdrawing { generation: PageGeneration, binding: u64 },
}
```

`begin_publication`, `validate_publication`, `finish_publication`,
`abort_publication`, `begin_withdrawal`, `validate_withdrawal`,
`finish_withdrawal`, and `abort_withdrawal` all operate under the per-slot FSM
lock. A live claim rejects competing fetch, dirty, writeback, truncate, reclaim,
or direct-write transition for that generation.

- [ ] **Step 4: Implement the guard-scoped resident hit**

```rust
fn try_pin_resident(&self, page: PageIndex) -> Option<MaterializedPageSnapshot> {
    let guard = epoch::guard();
    let binding = self.resident.lookup(page, &guard)?;
    let (generation, ppn) = binding.try_observe_active()?;
    let map_pin = page_allocator::acquire_map_pin(ppn).ok()?;
    if !binding.revalidate_active(generation, ppn)
        || !binding.slot().resident_generation_matches(generation)
    {
        drop(map_pin);
        return None;
    }
    Some(MaterializedPageSnapshot::allocated(ppn, map_pin, generation))
}
```

Return only owned data from the function. The epoch guard and binding borrow
end before copying, mapping, yielding, or I/O.

- [ ] **Step 5: Implement prepare-claim-publish install**

Create an Installing binding under the PageSlot publication claim. Under the
resident writer, derive and prepare the root. Revalidate the claim, activate
the binding, commit infallibly, finish PageSlot publication, then wake waiters.
On prepare/backpressure/revalidation failure, abort the claim and retain the old
root.

- [ ] **Step 6: Implement prepare-withdraw-remove**

With the caller's range/reclaim claim active, reserve PageSlot withdrawal,
prepare the root without the exact generation, revalidate, mark Withdrawn,
commit removal, finish PageSlot transition, then release the semantic claim and
wake. Previously acquired pins survive; stale roots cannot acquire new pins.

- [ ] **Step 7: Route all resident mutations through the protocol**

Convert fetch install, reclaim, truncate, direct-write invalidation, explicit
remove, device prepopulation, and teardown. Remove the locked `PageCacheIndex`
backend and mark duplication. PageSlot remains dirty/writeback authority;
referenced aging uses a separate atomic counter.

- [ ] **Step 8: Add deterministic race-model and SMP4 witnesses**

Host tests interleave lookup/pin/install/withdraw/truncate/reclaim at every
linearization hook. The guest witness runs four threads repeatedly reading one
hot set while a fifth bounded control loop invalidates/refetches a disjoint set.
Require:

```text
TX_IO_RESIDENT_SMP hits=<n> retries=<n> errors=0 stale_pins=0
```

- [ ] **Step 9: Run publication and SMP gates**

```bash
cargo test -p tx-subsystems --lib resident_ -- --nocapture --test-threads=1
cargo test -p tx-subsystems --lib page_backed -- --nocapture --test-threads=1
cargo test -p tx-subsystems --lib page_slot_ -- --nocapture
cargo xtask test io-submission-witness --target rv64-qemu --case resident-smp --smp 4 --timeout-ms 120000
cargo xtask fault-decode --target rv64-qemu \
  --serial target/io-submission/witness/resident-smp/serial.log --all --brief
```

Expected: all pass; serial marker reports zero errors/stale pins and fault
decode reports no unexpected trap.

- [ ] **Step 10: Compare P4 resident performance and commit**

Run five interleaved hot baseline/candidate pairs. Require single-hart
throughput at least 98% and either four-hart throughput +20% or resident-lock
wait -50%. A failed performance gate leaves the prior binary selected and the
root work behind a non-default build mode until corrected.

```bash
git add crates/tx-subsystems/src/page_backed \
  tools/shell-tests/io-resident-smp-witness.c xtask/src/test.rs
git commit -m "feat(pagebacked): publish lock-free resident roots"
```

## Task 13: Extend The Existing Lease And Settlement To Multi-Page Reads

**Files:**

- Modify: `crates/tx-subsystems/src/fs_iface/plan.rs:84-250,454-590`
- Modify: `crates/tx-subsystems/src/page_backed/lifecycle.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs:2940-3385,3620-3755`
- Modify: `crates/tx-subsystems/src/io_manager/page/manager.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/service.rs`
- Test: `crates/tx-subsystems/src/page_backed/lifecycle_tests.rs`
- Test: `crates/tx-subsystems/src/page_backed/core_tests.rs`

- [ ] **Step 1: Write failing multi-page lease tests**

```rust
#[test]
fn read_lease_projects_page_cache_segments_without_direct_io_aliasing() {
    let lease = read_lease(&[(4, 10, 0x100), (5, 11, 0x101)]);
    assert!(matches!(
        lease.target(),
        IoDataTarget::PageCacheSegments { ref vecs, .. } if vecs.len() == 2
    ));
}

#[test]
fn settlement_carries_one_result_per_page_generation() {
    let settlement = settlement_for_pages(&[(4, 10, Ok(())), (5, 11, Err(Errno::EIO))]);
    assert_eq!(settlement.completions.as_slice().len(), 2);
    assert_eq!(settlement.resources.segments().len(), 2);
}
```

- [ ] **Step 2: Run lease tests to verify RED**

```bash
cargo test -p tx-subsystems --lib read_lease_projects -- --nocapture
cargo test -p tx-subsystems --lib settlement_carries_one -- --nocapture
```

Expected: FAIL because reads retain one `CachedFrame` and multi-page writeback
is projected as `Direct`.

- [ ] **Step 3: Make `PageDataLease` direction-neutral**

```rust
enum PageDataDirection {
    ReadTarget,
    WriteSource,
}

struct PageDataSegment {
    page: PageIndex,
    generation: PageGeneration,
    frame: PageFrameRef,
    offset: u32,
    len: u32,
    retention: PageDataRetention,
}

pub(super) struct PageDataLease {
    id: IoDataLeaseId,
    direction: PageDataDirection,
    segments: Box<[PageDataSegment]>,
}
```

`PageDataRetention` privately holds either an installing read `CachedFrame` or
an existing writeback `PageLease`. Empty, duplicate-page, mixed-direction,
zero-length, and page-overflow construction is rejected.

- [ ] **Step 4: Extend existing neutral projections**

Add `PageCacheSegments { lease, vecs }` to both `IoDataSource` and
`IoDataTarget`. Preserve `Direct` exclusively for DMA-pinned user pages. Update
lease extraction, graph validation, planners, and tests. Do not add a new lease
or graph family.

- [ ] **Step 5: Admit one bounded read range**

Add a private `PageContainer::admit_file_read_batch` used by `step_read` and
fault clustering. It probes up to
`min(DEFAULT_PAGE_BATCH_PAGES, MAX_PAGE_BATCH_PAGES, remaining_pages)`, creates
one PageSlot fetch generation and target frame per miss, deduplicates already
fetching pages, then atomically admits one `PageIoRequest` range and one
multi-segment lease. Allocation or manager rejection rolls back every newly
created slot/frame.

- [ ] **Step 6: Aggregate per-page completion**

Use the existing `PageCompletionList`. Each successful page includes its exact
range, generation, kind, and frame. Failed/unissued pages carry explicit errors.
L4 emits one `OwnedPageIoSettlement` only after every demand page is terminal;
optional tail completion is separable and never changes demand bytes.

- [ ] **Step 7: Install successful siblings and roll back failed pages**

PageBacked consumes each completion independently. It installs a successful
exact generation through the resident publication protocol and transitions a
failed generation to retry/error. Partial failure must leave no lease, target
frame, waiter, graph node, or PageSlot claim orphaned.

- [ ] **Step 8: Run multi-page lifetime and partial-failure tests**

```bash
cargo test -p tx-subsystems --lib page_data_lease -- --nocapture
cargo test -p tx-subsystems --lib multi_page_read -- --nocapture
cargo test -p tx-subsystems --lib partial_read_completion -- --nocapture
cargo test -p tx-subsystems --lib page_backed -- --nocapture --test-threads=1
cargo test -p tx-subsystems --lib io_manager -- --nocapture
```

Expected: all pass; direct-I/O variants remain distinct; allocation counters
return to baseline after every rejected/failed batch.

- [ ] **Step 9: Commit multi-page ownership/completion**

```bash
git add crates/tx-subsystems/src/fs_iface/plan.rs \
  crates/tx-subsystems/src/page_backed/lifecycle.rs \
  crates/tx-subsystems/src/page_backed/mod.rs \
  crates/tx-subsystems/src/io_manager/page
git commit -m "feat(io): settle multi-page read leases"
```

## Task 14: Lower Multi-Page Ext4 And Bdev-Fs Reads

**Files:**

- Modify: `crates/tx-ext4/src/planner.rs:390-570`
- Modify: `crates/tx-ext4/src/read_backend.rs`
- Modify: `crates/tx-ext4/src/tests_v3.rs`
- Modify: `crates/tx-fs/src/bdevfs/mod.rs:281-340`
- Test: `crates/tx-ext4/src/planner.rs`
- Test: `crates/tx-fs/src/bdevfs/mod.rs`

- [ ] **Step 1: Write failing ext4 holes/extents tests**

```rust
#[test]
fn range_read_separates_holes_and_contiguous_extents() {
    let request = read_request_for_pages(4, 5, target_segments(5));
    let mapping = mapping_runs(&[
        mapped(4, 40),
        mapped(5, 41),
        hole(6),
        mapped(7, 90),
        mapped(8, 91),
    ]);
    let plan = plan_read_range(geometry(), &request, mapping).unwrap();
    assert_eq!(plan.zero_pages(), &[PageIndex::new(6)]);
    assert_eq!(plan.graph().nodes().len(), 2);
}

#[test]
fn bdev_range_read_maps_one_contiguous_lba_run() {
    let plan = plan_page_range_bio(object(), PageIoRange::new(8, 4), target_segments(4)).unwrap();
    assert_eq!(plan.lba.block_count(), 32);
    assert_eq!(plan.vecs.len(), 4);
}
```

- [ ] **Step 2: Run planner tests to verify RED**

```bash
cargo test -p tx-ext4 range_read_separates_holes -- --nocapture
cargo test -p tx-fs bdev_range_read_maps -- --nocapture
```

Expected: FAIL because both planners are one-page shaped.

- [ ] **Step 3: Map ext4 logical ranges without moving ownership**

Walk every requested logical page through the existing extent/mapping cache.
Produce ordered `Hole` and `MappedRun { logical_start, physical_start,
page_count, target_slice }` values. Coalesce only physically and logically
adjacent mapped pages. Metadata misses preserve the original complete request
and target lease through the existing resume token.

- [ ] **Step 4: Lower runs into the existing graph**

Zero each hole target and emit one successful `PageCompletion` for its exact
generation. Emit one `BackendBioNode` per contiguous mapped run with the
corresponding PageCacheSegments slice. Keep graph dependencies and payload
offsets exact; do not interpret device limits here.

- [ ] **Step 5: Add bdev-fs range lowering**

Implement `plan_page_range_bio` by checked conversion from page range to
partition-relative LBA range and exact SG target slice. Keep `plan_page_bio` as
a one-page forwarding wrapper until callsites migrate, then remove duplicate
logic.

- [ ] **Step 6: Add partial final-page and overflow tests**

Cover EOF within the final page, extent boundary, hole between extents,
metadata-first resume, partition end, LBA multiplication overflow, target count
mismatch, and direct-I/O target rejection for this buffered range path.

- [ ] **Step 7: Run ext4/bdev-fs integration tests**

```bash
cargo test -p tx-ext4 planner -- --nocapture
cargo test -p tx-ext4 --lib -- --nocapture
cargo test -p tx-fs --lib bdevfs -- --nocapture
cargo test -p tx-subsystems --lib multi_page_read -- --nocapture
cargo check -p tx-ext4 -p tx-fs -p tx-subsystems
```

Expected: all pass; one-page callers retain parity; multi-page mapped/hole
requests settle each target generation correctly.

- [ ] **Step 8: Commit filesystem range lowering**

```bash
git add crates/tx-ext4/src/planner.rs crates/tx-ext4/src/read_backend.rs \
  crates/tx-ext4/src/tests_v3.rs crates/tx-fs/src/bdevfs/mod.rs
git commit -m "feat(fs): lower multi-page reads into I/O graphs"
```

## Task 15: Add Device-Limit Splitting, Compatible Merge, And Short Plugging

**Files:**

- Modify: `crates/tx-subsystems/src/io_manager/block/manager.rs`
- Modify: `crates/tx-subsystems/src/io_manager/block/mod.rs:110-210`
- Modify: `crates/tx-subsystems/src/io_manager/page/manager.rs`
- Modify: `crates/tx-subsystems/src/io_manager/runtime/observe.rs`
- Test: `crates/tx-subsystems/src/io_manager/block/manager.rs`
- Test: `crates/tx-subsystems/src/io_manager/block/mod.rs`

- [ ] **Step 1: Write failing split/merge property tests**

```rust
#[test]
fn split_obeys_every_device_limit() {
    let limits = test_limits_with(16 * 1024, 3, 8 * 1024, 0xffff);
    let pieces = split_submission(owned_bio_bytes(48 * 1024, 6), limits).unwrap();
    assert!(pieces.iter().all(|piece| piece.bio.byte_len() <= 16 * 1024));
    assert!(pieces.iter().all(|piece| piece.bio.plan.vecs.len() <= 3));
    assert!(pieces.iter().all(|piece| !piece.crosses_segment_boundary(limits)));
}

#[test]
fn merge_rejects_limit_and_barrier_crossing() {
    let limits = test_limits_with(8 * 1024, 2, 4 * 1024, u64::MAX);
    assert_eq!(merge_pair(two_adjacent_4k_bios(), limits).unwrap().byte_len(), 8 * 1024);
    assert!(merge_pair(two_adjacent_8k_bios(), limits).is_err());
    assert!(merge_pair(bios_across_barrier(), limits).is_err());
}
```

- [ ] **Step 2: Run split/merge tests to verify RED**

```bash
cargo test -p tx-subsystems --lib split_obeys_every -- --nocapture
cargo test -p tx-subsystems --lib merge_rejects_limit -- --nocapture
```

Expected: FAIL because current merge checks adjacency/fence only and no generic
split exists.

- [ ] **Step 3: Split before mutable queue admission**

Implement checked slicing over LBA and `BioVec` offsets. Split on the earliest
of max transfer, max segments, max segment bytes, segment boundary, partition
end, and payload end. Every piece retains parent graph route, piece index/count,
barrier domain, priority, flags, and payload offset. Build the complete bounded
piece vector before acquiring the L6 queue lock; reject and return the original
owned submission when full capacity is unavailable.

- [ ] **Step 4: Merge only within immutable limits**

Extend `can_merge_with` to require same device/op/flags/priority/barrier domain,
adjacent LBA and payload, and a combined request satisfying every limit. Record
one rejection reason counter: nonadjacent, semantics, barrier, transfer,
segments, segment-size, boundary, partition, or payload.

- [ ] **Step 5: Aggregate split completion exactly once**

Create one parent counter before enqueue. A piece error records the first error
and prevents unsent dependent pieces; already in-flight siblings may complete.
Return one graph-node completion only when every admitted piece is terminal.

- [ ] **Step 6: Implement bounded plugs**

Add a manager-owned plug bucket keyed by current service turn and priority.
Flush on turn end, demand/background BIO thresholds, idle queue, barrier/FUA,
or impending yield/wait. Completion and barriers bypass plugs. Plugs own only
already-admitted submissions and never hold owner locks.

- [ ] **Step 7: Add randomized split conservation tests**

For deterministic seeds 1 through 256, generate aligned SG/LBA inputs and
validated limits. Assert sum(piece bytes) equals original bytes, LBA/payload
coverage has no gap/overlap, every piece validates, and recombined completion
fires once. Add explicit zero/overflow/misalignment cases.

- [ ] **Step 8: Run block and graph tests**

```bash
cargo test -p tx-subsystems --lib io_manager::block -- --nocapture
cargo test -p tx-subsystems --lib split_ -- --nocapture
cargo test -p tx-subsystems --lib plug_ -- --nocapture
cargo test -p tx-subsystems --lib backend_graph -- --nocapture
cargo test -p tx-ext4 --lib -- --nocapture
cargo test -p tx-fs --lib bdevfs -- --nocapture
```

Expected: all pass; metrics report split/merge/plug reasons; barriers retain
strict order.

- [ ] **Step 9: Commit generic clustering/splitting**

```bash
git add crates/tx-subsystems/src/io_manager/block \
  crates/tx-subsystems/src/io_manager/page/manager.rs \
  crates/tx-subsystems/src/io_manager/runtime/observe.rs
git commit -m "feat(io): split and plug device requests"
```

## Task 16: Add Adaptive And Explicit Readahead

**Files:**

- Create: `crates/tx-subsystems/src/io_manager/page/readahead.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/manager.rs`
- Modify: `crates/tx-subsystems/src/io_manager/page/mod.rs`
- Modify: `crates/tx-subsystems/src/page_backed/mod.rs`
- Modify: `crates/tx-subsystems/src/vfs/structure.rs`
- Modify: `crates/tx-subsystems/src/vfs/execution.rs`
- Modify: `crates/tx-subsystems/src/io_manager/runtime/observe.rs`
- Modify: `crates/tx-shims/src/linux_syscall/io.rs`
- Modify: `crates/tx-shims/src/linux_syscall/tests/high_stakes_syscalls.rs`
- Modify: `tools/shell-tests/io-submission-witness.c`
- Modify: `xtask/src/test.rs`
- Test: `crates/tx-subsystems/src/io_manager/page/readahead.rs`
- Test: `crates/tx-subsystems/src/page_backed/mod.rs`

- [ ] **Step 1: Write failing bounded-window and stream-isolation tests**

```rust
#[test]
fn sequential_stream_grows_but_never_exceeds_cap() {
    let mut state = ReadaheadState::new(DEFAULT_READAHEAD_PAGES, MAX_READAHEAD_PAGES);
    let stream = ReadaheadStreamId::from_raw(7);
    assert_eq!(state.on_demand(stream, 10, false).unwrap().page_count, 4);
    assert_eq!(state.on_demand(stream, 11, true).unwrap().page_count, 8);
    for page in 12..96 {
        let _ = state.on_demand(stream, page, true);
    }
    assert!(state.window_pages(stream) <= MAX_READAHEAD_PAGES);
}

#[test]
fn random_and_backward_reads_disable_only_their_stream() {
    let mut state = ReadaheadState::new(DEFAULT_READAHEAD_PAGES, MAX_READAHEAD_PAGES);
    let sequential = ReadaheadStreamId::from_raw(11);
    let random = ReadaheadStreamId::from_raw(12);
    let _ = state.on_demand(sequential, 40, false);
    let _ = state.on_demand(sequential, 41, true);
    let _ = state.on_demand(random, 90, false);
    let _ = state.on_demand(random, 2, false);
    assert!(state.window_pages(sequential) > 0);
    assert_eq!(state.window_pages(random), 0);
}
```

Add focused cases for marker extension, queue-pressure prefix admission,
cancel, duplicate resident/slot suppression, reclaimed-unused accounting, and
usefulness only after a later demand consumption.

- [ ] **Step 2: Run the policy tests to verify RED**

```bash
cargo test -p tx-subsystems --lib readahead_ -- --nocapture
```

Expected: FAIL because `ReadaheadState`, stream history, and real optional
window admission do not exist.

- [ ] **Step 3: Add one stable stream token per open-file description**

Define the approved token in the L4 readahead module and store it on
`OpenFile`, so `dup` and `fork` share it while a separate `open` gets a new
token:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ReadaheadStreamId(u64);

impl ReadaheadStreamId {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

pub struct OpenFile {
    // Existing fields remain unchanged.
    readahead_stream: ReadaheadStreamId,
}
```

Allocate the token once in every `OpenFile` constructor. Add a focused VFS
test that cloned capabilities observe the same token and separately opened
files do not.

- [ ] **Step 4: Implement bounded L4 readahead mechanics**

Use the existing `PageIoRequest`, `PageIoOp::Readahead`,
`PageIoPriority::Readahead`, and `PageIoFlags::READAHEAD` family. The private
policy state is keyed by `(PageContainerKey, ReadaheadStreamId)` and returns a
bounded hint, never a second request type:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadaheadHint {
    pub start_page: u64,
    pub page_count: u32,
    pub stream: ReadaheadStreamId,
}

impl ReadaheadHint {
    pub fn bounded(start_page: u64, page_count: u64, stream: ReadaheadStreamId) -> Option<Self> {
        let page_count = u32::try_from(page_count.min(u64::from(MAX_READAHEAD_PAGES))).ok()?;
        (page_count != 0).then_some(Self { start_page, page_count, stream })
    }

    pub fn from_byte_range(start: u64, end: u64, stream: ReadaheadStreamId) -> Option<Self> {
        if end <= start {
            return None;
        }
        let page_bytes = PAGE_SIZE as u64;
        let start_page = start / page_bytes;
        let end_page = end.div_ceil(page_bytes);
        Self::bounded(start_page, end_page - start_page, stream)
    }
}
```

First demand miss opens the four-page optional window, sequential consumption
grows it to at most 64 pages, marker consumption queues the next window, and
backward/random access shrinks it to zero. Drop optional tails under queue or
memory pressure. Optional failure must never overwrite a demand result.

- [ ] **Step 5: Wire adaptive reads through the existing demand path**

Pass `OpenFile::readahead_stream()` into the existing PageBacked `step_read`
boundary. After demand settlement, report the consumed page indices to L4.
L4 may submit one optional `OwnedPageIoSubmission` through the same manager
handle used by demand I/O. Optional pages use ordinary PageSlot generations,
resident install, reclaim charging, and settlement; they do not install VM
PTEs or call an ext4-specific readahead API.

- [ ] **Step 6: Wire `readahead(2)` to checked advisory admission**

Keep the current fd/readability/regular-file checks, then validate the signed
offset and checked end before converting the byte range:

```rust
let offset = args[1] as i64;
let count = args[2];
if offset < 0 {
    return SyscallResult::Error(EINVAL_VALUE);
}
if count == 0 {
    return SyscallResult::Return(0);
}
let end = match (offset as u64).checked_add(count) {
    Some(end) => end,
    None => return SyscallResult::Error(EINVAL_VALUE),
};
let Some(hint) = ReadaheadHint::from_byte_range(
    offset as u64,
    end,
    file.readahead_stream(),
) else {
    return SyscallResult::Return(0);
};
page_container.advise_readahead(hint);
SyscallResult::Return(0)
```

`PageContainer::advise_readahead` performs best-effort L4 admission and may
drop a bounded tail after successful structural validation. Rejected optional
admission returns and releases the complete owned bundle locally.

- [ ] **Step 7: Add observation and syscall regression tests**

Assert requested, admitted, completed, demand-consumed, reclaimed-unused,
canceled, and duplicate pages separately. Extend the high-stakes syscall test
with negative offset, overflow, zero length, and a valid range that increments
the PageContainer/L4 admission witness. Retain the existing demand-over-
readahead priority test:

```bash
cargo test -p tx-subsystems --lib io_manager_page_queue_prioritizes_demand_over_readahead -- --nocapture
cargo test -p tx-shims --lib dispatch_file_advice_readahead_and_sync_file_range_validate_inputs -- --nocapture
```

- [ ] **Step 8: Add a deterministic guest syscall witness**

Add this mode without changing the existing witness output format:

```text
io-submission-witness readahead FILE BYTES WINDOW
```

The mode calls `readahead(2)`, consumes alternating admitted windows with
`pread`, leaves alternating windows unused, and emits one `TX_IO_WITNESS`
line. Add `--case readahead` to the existing xtask lane.

- [ ] **Step 9: Run the P7 verification matrix**

```bash
cargo test -p tx-subsystems --lib readahead_ -- --nocapture
cargo test -p tx-subsystems --lib io_manager_page_queue_prioritizes_demand_over_readahead -- --nocapture
cargo test -p tx-subsystems --lib page_backed -- --nocapture
cargo test -p tx-shims --lib dispatch_file_advice_readahead_and_sync_file_range_validate_inputs -- --nocapture
cargo test -p xtask io_submission_witness -- --nocapture
cargo check -p tx-subsystems -p tx-shims
cargo xtask test io-submission-witness --target rv64-qemu --case readahead --smp 4 --timeout-ms 120000
```

Expected: all host tests pass; the guest marker reports zero errors; sequential
usefulness is nonzero; unused/canceled work does not change demand bytes or
errors; random amplification remains bounded by the fixed cap.

- [ ] **Step 10: Commit readahead**

```bash
git add crates/tx-subsystems/src/io_manager/page \
  crates/tx-subsystems/src/io_manager/runtime/observe.rs \
  crates/tx-subsystems/src/page_backed/mod.rs \
  crates/tx-subsystems/src/vfs/structure.rs \
  crates/tx-subsystems/src/vfs/execution.rs \
  crates/tx-shims/src/linux_syscall/io.rs \
  crates/tx-shims/src/linux_syscall/tests/high_stakes_syscalls.rs \
  tools/shell-tests/io-submission-witness.c xtask/src/test.rs
git commit -m "feat(io): add adaptive file readahead"
```

## Task 17: Run Final Correctness And Performance Selection

**Files:**

- Modify: `tools/io-submission-perf.py`
- Modify: `tools/tests/test_io_submission_perf.py`
- Create: `tools/io-submission-candidate.json`
- Modify: `docs/progress/research/2026-08-05-io-submission-performance-baseline.md`
- Modify: `docs/progress/plans/2026-08-05-io-submission-manager-performance.json`
- Modify: `docs/progress/STATUS.md`
- Artifact: `target/ext4/tier1/${RUN_ID}/acceptance-receipt.json`
- Artifact: `target/perf/io-submission/${RUN_ID}/acceptance-receipt.json`

- [ ] **Step 1: Write failing candidate-integrity tests**

```python
def test_rejects_candidate_without_fresh_tier1_receipt(self):
    receipt = valid_receipt(pair_count=5)
    receipt["candidate"]["tier1_receipt"] = None
    self.assertEqual(evaluate(receipt)["status"], "failed")

def test_rejects_incomplete_correctness_matrix(self):
    receipt = valid_receipt(pair_count=5)
    del receipt["correctness"]["barrier_order"]
    self.assertEqual(evaluate(receipt)["status"], "failed")

def test_rejects_non_interleaved_pairs(self):
    receipt = valid_receipt(pair_count=5)
    receipt["runs"] = sorted(receipt["runs"], key=lambda run: run["role"])
    self.assertEqual(evaluate(receipt)["status"], "failed")
```

- [ ] **Step 2: Run verifier tests to verify RED**

```bash
python3 -m unittest tools.tests.test_io_submission_perf
```

Expected: FAIL because the evaluator does not yet require a fresh candidate-
bound Tier 1 receipt, the full correctness matrix, and interleaved pair order.

- [ ] **Step 3: Fail-close final candidate evaluation**

Require schema `tx.io_submission.performance_receipt.v1`, at least five
interleaved complete pairs, known trace loss, identical declared geometry,
all bound artifact hashes, and these named correctness facts:

```python
REQUIRED_CORRECTNESS = (
    "owned_settlement_once",
    "stale_generation_rejected",
    "truncate_invalidate_race",
    "barrier_order",
    "fsync_frontier",
    "smp4_same_page",
    "ext4_tier1",
)
```

Bind the candidate revision and dirty declaration to the fresh ext4 receipt.
Unknown or missing counters, trace loss, geometry, workload, image, toolchain,
or hash data is a hard failure. Keep receipt writing temp-file + fsync +
single-rename and reject overwrite of an accepted receipt.

- [ ] **Step 4: Freeze the candidate configuration**

Create `tools/io-submission-candidate.json` as one comparison config. It imports
the baseline role from `tools/io-submission-baseline.json`, pins the same
machine, CPU, RAM, storage, filesystem images, workload corpus, cache mode,
repetitions, and build concurrency, and selects the production L4, L6,
resident publication, multi-page, split/merge/plug, and bounded readahead paths
for the candidate role. Keep multi-queue disabled for both roles.

- [ ] **Step 5: Run short correctness gates before the long campaign**

```bash
python3 -m unittest tools.tests.test_io_submission_perf
cargo test -p tx-substrate --test publication -- --nocapture
cargo test -p tx-subsystems --lib io_manager -- --nocapture
cargo test -p tx-subsystems --lib page_backed -- --nocapture
cargo test -p tx-ext4 --test mutation_lifecycle -- --nocapture
cargo test -p tx-ext4 --lib -- --nocapture
cargo test -p tx-fs --lib bdevfs -- --nocapture
cargo -q xtask unit
cargo xtask progress validate
```

Expected: all pass. Stop before QEMU if any host gate fails.

- [ ] **Step 6: Run one fresh candidate-bound ext4 Tier 1 campaign**

Use one immutable run ID tied to the candidate revision:

```bash
REVISION="$(git rev-parse --short=12 HEAD)"
RUN_ID="io-submission-p8-${REVISION}"
cargo xtask ext4 tier1 --run-id "$RUN_ID"
```

If a crash cut fails, retain the run directory and resume only at that cut:

```bash
test -n "${FAILED_CUT:?set FAILED_CUT to the exact runner-reported crash-cut id}"
cargo xtask ext4 tier1 --run-id "$RUN_ID" \
  --resume --start-cut "$FAILED_CUT"
```

Do not replay valid completed prefixes. After success, verify the immutable
receipt independently:

```bash
cargo xtask ext4 tier1 --verify-receipt \
  "target/ext4/tier1/${RUN_ID}/acceptance-receipt.json"
```

- [ ] **Step 7: Collect five interleaved matched A/B pairs**

Run hot resident, cold sequential, random, raw SG/split/merge, concurrent
1/2/4-hart readers, fsync/writeback, and fixed clean-build workloads:

```bash
python3 tools/io-submission-perf.py collect \
  --config tools/io-submission-candidate.json \
  --out target/perf/io-submission/p8-matched
```

The collector must alternate baseline/candidate for each pair and reject host,
image, geometry, workload, cache-state, or trace-loss drift.

- [ ] **Step 8: Evaluate and verify the immutable P8 receipt**

```bash
python3 tools/io-submission-perf.py evaluate \
  --baseline target/perf/io-submission/p8-matched/baseline \
  --candidate target/perf/io-submission/p8-matched/candidate \
  --out target/perf/io-submission/p8-final/acceptance-receipt.json
python3 tools/io-submission-perf.py verify \
  --receipt target/perf/io-submission/p8-final/acceptance-receipt.json
```

Expected: aligned payload extra-copy and normal bounce bytes are zero; all
correctness/error gates pass; hot-resident, sequential, random-amplification,
write-amplification, and clean-build thresholds from the approved design pass.
Any failed threshold keeps the prior production binary/config selected and
records the precise blocker.

- [ ] **Step 9: Record selection evidence and commit**

Record both receipt paths and SHA-256 values, candidate revision/dirty state,
threshold table, selected binary/config, failed gates if any, and the P9
trigger command in the research note, JSON stage, and STATUS. Do not commit
`target/` artifacts.

```bash
git add tools/io-submission-perf.py tools/tests/test_io_submission_perf.py \
  tools/io-submission-candidate.json \
  docs/progress/research/2026-08-05-io-submission-performance-baseline.md \
  docs/progress/plans/2026-08-05-io-submission-manager-performance.json \
  docs/progress/STATUS.md
git commit -m "test(io): select submission manager candidate"
```

## Task 18: Decide Conditional Device Multi-Queue

**Files:**

- Modify: `tools/io-submission-perf.py`
- Modify: `tools/tests/test_io_submission_perf.py`
- Conditional Create: `tools/io-submission-multiqueue.json`
- Conditional Modify: `crates/tx-subsystems/src/io_manager/block/manager.rs`
- Conditional Modify: `crates/tx-subsystems/src/io_manager/block/mod.rs`
- Conditional Modify: `crates/tx-subsystems/src/device.rs`
- Conditional Modify: `crates/tx-drivers/src/virtio/mmio.rs`
- Conditional Modify: `crates/tx-drivers/src/virtio/blk.rs`
- Modify: `docs/progress/research/2026-08-05-io-submission-performance-baseline.md`
- Modify: `docs/progress/plans/2026-08-05-io-submission-manager-performance.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Write failing trigger-evaluation tests**

```python
def test_multi_queue_trigger_requires_every_saturation_fact(self):
    receipt = valid_receipt(pair_count=5)
    receipt["hardware"]["queue_count"] = 2
    receipt["metrics"].update({
        "active_submitter_harts": 4,
        "queue_occupancy_ratio": 0.80,
        "l6_saturation_ratio": 0.15,
        "upstream_bottleneck": None,
    })
    self.assertTrue(multi_queue_trigger(receipt)["triggered"])
    for field in ("active_submitter_harts", "queue_occupancy_ratio", "l6_saturation_ratio"):
        broken = copy.deepcopy(receipt)
        broken["metrics"][field] = 0
        self.assertFalse(multi_queue_trigger(broken)["triggered"])

def test_multi_queue_trigger_rejects_single_hardware_queue(self):
    receipt = valid_receipt(pair_count=5)
    receipt["hardware"]["queue_count"] = 1
    self.assertFalse(multi_queue_trigger(receipt)["triggered"])
```

- [ ] **Step 2: Run trigger tests to verify RED**

```bash
python3 -m unittest tools.tests.test_io_submission_perf
```

Expected: FAIL because the trigger does not yet fail-close on every required
saturation and hardware fact.

- [ ] **Step 3: Implement the evidence-only trigger**

`multi-queue-trigger` returns a structured decision and reason list. It is true
only when the accepted single-queue P8 receipt proves: at least four active
submitter harts, more than one advertised hardware queue, occupancy at least
0.80, L6 lock/service saturation at least 0.15 of end-to-end block service,
and no dominant mapping/copy/readahead/ext4 bottleneck. Missing or unknown
fields return false.

- [ ] **Step 4: Evaluate the accepted single-queue receipt**

```bash
python3 tools/io-submission-perf.py multi-queue-trigger \
  --receipt target/perf/io-submission/p8-final/acceptance-receipt.json
```

Expected: one machine-readable decision containing every input and rejection
reason. Do not inspect ad hoc counters outside this accepted receipt.

- [ ] **Step 5A: Close P9 when the trigger is false**

Do not create `BlockQueueShardSet` and do not change driver/kernel queue code.
Record the negative result, receipt SHA-256, failed trigger facts, and the
single-queue production selection in the research note, JSON stage, and
STATUS. This is a complete P9 outcome.

```bash
git add tools/io-submission-perf.py tools/tests/test_io_submission_perf.py \
  docs/progress/research/2026-08-05-io-submission-performance-baseline.md \
  docs/progress/plans/2026-08-05-io-submission-manager-performance.json \
  docs/progress/STATUS.md
git commit -m "perf(io): close multi-queue gate"
```

- [ ] **Step 5B: If and only if triggered, write failing shard tests**

```rust
#[test]
fn block_queue_shards_preserve_device_wide_barrier_order() {
    let mut shards = two_queue_shard_set();
    let before = shards.submit(data_bio_on_hart(0)).unwrap();
    let barrier = shards.submit(barrier_bio_on_hart(1)).unwrap();
    let after = shards.submit(data_bio_on_hart(2)).unwrap();
    assert!(shards.dispatch(after).is_none());
    shards.complete(before.tag(), Ok(())).unwrap();
    assert_eq!(shards.dispatch_next().unwrap().tag(), barrier.tag());
}

#[test]
fn block_queue_shards_complete_each_tag_once() {
    let mut shards = two_queue_shard_set();
    let dispatch = shards.submit(data_bio_on_hart(3)).unwrap();
    assert!(shards.complete(dispatch.tag(), Ok(())).is_ok());
    assert_eq!(shards.complete(dispatch.tag(), Ok(())), Err(BlockQueueError::UnknownTag));
}
```

Run:

```bash
cargo test -p tx-subsystems --lib block_queue_shards_ -- --nocapture
```

Expected: FAIL because `BlockQueueShardSet` is intentionally absent before
the trigger passes.

- [ ] **Step 6B: Implement the minimal L6/driver sharding candidate**

Add `BlockQueueShardSet` only inside L6. Preserve one device-wide monotonic
barrier sequence and one exactly-once tag-completion authority. Queue selection
uses the submitter hart and immutable hardware-queue capabilities inside
L6/driver policy. `BioPlan`, ext4, bdev-fs, PageSlot, journal, resident root,
and range reservations never select or own a shard. Do not add a public
`submit_on_queue` filesystem/device API. Create
`tools/io-submission-multiqueue.json` as a matched comparison config whose
baseline role is the accepted single-queue P8 candidate and whose candidate
role changes only the L6/driver multi-queue selection.

- [ ] **Step 7B: Run shard correctness and matched A/B gates**

```bash
cargo test -p tx-subsystems --lib block_queue_shards_ -- --nocapture
cargo test -p tx-subsystems --lib io_manager_block_queue_barrier -- --nocapture
cargo test -p tx-subsystems --lib io_manager_block_queue_allocates_tags_and_completes_by_tag -- --nocapture
cargo test -p tx-ext4 --lib -- --nocapture
cargo test -p tx-fs --lib bdevfs -- --nocapture
cargo -q xtask unit
python3 tools/io-submission-perf.py collect \
  --config tools/io-submission-multiqueue.json \
  --out target/perf/io-submission/p9-matched
python3 tools/io-submission-perf.py evaluate \
  --baseline target/perf/io-submission/p9-matched/baseline \
  --candidate target/perf/io-submission/p9-matched/candidate \
  --out target/perf/io-submission/p9-final/acceptance-receipt.json
python3 tools/io-submission-perf.py verify \
  --receipt target/perf/io-submission/p9-final/acceptance-receipt.json
```

Evaluate at least five interleaved single-queue/sharded pairs. Land the
candidate only when median throughput improves by at least 15%, p99 regresses
by no more than 5%, trace loss does not increase, and correctness receipts are
identical. Otherwise discard the code candidate, record the negative result,
and retain one queue. Never rerun ext4 Tier 1 merely to evaluate a rejected
sharding candidate.

- [ ] **Step 8B: Commit an accepted sharding candidate**

```bash
git add crates/tx-subsystems/src/io_manager/block \
  crates/tx-subsystems/src/device.rs \
  crates/tx-drivers/src/virtio/mmio.rs crates/tx-drivers/src/virtio/blk.rs \
  tools/io-submission-perf.py tools/tests/test_io_submission_perf.py \
  tools/io-submission-multiqueue.json \
  docs/progress/research/2026-08-05-io-submission-performance-baseline.md \
  docs/progress/plans/2026-08-05-io-submission-manager-performance.json \
  docs/progress/STATUS.md
git commit -m "perf(io): shard saturated block queues"
```

## Task 19: Close Active Contracts And Progress State

**Files:**

- Modify: `docs/design/05_filesystem/IO_MANAGER_v1.md`
- Modify: `docs/design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md`
- Modify: `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- Modify: `docs/design/06_devices/DEVICE.md`
- Modify: `docs/design/05_filesystem/EXT4_LIFECYCLE_v1.md`
- Modify: `docs/progress/research/2026-08-05-io-submission-performance-baseline.md`
- Modify: `docs/progress/plans/2026-08-05-io-submission-manager-performance.json`
- Modify: `docs/progress/STATUS.md`

- [ ] **Step 1: Run the closure assertion to verify RED**

```bash
jq -e '
  .status == "complete" and
  ([.steps[].status] | all(. == "complete")) and
  ([.verification[].status] | all(. == "passed" or . == "skipped"))
' docs/progress/plans/2026-08-05-io-submission-manager-performance.json
```

Expected: FAIL because the plan remains active and at least the final-closeout
stage is pending.

- [ ] **Step 2: Reconcile active docs with selected production behavior**

Document only paths proven by the accepted receipts: sole L4/L6 ownership,
domain split, resident publication reader/writer rules, multi-page settlement,
limit-aware block lowering, bounded readahead, and either accepted multi-queue
or explicit single-queue selection. Remove temporary staging/fallback language
only where production callsite scans prove it is unreachable. Keep full ext4
direct I/O, concurrent JBD2 transactions, delayed allocation, and complex
schedulers out of scope.

- [ ] **Step 3: Record final immutable evidence**

Update the research note and STATUS with revision and dirty declaration,
correctness/performance receipt paths and SHA-256 values, selected config,
threshold results, trace-loss facts, the P9 decision, verification commands,
next owner, and any residual blocker. Do not copy mutable `target/` artifacts
into docs.

- [ ] **Step 4: Close every completed JSON stage**

Set all implemented stages to `complete`, set `final-closeout` complete last,
then set top-level status and the actual close date. For example, after the
stage notes and verification entries are complete:

```bash
UPDATED="$(date +%F)"
jq --arg updated "$UPDATED" \
  '.status = "complete" | .updated = $updated' \
  docs/progress/plans/2026-08-05-io-submission-manager-performance.json \
  > docs/progress/plans/2026-08-05-io-submission-manager-performance.json.tmp
mv docs/progress/plans/2026-08-05-io-submission-manager-performance.json.tmp \
  docs/progress/plans/2026-08-05-io-submission-manager-performance.json
```

Append exact final verification entries. A failed required gate keeps the plan
`active` or `blocked`; do not close it from prose alone. P9 may be complete
with a measured negative result when its notes bind the accepted receipt.

- [ ] **Step 5: Run the final bounded verification matrix**

```bash
cargo -q xtask unit
cargo xtask check
cargo xtask observe-schema codegen --check
cargo xtask observe-schema check
cargo xtask observe-discipline
cargo xtask lint docs
cargo xtask lint invariants
cargo xtask progress validate
git diff --check
```

Expected: all required gates pass; any warning-only lint is named in STATUS;
the progress validator reports no overlapping active write claim.

- [ ] **Step 6: Re-run the closure assertion and commit**

```bash
jq -e '
  .status == "complete" and
  ([.steps[].status] | all(. == "complete")) and
  ([.verification[].status] | all(. == "passed" or . == "skipped"))
' docs/progress/plans/2026-08-05-io-submission-manager-performance.json
git add docs/design/05_filesystem/IO_MANAGER_v1.md \
  docs/design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md \
  docs/design/03_memory-vm/PAGE_BACKED_v1.md \
  docs/design/06_devices/DEVICE.md \
  docs/design/05_filesystem/EXT4_LIFECYCLE_v1.md \
  docs/progress/research/2026-08-05-io-submission-performance-baseline.md \
  docs/progress/plans/2026-08-05-io-submission-manager-performance.json \
  docs/progress/STATUS.md
git commit -m "docs(io): close submission manager rollout"
```

## Dependency Order And Execution Discipline

Execute Tasks 1 through 19 in order. Tasks 5/6, 7/8, 10/11/12, and 13/14 are
separate commits inside one rollout stage because each changes custody or
rollback boundaries. Do not parallelize tasks that edit the same owner domain.

Before each task, claim only that task's write set in
`docs/progress/plans/2026-08-05-io-submission-manager-performance.json`; after
its commit, update that stage with verification and the next dependency. A
rejected admission or failed A/B must leave the previous owner/config selected.
Never keep two live mutation or I/O authorities as a compatibility fallback.

Use focused host/model/SMP and short guest witnesses through Task 16. Run the
fresh full ext4 Tier 1 campaign exactly once at Task 17 for the final candidate;
on failure preserve artifacts and resume from the first failed cut. Task 18 is
evidence-gated and may complete without kernel code. Task 19 closes the plan
only after immutable correctness and performance receipts verify.
