# ext4 Lifecycle and Tier 1 Correctness

<!-- txdoc:05-FILESYSTEM-EXT4-LIFECYCLE-V1 -->

**Status.** Active v1 correctness contract (2026-07-30). This document
specializes the ownership and mutation rules in
[`MEMORY_IO_ARCHITECTURE_v1.md`](../03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md),
[`IO_MANAGER_v1.md`](IO_MANAGER_v1.md), and
[`TX_EXT4_PLAN_v1_2.md`](TX_EXT4_PLAN_v1_2.md) into three owner-specific
lifecycle primitives for the bounded Tier 1 ext4 profile.

**Readiness.** The architecture contract is ready for implementation. The
current production implementation is not accepted until the gates in section
9 pass on the same candidate revision. Host plan, codec, or command-contract
tests do not substitute for QEMU, crash/replay, and offline `e2fsck -fn`
evidence.

## 1. Decision and scope

<!-- txdoc:EXT4-LIFECYCLE-DECISION-1 -->

The Tier 1 implementation uses exactly three lifecycle primitives:

1. `OwnedFileIoRequest` owns one PageBacked request from preparation through
   exactly one terminal settlement.
2. `MutationHandle` owns one admitted ext4 mutation, including its frozen
   inputs, allocator claims, journal reservation, graph custody, and
   durability phase.
3. `MountSettlementOp` drives a file, mount, or detach frontier through the
   same transaction and checkpoint machinery.

These are owner-specific primitives, not instances of a universal cleanup
framework. The design does not add a cleanup service, callback registry,
public type for every internal phase, or a second I/O graph. Internal enums
may describe phase, but ownership transfer occurs only through the three
primitive boundaries.

Tier 1 favors correctness over concurrency:

- one mounted instance admits at most one active mutation;
- an admitted namespace or setattr mutation, and any durability operation,
  reaches durable commit, checkpoint, cache refresh, and terminal settlement
  before publication or syscall success; an ordinary buffered write may still
  return after PageSlot dirty publication and joins a later settlement frontier;
- a post-commit settlement failure moves the mount into `RecoveryOnly`, where
  normal reads and mutations fail until retry or remount replay establishes one
  coherent view;
- the implementation does not need a general committed-metadata overlay or a
  concurrent checkpoint queue;
- a later concurrent implementation must remain behind the same three public
  lifecycle boundaries.

Waiting for checkpoint is stronger and slower than Linux ext4's normal
commit-only `fsync` completion. It is valid for the Tier 1 compatibility
profile and removes an additional metadata-authority domain while correctness
is being established. Performance is not a Tier 1 correctness blocker.

## 2. Ownership model

<!-- txdoc:EXT4-LIFECYCLE-OWNERSHIP-1 -->

| Owner | Long-lived authority | May temporarily own | Must not own |
|---|---|---|---|
| PageBacked / `PageSlot` | ordinary file-page identity, resident state, dirty/writeback generation, page errors | an issued `PageDataLease` inside one `OwnedFileIoRequest` until transfer | ext4 layout, journal state, block queue state |
| Pure ext4 planner | deterministic layout rules and immutable plan values | no kernel resource | page/frame lifetime, reservation, I/O execution |
| Mounted ext4 runtime | mutation admission, metadata generations, allocator claims, journal ring, transaction state, mount `errseq` | child request IDs, `FrozenMetadataToken`, `JournalExtentToken`, and `AllocatorClaimToken` | a `PageDataLease` already owned by a child request; a second ordinary file-data cache |
| I/O manager L4/L6 | admitted request/graph custody, tags, queues, barriers, completion routing | complete child request bundles transferred with a graph | dirty policy, ext4 semantics, post-completion data retention |
| Block device / HAL | device capability, DMA descriptors, hardware completion | submitted buffers | filesystem completion or publication state |

An ownership transfer is atomic with respect to the source authority. A source
must not clear its state before the destination has accepted the complete
resource bundle. A destination must return one owned terminal bundle on both
success and failure. An errno without the bundle is not a terminal result.

Guards, witnesses, borrowed reservations, and locks never cross a yield or
device completion. A lifecycle object carries owned IDs, generation facts, and
capabilities; a resumed step reacquires and revalidates borrowed state.

The lifecycle objects and their owned tokens are move-only payload capabilities
stored by existing PageBacked request tables or the existing mounted ext4
payload. They are not new identity entities, zone rows, or globally discoverable
objects.

## 3. `OwnedFileIoRequest`

<!-- txdoc:EXT4-LIFECYCLE-OWNED-FILE-IO-REQUEST-1 -->

`OwnedFileIoRequest` replaces caller-managed combinations of writeback abort,
read-target release, queue failure, resume failure, and stale completion
cleanup. Its internal resource bundle contains, as applicable:

- request, mount, filesystem-object, range, and submitted-generation facts;
- the `PageDataLease` or fetch target that keeps payload memory live;
- rollback authority for a prepared but unsubmitted PageSlot transition;
- the admitted completion route; and
- an error slot that can be transferred to the page, file, or mount `errseq`.

The conceptual interface is deliberately small:

```rust,ignore
impl OwnedFileIoRequest {
    fn submit(self, graph: BackendBioGraph) -> Result<RequestId, SubmitFailure>;
    fn finish(self, result: FileIoTerminalResult) -> SettledFileIo;
}

struct SubmitFailure {
    request: OwnedFileIoRequest,
    error: SubmitError,
}
```

The exact Rust API may use owner-table IDs rather than return the in-flight
object to the caller. `SubmitFailure` owns the consumed request bundle; its
only exits are `finish` or transfer into the authoritative owner table. The
following semantics are mandatory:

| Phase | Event | Terminal action |
|---|---|---|
| prepared | planning or admission failure | restore the matching PageSlot generation and release the complete bundle |
| prepared | graph accepted | transfer the complete bundle into the authoritative request table |
| in flight | device success or failure | produce a complete terminal bundle, never a bare status |
| terminal | generation matches | update PageSlot, preserve redirty, record error, release resources |
| terminal | completion is stale | do not mutate the newer generation; still release the old bundle |
| nonterminal drop or corrupt route | owner detects loss | poison the owner and enqueue a synthetic failed terminal result |

`Drop` performs no asynchronous I/O. It may only assert in tests, mark an
owner poisoned, and enqueue already-owned terminal work. Normal code must
consume the object through `submit` or `finish`.

Only the PageBacked lifecycle module may invoke the low-level PageSlot abort,
completion, lease release, or read-target release operations. Those operations
are private implementation details and static lint rejects production callers
outside the module.

## 4. `MutationHandle`

<!-- txdoc:EXT4-LIFECYCLE-MUTATION-HANDLE-1 -->

The pure planner first produces a `PlannedMutation` value. It includes the
complete metadata after-images, immutable JBD2 revoke set, and deferred-free
claims whose blocks cannot be reused until replay is safe. Admission validates
the frozen read set and consumes the resources needed to create one
`MutationHandle`. The handle owns:

- mutation origin, mount identity, affected filesystem objects, and sequence;
- immutable metadata read-set versions and complete home-block after-images;
- child `OwnedFileIoRequest` IDs and their terminal dependencies, not their
  `PageDataLease` bundles;
- an owned `FrozenMetadataToken` plus independent `JournalRecordLease` values
  for descriptor, revoke, escaped metadata, and commit bytes;
- owned `AllocatorClaimToken` values, including deferred-free claims;
- one owned `JournalExtentToken` covering the admitted ring extent;
- a not-yet-admitted `BackendBioGraph` or the authority to build it once; after
  graph admission the handle retains only child IDs and terminal dependencies;
- phase-local error and retry state.

The internal phase enum has these semantic states:

```text
Admitted
  -> Prepared
  -> CommitPending
  -> CommittedNeedsSettlement
  -> CheckpointPending
  -> TailReclaimPending
  -> Settled

Prepared/CommitPending
  -> AbortRequested
  -> AbortDraining
  -> RolledBack

CommitPending
  -> CommitUnknown
  -> RecoveryOnly
```

The enum is not a public family of typestate structs. The handle exposes only
admission, bounded `StepOp` driving, and pre-commit abort. Every drive result
retains or transfers the same owned handle; callers never perform phase cleanup.

Admission may use local rollback guards, but before the first yield it consumes
them into cross-yield-safe owned tokens or rolls them back. `MutationHandle`
stores only owned tokens and IDs. It never stores a reservation guard,
borrowed allocator claim, witness, `IdentRef`, or epoch guard across a yield.

### 4.1 Pre-commit failure

<!-- txdoc:EXT4-LIFECYCLE-PRECOMMIT-FAILURE-1 -->

Read-set conflict, unsupported shape, allocation failure, checksum failure, or
graph validation failure before graph transfer performs an immediate rollback.
After any child graph node has been submitted, a failure first enters
`AbortRequested` and then `AbortDraining`: new nodes are not submitted, but all
submitted child requests retain their buffers until their terminal completions
arrive. Only then does one rollback:

1. discard unpublished after-images;
2. release or reverse allocator claims;
3. restore PageSlot generations through their owned request terminalizers;
4. release the `JournalExtentToken`; and
5. publish no namespace or inode metadata result.

The rollback consumes the handle. A partially released handle cannot be
returned to a caller. A known pre-commit error therefore describes transaction
durability, not permission to free DMA-visible or in-flight request resources.

### 4.2 Commit and post-commit failure

<!-- txdoc:EXT4-LIFECYCLE-POSTCOMMIT-FAILURE-1 -->

Once durable commit is confirmed, the mutation enters
`CommittedNeedsSettlement` and cannot be rolled back. A checkpoint failure
retains the handle, after-images, deferred frees, and journal extent and
retries through the mount owner. It records a mount error but does not discard
the committed transaction.

If commit completion is ambiguous, the handle enters `CommitUnknown` and the
mounted instance enters `RecoveryOnly`. The mount rejects new mutations and
normal reads, records recovery-required state, and retains enough journal state
for retry or remount replay. It must not report a clean filesystem or release
the journal extent as though commit had failed before submission.

For Tier 1, an unrecoverable checkpoint or flush error also keeps the mount in
`RecoveryOnly`. The initiating syscall returns `EIO` if settlement cannot
finish; a later replay may expose the durably committed operation. That
post-commit ambiguity is explicit and is never reported as a pre-commit
rollback. Normal VFS access resumes only after retry or replay, checkpoint,
cache refresh, and publication establish one coherent view.

## 5. Durability sequence

<!-- txdoc:EXT4-LIFECYCLE-DURABILITY-SEQUENCE-1 -->

Before the first Tier 1 RW mutation, mount admission durably sets the ext4
recovery-required state and establishes an active journal. That state remains
set for the entire RW mount lifetime. An ordinary transaction must not mark the
filesystem or journal globally clean.

The ordered per-transaction sequence is:

```text
ordered data writes
  -> device flush
  -> JBD2 descriptor, journal metadata, and revoke writes
  -> device flush
  -> JBD2 commit write with FUA
  -> post-commit flush when FUA is unsupported or not guaranteed
  -> metadata home-block checkpoint writes
  -> device flush
  -> journal-tail update
  -> tail FUA or flush
  -> ring-extent reclamation
  -> format-cache refresh/invalidation
  -> terminal settlement and publication
```

The block adapter must consume write flags. A production implementation may
not inherit a successful no-op barrier. Device admission records whether FUA
is supported and reliable; otherwise the graph contains the explicit flush
fallback. Tests assert the submitted device trace, not only graph metadata.

The `JournalExtentToken` is released only after checkpoint durability, safe
journal-tail advancement, and required cache refresh complete. Before that
point it belongs to the `MutationHandle`, including on retryable failure.

A freed block remains covered by the immutable revoke set and a deferred-free
claim. The allocator may reuse it only after the freeing transaction is
checkpointed and journal-tail advancement proves that no older replayable
transaction can write stale metadata onto the block. Truncate, unlink,
rename-overwrite, and orphan cleanup tests cut power before and after revoke,
commit, checkpoint, tail advancement, and first allowed reuse.

Filesystem clean state is a detach/remount-RO operation only. After every
transaction and child request drains, detach performs final checkpoint and
tail reclamation, flushes, clears recovery-required state, performs a final
FUA or flush, and then permits topology teardown.

## 6. `MountSettlementOp`

<!-- txdoc:EXT4-LIFECYCLE-MOUNT-SETTLEMENT-1 -->

`MountSettlementOp` is the only durability entry used by `fsync`, `fdatasync`,
`syncfs`, `sync`, and `umount`. It carries one of three scopes:

```rust,ignore
enum SettlementScope {
    File {
        object: FsObjectId,
        generation_frontier: u64,
    },
    Mount {
        transaction_frontier: u64,
    },
    Detach,
}
```

Semantics:

- file scope freezes the target file's dirty and transaction frontier;
- mount scope freezes the current mount frontier, while later mutations form a
  later frontier;
- `sync()` drives mount scope for each mount visible at call time;
- detach scope changes the mounted instance to `Quiescing`, rejects new
  admission, drains every transaction and checkpoint, advances the tail,
  writes clean state,
  performs the final flush, and only then removes mount topology;
- `MNT_FORCE` is not a Tier 1 operation and returns the mount contract's
  unsupported result;
- `MNT_DETACH` may withdraw topology first, but `MountPayload` remains in
  `DetachedPending` and owns every pin, request, and token until ordinary
  quiesce and terminal settlement complete. Lazy topology withdrawal is not a
  clean-state claim.

The mounted instance has the semantic states `Open`, `Quiescing`,
`RecoveryOnly`, `DetachedPending`, and `Detached`. Namespace code does not
change those states directly.

Page, file, and mount errors are sequence-numbered. Redirty and retry do not
erase an older writeback, journal, checkpoint, flush, or device error. Each
open file stores an observer cursor initialized at open; the mount stores a
cursor for mount-scoped reporting. A reporting syscall advances its cursor
only after observing the error. Durability frontiers and error cursors are
separate facts.

### 6.1 Syscall projection

<!-- txdoc:EXT4-LIFECYCLE-SYSCALL-PROJECTION-1 -->

| Syscall | Tier 1 projection |
|---|---|
| regular-file `fsync` | settle file data and all required metadata through checkpoint; report file/mount errors through the open-file cursor |
| directory `fsync` | settle namespace mutations involving the directory through checkpoint |
| `fdatasync` | initially use the stronger `fsync` settlement; later metadata minimization must not change the lifecycle boundary |
| `syncfs` | settle the captured mount frontier and report through the mount cursor |
| `sync` | settle all visible mount frontiers; retain Linux's no-error return contract while recording failures in mount error state |
| normal `umount` | quiesce, settle, write clean state, flush, then detach; return an error on failure |
| `MNT_DETACH` | withdraw topology, retain the payload until asynchronous ordinary settlement finishes |
| `MNT_FORCE` | unsupported in Tier 1 |
| `msync` | not promoted by this ext4 Tier 1 contract; PageBacked cleanup may still migrate internally before Tier 2 behavior is admitted |

## 7. Tier 1 capability boundary

<!-- txdoc:EXT4-LIFECYCLE-TIER1-CAPABILITY-1 -->

The lifecycle contract applies to the pinned 4 KiB Tier 1 image profile with
discovered ordered JBD2 and `metadata_csum`. The capability ledger is the
machine-readable authority for exact feature bits and shape bounds.

Tier 1 production operations are:

- mount admission, required replay, read, lookup, and directory traversal;
- buffered write and coherent file-backed mmap;
- create, mkdir, link, symlink, unlink, rmdir, and rename;
- truncate, chmod, chown, and timestamp updates;
- classic-orphan handling for unlinked-open and crash-truncate cases; and
- regular-file and directory fsync, fdatasync, syncfs, sync, lazy detach, and
  clean normal unmount.

Each persistent operation constructs one complete mutation covering all
affected inode, extent, directory, bitmap, group-descriptor, superblock,
accounting, checksum, and orphan after-images. Multiple logical changes to one
home block merge before admission.

Tier 1 does not admit arbitrary-depth extent growth, htree split or rebalance,
`orphan_file`, xattr, ACL, quota, fallocate, direct I/O, DAX, fast commit, or
Tier 2 feature shapes. Unsupported work fails before reservation, PageSlot
mutation, VFS publication, or home-block write with the capability ledger's
declared errno.

## 8. Production surface convergence

<!-- txdoc:EXT4-LIFECYCLE-PRODUCTION-CONVERGENCE-1 -->

| Current surface | Required convergence |
|---|---|
| PageBacked submission/resume error branches | route through `OwnedFileIoRequest` terminal settlement |
| raw writeback abort and read-target release | make private to the PageBacked lifecycle module |
| direct pager bitmap/inode/directory/extent mutation | retain only in host tests and explicit compatibility oracles |
| legacy `step_fsync` and duplicate fsync wrappers | route fsync/fdatasync to file-scope `MountSettlementOp`; keep ext4 msync behavior outside Tier 1 |
| caller-managed journal `discard` and ring completion | make private to `MutationHandle` |
| ignored write FUA flags and default successful barriers | implement capability-aware FUA/flush or reject production admission |
| replay based only on journal position | gate on ext4 recovery state, validate transaction checksums, and refresh caches |
| mount-table-only unmount | detach only after detach-scope settlement succeeds |
| successful no-op `sync()` | enumerate visible mounts and drive mount-scope settlement |
| legacy or compatibility RW mount constructors | remove from production selection; keep one discovered-journal RW path |

Static checks reject new production calls to format-level direct home writes,
raw lifecycle cleanup operations, and legacy RW admission outside their
declared test/oracle modules.

## 9. Acceptance gates

<!-- txdoc:EXT4-LIFECYCLE-ACCEPTANCE-1 -->

| Gate | Evidence | Exit criterion |
|---|---|---|
| G0 boundary | static ownership and direct-write lints | zero production bypasses |
| G1 lifecycle | failure at every request and mutation phase; duplicate and stale completion | each resource reaches exactly one terminal owner action |
| G2 format | after-image and checksum fixtures for every Tier 1 mutation | every produced image passes offline `e2fsck -fn` |
| G3 durability | recorded block-device write, flag, and flush trace | trace matches section 5 and never ignores FUA |
| G4 recovery | hard kill at every durability boundary, remount, replay | fsync-success data survives and no image needs repair |
| G5 guest | fresh SCRATCH operation matrix in this section | every declared Tier 1 operation and post-run `e2fsck -fn` pass |
| G6 Linux | pinned Tier 1 xfstests manifest | no selected case is timeout, skipped, or not run |
| G7 unmount | clean and injected-error detach campaigns | clean detach needs no replay; failures return and retain recovery state |

Development slices may use a reduced deterministic cut set. Product promotion
uses the `TX_EXT4_PLAN_v1_2.md` requirement of 1000 deterministic crash cuts
and runs offline `e2fsck -fn` against an immutable copy of every resulting
image.

The planned canonical host inputs are:

| Artifact | Planned path | Schema ID |
|---|---|---|
| Tier 1 capability ledger | `tools/ext4/tier1/capability-ledger.json` | `tx.ext4.capability_ledger.v1` |
| selected xfstests ledger | `tools/ext4/tier1/xfstests-selection.json` | `tx.ext4.xfstests_selection_ledger.v1` |
| crash-cut catalog | `tools/ext4/tier1/crash-cuts.json` | `tx.ext4.crash_cut_catalog.v1` |
| immutable run receipt | `target/ext4/tier1/<run-id>/acceptance-receipt.json` | `tx.ext4.tier1_acceptance_receipt.v1` |

These paths and schemas are implementation targets, not claims about the
current command surface. Planned G0 lint rules are
`ext4-lifecycle-ownership`, `ext4-no-direct-home-write`, and
`ext4-durability-flags`.

The stable durability cutpoint families are:

```text
D0  before ordered-data submission
D1  ordered data complete, before its flush
D2  after ordered-data flush
D3  partial descriptor/metadata/revoke journal body
D4  journal body complete, before its flush
D5  after journal-body flush, before commit
D6  commit submitted with unknown completion
D7  commit durable, before checkpoint
D8  partial checkpoint
D9  checkpoint durable, before tail advancement
D10 tail advanced, before cache settlement/publication
D11 detach drained, before clean-state write
D12 clean-state write complete, before final flush
```

Each free-capable mutation adds cuts around revoke durability and first block
reuse. The catalog expands each family into operation and device-failure cases
without changing these stable IDs.

The G5 guest operation matrix covers:

| Family | Required operations |
|---|---|
| data | read, buffered write, sparse growth, truncate, writable mmap followed by fsync |
| inode metadata | chmod, chown, utimens |
| namespace | create, mkdir, link, symlink, rename within/across directories, rename-overwrite, unlink, rmdir |
| orphan | unlinked-open close, crash-truncate, replay cleanup |
| durability | regular-file fsync, directory fsync, fdatasync, syncfs, sync, normal unmount, lazy detach lifetime |
| execution | remount and execute a generated file from SCRATCH |

One target public host entry performs the product sequence. The command does
not exist in the current checkout until its implementation step lands:

```text
cargo xtask ext4 tier1
  -> build candidate
  -> create fresh TEST/SCRATCH/WORKLOAD images
  -> run QEMU Tier 1 workload
  -> execute deterministic crash cuts and replay
  -> collect immutable image copies
  -> run e2fsck -fn
  -> run the pinned Tier 1 xfstests manifest
  -> emit one versioned acceptance receipt
```

Internal preparation DTOs and scripts may remain as modules, but users and CI
do not manually assemble provision, bundle, availability, shell-environment,
contract, metadata, cleanup, and result steps. A host-only `RunWorkspace`
owns temporary directories, image copies, child processes, and artifact
finalization. It is not reused as a kernel transaction primitive.

## 10. Implementation entry order

<!-- txdoc:EXT4-LIFECYCLE-IMPLEMENTATION-ORDER-1 -->

1. Reconcile current HEAD with verified ext4 worktree slices and correct the
   progress ledger; do not merge a worktree wholesale.
2. Add the three lifecycle primitives and their failure-injection tests.
3. Migrate PageBacked submission, resume, queue failure, completion, fsync, and
   fdatasync to the single request terminalizer; internal msync cleanup may
   converge without promoting ext4 msync into Tier 1.
4. Move journal reservation, commit uncertainty, checkpoint retry, FUA/flush,
   replay, and cache refresh behind `MutationHandle`.
5. Migrate Tier 1 setattr, data/extent/truncate, namespace, and classic-orphan
   mutations one vertical slice at a time.
6. Cut production to the single discovered-journal path and connect all sync
   and unmount entry points to `MountSettlementOp`.
7. Collapse the public acceptance tool to the single Tier 1 entry and close
   G0-G7 in dependency order.

An implementation step is not complete merely because its types, host unit
tests, or command DTOs exist. Status reporting distinguishes primitive
foundation, migrated function, production wiring, and crash/e2fsprogs product
acceptance.

## 11. Deferred optimizations

<!-- txdoc:EXT4-LIFECYCLE-DEFERRED-OPTIMIZATIONS-1 -->

The following changes are deliberately deferred until Tier 1 acceptance:

- multiple concurrently running or committing transactions;
- background checkpoint concurrent with later admitted mutations;
- publication after commit using a committed-metadata overlay;
- cross-container writeback clustering and performance policy;
- Tier 2 ext4 shapes and features; and
- native RV64 rustc performance promotion gates.

Those changes may replace internal scheduling and phase data. They must not
bypass or duplicate `OwnedFileIoRequest`, `MutationHandle`, or
`MountSettlementOp`.
