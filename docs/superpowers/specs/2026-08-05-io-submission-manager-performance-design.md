# I/O SubmissionManager And File-Data Performance Design

**Status:** conversational design approved; written-spec review pending
(2026-08-05)

## Decision Summary

This design completes the ownership and performance program already defined by
[`IO_MANAGER_v1.md`](../../design/05_filesystem/IO_MANAGER_v1.md) and
[`MEMORY_IO_ARCHITECTURE_v1.md`](../../design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md).
It makes six coupled changes in one dependency-ordered rollout:

1. extract one independent L4 `PageIoSubmissionManager` and a device-scoped L6
   `BlockSubmissionManager` from `PageContainerState`;
2. split resident publication, page-state indexing, and logical range
   reservations into independent ownership and lock domains;
3. replace the locked resident `BTreeMap` with a persistent
   `Published<ResidentRoot>` whose readers do not take PageContainer or manager
   locks;
4. connect real multi-page reads, extent-aware clustering, device-limit-aware
   BIO splitting, bounded plugging, and adaptive file-data readahead;
5. make the data plane observable through trace-backed correctness and
   performance receipts; and
6. permit block multi-queue sharding only when the single-queue receipt proves
   that queue ownership is the bottleneck and an A/B candidate passes the
   declared gate.

The approved order is normative:

```text
correctness/ledger repair
  -> trace baseline
  -> L4 manager extraction
  -> L6 manager + device capabilities
  -> PageSlot/range lock-domain split
  -> Published<ResidentRoot>
  -> multi-page read/completion
  -> extent clustering + BIO split + short plugging
  -> readahead
  -> performance receipt
  -> conditional multi-queue
```

RCU is deliberately narrow. It publishes only immutable resident roots and
stable resident bindings. `PageSlot` state transitions, range reservations,
manager queues, tags, DMA leases, completion aggregation, and journal state
remain serialized by their semantic owners.

## Authority And Current Baseline

The active authorities are:

- `txdoc:IO-MANAGER-OWNERSHIP-RULE-1`,
  `txdoc:IO-MANAGER-L4-PAGE-SUBMISSION-1`,
  `txdoc:IO-MANAGER-L6-BLOCK-SUBMISSION-1`, and
  `txdoc:IO-MANAGER-PC-SYNC-BOUNDARY-1` in
  [`IO_MANAGER_v1.md`](../../design/05_filesystem/IO_MANAGER_v1.md);
- `txdoc:MEMORY-IO-IMPLEMENTATION-FOUNDATION-1`,
  `txdoc:MEMORY-IO-IMPLEMENTATION-DATA-PLANE-1`, and
  `txdoc:MEMORY-IO-PERFORMANCE-1` in
  [`MEMORY_IO_ARCHITECTURE_v1.md`](../../design/03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md);
- the durability and terminal-settlement rules in
  [`EXT4_LIFECYCLE_v1.md`](../../design/05_filesystem/EXT4_LIFECYCLE_v1.md);
  and
- the existing persistent-publication implementation in
  [`publication/mod.rs`](../../../crates/tx-substrate/src/publication/mod.rs)
  and its VM pilot in
  [`recipe.rs`](../../../crates/tx-subsystems/src/vm/structure/recipe.rs).

The current executable path is staging, not the target ownership model:

- [`PageContainerState`](../../../crates/tx-subsystems/src/page_backed/mod.rs)
  owns the resident `PageCacheIndex`, slot index, in-flight fetches,
  `PageService`, owned requests, background graphs, per-file block runtime,
  range reservations, and direct-I/O state under one state cell;
- `PageDataLease` can retain more than one writeback page, and
  `PageIoRequest` already carries `PageIoRange`, but normal read ownership and
  terminal installation still retain one `CachedFrame`;
- `BlockQueue`, `QueueDepth`, `BlockTagTable`, graph execution, adjacent merge,
  and service futures run today, but block runtime is still embedded per
  PageContainer rather than owned once per device;
- `BlockDevice` reports capacity, block size, and durability capabilities, but
  has no typed maximum transfer, SG, alignment, segment-boundary, or hardware
  queue limits; and
- [`sys_readahead()`](../../../crates/tx-shims/src/linux_syscall/io.rs) checks
  the descriptor and file kind, then returns success without admitting any
  page-cache work.

The completed ext4 Tier 1 receipt is the durability baseline. It is not a
performance receipt and does not by itself prove that a newly extracted data
plane preserves production `fsync` routing. Before the performance path can
become the default, the live `FsPageBacking::fsync_file` path must not return
`ENOSYS` for a supported RW mount, and mount settlement must snapshot a
monotonic frontier derived from admitted transactions rather than silently
substituting a default frontier. This targeted gate does not require replaying
the completed six-hour crash prefix at every stage.

## Goals

The rollout must provide:

- independent, typed custody boundaries for L4 page work and L6 device work;
- one terminal settlement for every successfully admitted page request and no
  settlement for a rejected request whose ownership was returned;
- resident-hit lookup without the PageContainer state lock or either manager;
- real multi-page read targets and per-page generation-checked completion;
- filesystem-owned logical-to-physical mapping followed by generic BIO
  splitting and compatible adjacent merge;
- conservative adaptive readahead that installs only PageContainer residents;
- bounded queueing, plugging, completion, retry, and observation work;
- zero normal-path ordinary payload copies and zero unexplained bounce bytes;
  and
- immutable evidence that ties performance claims to source revision, image,
  geometry, toolchain, workload, traces, and loss accounting.

## Non-Goals

This program does not add:

- a second ordinary file-data cache in ext4 or the I/O manager;
- full ext4 `O_DIRECT` product admission or update-in-place cache coherence;
- multiple simultaneous JBD2 mutation transactions, delayed allocation, group
  commit, directory indexing, or a new journal graph;
- BFQ, Kyber, deadline trees, or another complex block scheduler;
- automatic VM PTE prefault as a consequence of file-data readahead;
- RCU conversion of `PageSlot`, range reservations, queues, tags, DMA leases,
  completion trackers, or journal state;
- multi-queue as an unconditional milestone; or
- a second `BackendBioGraph`, page-request family, lease family, or completion
  graph alongside the existing canonical values.

The existing generic direct-I/O custody and range-coherency scaffolding must
continue to compile and pass its focused tests. Ext4 product-level direct I/O
remains a separate admission program.

## Canonical Gate

The names and dispositions in this table are normative for the implementation
plan. A proposed item not listed here must either be a private helper derived
directly from one row or return for written design review.

| Proposed item | Canonical source | Current live seam | Decision and landing gate |
| --- | --- | --- | --- |
| `ResidentDomain` | `IO-MANAGER-PC-SYNC-BOUNDARY-1` | `PageCacheIndex` inside `PageContainerState` | Accept as the owner of `Published<ResidentRoot>` plus one bounded writer lock. It owns no slot FSM, range table, or I/O queue. |
| `ResidentRoot` | `IO-MANAGER-PC-SYNC-BOUNDARY-1`; VM `RecipeTree` pilot | locked `BTreeMap<PageIndex, PageCacheEntry>` | Accept as an immutable persistent sparse root. An update path-copies only touched nodes; cloning the full map per mutation is forbidden. |
| `ResidentBinding` | PageBacked frame ownership and resident-withdrawal rules | mutable `PageCacheEntry` under the PC lock | Accept as the stable, root-retained binding of page index, generation, frame retention, and active/withdrawn publication state. It does not duplicate dirty/writeback authority. |
| `PageStateDomain` | PageSlot-only semantic authority | `file_page_slots` under `PageContainerState` | Accept as a slot-index lock plus stable per-page `PageSlot` cells. Slot FSM locks remain mutable and are not RCU readers. |
| `RangeDomain` | range semantic exclusion in `IO_MANAGER_v1.md` | `RangeReservationTable` under `PageContainerState` | Accept as an independent range-table owner/lock. Reserve/release remains serialized and no reservation lock crosses yield or I/O. |
| strengthened `Published::prepare_replace` | publication substrate and VM pilot | prepare allocates the next root, while `commit()` may perform an unbounded retire drain | Accept only after prepare reserves bounded retirement capacity or returns explicit backpressure. A successful prepare makes commit allocation-free, bounded, and infallible. |
| `PageIoSubmissionManager` | `IO-MANAGER-L4-PAGE-SUBMISSION-1` | embedded `PageService`, owned requests, graph and waiter state | Accept as the initial single L4 owner for all PageContainers in one kernel I/O domain. It owns admitted custody, scheduling, graph execution, completion aggregation, readahead state, and waiters. |
| `PageIoSubmissionHandle` | typed manager handle rule | PageContainer directly drives embedded state | Accept as the only PageContainer-to-L4 submission surface. It exposes no manager lock or mutable queue reference. |
| `OwnedPageIoSubmission` | owner admission and resource-bundle transfer | `OwnedFileIoRequest` inserted separately from queue admission | Accept as the atomic admission bundle: request, page data lease/targets, graph/planner continuation, and typed terminal route. Rejection returns the entire bundle. |
| `OwnedPageIoSettlement` | exactly-one terminal settlement rule | `FileIoTerminalResult` plus request-map removal | Accept as the one consuming terminal bundle returned by L4 to PageBacked. It carries per-page generation/result and all retained resources. |
| direction-neutral multi-page `PageDataLease` | canonical PageBacked lease family | multi-page writeback source but one-frame read target | Extend the existing type; do not add a parallel lease. Each segment records page index, generation, frame range, direction, and retention evidence. |
| `PageIoRequest`, `PageIoRange`, `PageIoCompletion`, `BackendBioGraph` | existing canonical request and graph families | staging values already execute | Extend in place for per-page batch settlement and SG slices. A second request, completion, or graph family is forbidden. |
| `BlockSubmissionManager` | `IO-MANAGER-L6-BLOCK-SUBMISSION-1` | `FileIoBlockRuntime` embedded per PageContainer | Accept as one mutable owner per physical block device. Partitions share it and retain bounds in their handles. |
| `BlockSubmissionHandle` | typed L6 manager handle rule | direct access to block queue/runtime fields | Accept as the only L4/raw-block admission surface. It exposes immutable limits and owned submission, not queue internals. |
| `OwnedBioSubmission` | L6 queue/tag custody | graph nodes and `Bio` values are admitted piecemeal | Accept as the ready-node bundle consumed by L6 admission. Failure returns ownership; success is completed exactly once to the graph owner. |
| `BlockIoLimits` and `BlockDevice::io_limits()` | L6 device-limit ownership | only block size, capacity, flush, and FUA are typed | Accept as an immutable capability snapshot covering transfer, segment, alignment, boundary, queue-depth, and hardware-queue limits. Unknown fields use conservative finite defaults, never unlimited sentinels. |
| `ReadaheadStreamId` | L4 generic readahead ownership | no stream history | Accept as a stable per-open-file token supplied with demand reads. It prevents unrelated readers of one file from sharing sequential history. |
| `ReadaheadHint` and `PageContainer::advise_readahead` | Linux-visible `readahead(2)` plus L4 optional work | syscall validation-only stub | Accept as byte-range-to-page-range advisory admission. Success means the hint was accepted for best-effort processing, not that pages are resident. |
| `IoSubmissionSnapshot` | `IO-MANAGER-COORDINATOR-FEEDBACK-1` | scattered queue/test counters | Accept as immutable bounded L4/L6 facts. It contains no callbacks, owner locks, raw pages, or policy decisions. |
| `tx.io_submission.performance_receipt.v1` | `MEMORY-IO-PERFORMANCE-1` | no I/O-manager performance receipt | Accept as the immutable artifact schema described below. It is evidence, not runtime authority. |
| `BlockQueueShardSet` | measured-sharding rule | one mutable L6 queue/tag/depth owner | Conditional only. It may be implemented after the single-queue saturation gate passes; otherwise it remains absent. |
| `page_backed::{resident,state_domain,range_domain}` modules | PageBacked content/state/range ownership split | responsibilities coexist in `page_backed/mod.rs` | Accept as private owner modules. They may re-export existing public PageContainer APIs but cannot expose locks or add a second cache/index authority. |
| `io_manager::page::{manager,readahead}` modules | L4 ownership and optional-work rules | values in `page/mod.rs`; execution in `page/service.rs` | Accept as the manager owner and policy-mechanics split. Existing request/completion/service values move or re-export; they are not duplicated. |
| `io_manager::block::manager` module plus `device::BlockIoLimits` | L6/device boundary | block staging in `block/mod.rs`; device traits in `device.rs` | Accept. The manager consumes immutable device-owned limits; device code does not acquire manager internals or interpret page requests. |
| `io_manager::runtime::observe` module | coordinator feedback contract | scattered counters | Accept for snapshot/counter definitions only. Receipt serialization and host orchestration remain outside the kernel crate. |

### Target module landing

The initial file split is bounded to the accepted rows above:

```text
crates/tx-subsystems/src/
  page_backed/
    resident.rs
    state_domain.rs
    range_domain.rs
  io_manager/
    page/
      manager.rs
      readahead.rs
    block/
      manager.rs
    runtime/
      observe.rs
  device.rs                    # BlockIoLimits + BlockDevice::io_limits()
```

Existing `request`, `completion`, `service`, `graph`, `queue`, `tag`, and
barrier code is moved or re-exported into these owners as needed. The topology
does not authorize a second request family, graph executor, device registry, or
filesystem adapter. Large-file mechanical splits may precede semantic moves,
but every intermediate commit must retain one live authority.

## Ownership And Synchronization Model

`PageContainer` remains the ordinary file-data owner and contains only stable
identity/configuration plus private domain and manager handles:

```text
PageContainer
  identity, size, backend
  ResidentDomain       -> Published<ResidentRoot> + resident writer
  PageStateDomain      -> slot index + per-slot FSM locks
  RangeDomain          -> logical interval owner
  PageIoSubmissionHandle

PageIoSubmissionManager (L4, I/O-domain scoped)
  admitted PageIoRequest/resource custody
  ready/blocked graph execution
  page completion aggregation and waiters
  readahead stream state and short plugs

BlockSubmissionManager (L6, physical-device scoped)
  immutable BlockIoLimits
  ready BIO queue, depth, tags, barriers
  split/merge accounting, retry and completion
```

The domains are ownership boundaries, not a nested lock hierarchy. Operations
that touch more than one domain use snapshot, release, and generation recheck.
The only permitted short nested section is a resident writer operating on one
already-identified stable binding while no slot-index or range-table lock is
held. No epoch guard, semantic lock, manager lock, or writer claim crosses a
reactor yield, filesystem planning call, device submission, or wait.

`PageSlot` remains the sole authority for `Empty`, `Fetching`, `Resident`,
`Dirty`, `Writeback`, and `Error` transitions and their generations.
`ResidentBinding.active` only answers whether a root observation may acquire a
new map pin for that exact generation. It is publication eligibility, not a
second dirty/writeback state machine.

Initial L4 is one manager, not one manager per PageContainer. Initial L6 is one
manager per physical device, not per file or partition. These choices remove
the current coarse lock domains while avoiding premature sharding. Handles may
be cloned; manager mutable state remains private to its service owner.

## Resident Publication Protocol

### Root and binding shape

`ResidentRoot` is a persistent sparse map from `PageIndex` to a stable
`ResidentBinding`. A binding contains immutable page index, installed
generation, PPN/frame-retention evidence, a reference to the corresponding
stable PageSlot cell, and an atomic `Installing | Active | Withdrawn`
publication state. Old roots retain old bindings through the epoch grace
period; existing `MapPin`s retain their normal allocator/VM lifetime after the
root and binding are retired.

The root does not contain mutable queues, range reservations, LRU lists,
waiters, DMA state, or filesystem mappings. Referenced/aging accounting uses
bounded atomic counters or separate owner feedback and cannot force a resident
hit through a mutation lock.

### Resident hit

The read-side sequence is:

```text
pin epoch guard
  -> load ResidentRoot with Acquire
  -> lookup page and copy the binding observation
  -> require Active and capture generation/PPN
  -> acquire owned MapPin for the observed PPN
  -> revalidate Active, generation, and PPN
  -> return the owned pin/frame view
drop guard before any copy loop that may yield or any I/O
```

If pin acquisition or revalidation fails, the reader drops the pin and retries
through the normal miss/slot path. A reference borrowed from the root never
escapes the epoch guard. The resident-hit path does not inspect the range table
or enter either submission manager.

### Install

Installation is generation checked and prepare-before-publish:

1. complete the fetch into PageBacked-owned frame retention;
2. under the per-slot FSM lock, verify the fetch generation, reserve an
   exclusive internal `Publishing` transition claim, and create an
   `Installing` binding for that exact generation;
3. release the slot lock, acquire the resident writer, derive the path-copied
   root, and prepare both root allocation and retire capacity;
4. briefly reacquire the per-slot lock to verify that the same publication
   claim and generation remain authoritative, then release it; competing slot
   transitions must wait on or reject that claim;
5. publish `Installing -> Active` with Release, perform the infallible root
   commit, and release the resident writer;
6. consume the publication claim under the slot lock and transition the FSM to
   `Resident`; and
7. route waiter wakeup only after both the new root and resident FSM state are
   visible.

`Publishing` is an internal transition phase, not a second resident authority.
A reader that observes the new root before step 6 also checks the slot's hot
resident generation, fails revalidation, and retries. If allocation,
retire-capacity reservation, or claim revalidation fails before step 5, the
old root remains authoritative, the slot claim is rolled back explicitly, and
the new binding is dropped or retried. Once the binding is Active, commit
cannot fail.

### Withdraw

Reclaim, truncate, direct-write invalidation, and explicit removal use:

1. acquire the semantic range reservation or reclaim claim required by the
   caller, then reserve an exclusive internal `Withdrawing` transition claim
   for the exact PageSlot/binding generation;
2. release slot/index locks, acquire the resident writer, derive a root without
   that exact binding, and prepare root plus retire capacity;
3. briefly reacquire the per-slot lock to revalidate the transition claim and
   generation, then release it;
4. publish `Active -> Withdrawn` with AcqRel;
5. commit root removal infallibly, then release the resident writer;
6. consume the withdrawal claim and perform the PageSlot completion; and
7. release the semantic reservation/claim and wake waiters.

If root preparation or claim validation fails before step 4, the old root
remains authoritative and the PageSlot claim returns to its prior state. The
range reservation or reclaim claim prevents a conflicting semantic operation;
the internal slot claim prevents an unrelated slot transition from stealing
the generation while no lock is held.

A reader holding an old root after step 4 sees `Withdrawn` and cannot acquire a
new pin. A reader that acquired and revalidated a pin before step 4 may finish
with that owned pin. The frame is reported allocator-free only after root
retention, PageSlot/cache retention, DMA leases, map pins, and installed-PTE
lifetime have all ended.

### Publication substrate gate

The current `Published::commit()` can encounter an occupied retire bag and
invoke `epoch::drain_with_budget(usize::MAX)`. Resident hot-path writers must
not inherit that unbounded commit behavior. Before `ResidentRoot` lands,
`Published::prepare_replace` must reserve a bounded retire slot during prepare
and return allocation/backpressure failure while the old root is still
authoritative. `PublishReservation::commit` then performs only the root swap
and enqueue into already-reserved storage.

The strengthened contract must keep the VM `RecipeIndex` pilot passing and add
a forced retire-capacity exhaustion test. Commit remains infallible; callers
handle pressure only before the linearization point.

## L4 Admission And Terminal Settlement

PageBacked first creates PageSlot candidates and retains all page resources.
It then builds one `OwnedPageIoSubmission` containing the existing
`PageIoRequest`, direction-neutral `PageDataLease`, planner/graph continuation,
and typed terminal route.

Admission has exactly two outcomes:

- rejection leaves queue state unchanged and returns the entire owned bundle
  to PageBacked for rollback, retry, or error settlement; or
- success atomically transfers custody to L4 and records one settlement token.

Inserting a request ID into one map and later discovering a full queue is not
an admission. Capacity for request, graph, waiter, completion, and required
bookkeeping must be reserved before the transfer linearization point.

L4 may move admitted custody among ready, planning, graph, retry, and
completion states. Cancellation, planner failure, partial device failure,
timeout, and success all converge on one consuming `OwnedPageIoSettlement`.
That bundle contains one result per page generation and all retained resources.
PageBacked consumes it, revalidates object/range/generation, performs PageSlot
transitions, installs or withdraws resident bindings, and wakes waiters. A
duplicate settlement, dropped settlement token, or retained lease after
terminal settlement is a correctness failure.

Same-page demand misses deduplicate in L4 after PageBacked generation
admission. Readahead may attach to an existing demand request, but optional
work never becomes the settlement owner for a demand waiter.

## L6 Admission, Limits, And Completion

Each physical `BlockDeviceRegistration` creates one `BlockSubmissionManager`.
Whole-device and partition handles route to that manager; partition bounds are
validated before admission and are preserved in completion attribution.

`BlockIoLimits` contains at least:

- logical block bytes and required I/O alignment;
- maximum transfer bytes and blocks;
- maximum SG segments and bytes per segment;
- segment-boundary mask or equivalent no-cross boundary;
- maximum outstanding commands;
- advertised hardware queue count; and
- flush/FUA durability capabilities.

Drivers report negotiated values where available. A missing value selects a
finite conservative default such as one page, one segment, and one queue. Zero
or integer maximum must not mean unbounded. Registration rejects internally
inconsistent limits.

L5 remains the only layer that interprets inode extents and maps logical file
ranges. Its adapter lowers mapped runs, holes, metadata nodes, and retained
lease slices into the existing `BackendBioGraph`. For each ready node, L6
admission performs generic splitting against immutable `BlockIoLimits` before
any piece enters the mutable queue. Split pieces retain graph node identity,
payload offsets, barrier domain, priority, and one parent completion counter.
If the complete bounded split cannot be admitted atomically, ownership returns
to the graph executor.

The queue may merge front/back adjacent requests only when device, operation,
flags, priority compatibility, barrier domain, payload contiguity, alignment,
SG count, transfer size, and boundary limits all permit it. Merge never crosses
a journal/data fence or partition boundary. L6 completion settles one graph
node counter; only L4 can aggregate the graph into the page terminal
settlement.

## Multi-Page Read And Completion

The first production multi-page path is buffered regular-file read and file
fault clustering. It reuses `PageIoRequest`, `PageIoRange`, `PageDataLease`,
and `BackendBioGraph`:

1. PageBacked probes a bounded logical page range and creates/fetch-deduplicates
   one stable PageSlot generation per miss;
2. it allocates and retains all read target frames before L4 admission;
3. L4 admits one range request and one multi-segment target lease;
4. ext4 or bdev-fs maps the range into holes and physically contiguous runs;
5. holes are zero-filled and settled without L6; mapped runs become graph
   nodes and are split by device limits before enqueue;
6. completion records success or error per page/segment, not one batch-wide
   generation; and
7. PageBacked installs each successful page only after its generation check.

A failure in one split BIO does not fabricate success for sibling pages.
Already completed pages may be installed if their exact graph dependencies
finished; failed or unissued pages receive retry/error settlement and retain no
orphaned frame or request custody. The syscall returns bytes according to the
existing partial-read contract after demand pages settle; optional readahead
tail failures do not fail completed demand bytes.

The initial batch cap is the minimum of L4 admission budget, target-frame
availability, filesystem mapping cap, and device-derived split budget. It is a
bounded configuration value recorded in the receipt, not an unchecked file
length allocation.

## Clustering, Splitting, And Plugging

Clustering occurs at two distinct layers:

- L4 clusters adjacent logical pages only when PageContainer, operation,
  priority class, coherency reservation, and terminal semantics match; and
- L6 merges adjacent mapped BIO pieces only after L5 mapping and limit-aware
  splitting prove physical compatibility.

L4 never guesses extent continuity, and L6 never interprets an inode or extent.
The graph adapter preserves holes and extent boundaries. Device limits may
split an otherwise contiguous extent; a barrier or FUA dependency may split an
otherwise mergeable BIO.

A short plug delays ready BIO publication only until the first of:

- the current reactor/service turn ends;
- the configured BIO/byte threshold is reached;
- the device queue becomes idle and can dispatch immediately;
- a barrier, FUA, or fsync-critical node arrives; or
- the submitter is about to yield or wait.

Completion processing and barriers bypass plugs. Demand plugs are shorter than
readahead/background plugs. A plug owns only admitted immutable submissions;
it holds no PageContainer, PageSlot, root, range, filesystem, or driver lock.

## Real Readahead

L4 owns adaptive file-data readahead mechanics; pressure policy remains with
the memory-pressure coordinator. Sequential history is keyed by
`(PageContainerKey, ReadaheadStreamId)` so two readers with different access
patterns do not corrupt one another's window.

The first algorithm is deliberately bounded:

```text
first demand miss       -> demand page + small optional window
sequential demand hit   -> grow window to configured cap
marker-page demand hit  -> queue the next asynchronous window
backward/random access  -> shrink or disable the window
memory/queue pressure   -> cancel or drop optional tail
```

Optional pages use ordinary PageSlot generations and PageContainer residents.
They are reclaimable, charged normally, and lower priority than demand and
fsync work. Readahead never installs VM PTEs and never converts an optional
failure into a demand-read error.

`sys_readahead(fd, offset, count)` keeps its current descriptor/type checks,
adds checked byte-range conversion, and calls
`PageContainer::advise_readahead(ReadaheadHint)`. The hint is submitted to L4
as optional work. A zero length is a no-op. Queue or memory pressure may admit
only a bounded prefix or drop the hint after returning success, consistent with
advisory semantics; structural errors detected before admission are returned.

Usefulness is counted only when a later demand read/fault consumes a page that
was first installed by readahead. The receipt separately records requested,
admitted, completed, demand-consumed, reclaimed-unused, canceled, and duplicate
pages plus device-read amplification.

## Observation And Performance Receipt

### Runtime facts

`IoSubmissionSnapshot` reports immutable, bounded counters and histograms from
L4 and L6:

- admission accepted/rejected by cause and priority;
- queue delay and service time distributions;
- request page count, transfer bytes, SG segment count, and split fanout;
- merge attempts/success/rejection by reason;
- queue depth, tag occupancy, plug flush reason, retry, timeout, and error;
- payload extra-copy bytes and bounce bytes by reason;
- readahead requested/admitted/useful/unused/canceled/duplicate pages;
- unique demand bytes, device-read bytes, dirty bytes, writeback bytes, and
  journal/checkpoint bytes separately;
- resident lookup retries, pin revalidation failures, publication
  backpressure, and root update work; and
- trace emitted/lost/overwritten counts.

Counters are non-allocating on hot paths. High-volume per-page events are
sampled or enabled only for a bounded trace window. Queue delay and device
service time use distinct timestamps.

### Receipt artifact

Each accepted comparison writes
`target/perf/io-submission/<run-id>/acceptance-receipt.json` with schema
`tx.io_submission.performance_receipt.v1`. A small dated progress research
note may summarize it, but the immutable receipt and bound raw artifacts are
the evidence.

The receipt fixes and hashes:

- repository revision and dirty-state declaration;
- Rust/QEMU/host tool versions, target, profile, build command, target
  directory, and build concurrency;
- host CPU/storage, QEMU machine/backend/cache/aio mode, vCPU count, guest RAM,
  no-swap setting, device model, queue count, and `BlockIoLimits`;
- filesystem image, workload corpus, Linux ext4 and Tx ext4 image digests;
- warm/cold cache preparation and repetition count;
- baseline and candidate binaries/configuration;
- all metric snapshots, trace shards, trace-loss accounting, stdout/stderr,
  and artifact SHA-256 values; and
- gate evaluation with explicit pass/fail reasons.

Required workloads are hot cached lookup, cold sequential read, random read,
raw-block SG/split/merge, concurrent readers at 1/2/4 harts, ext4 fsync/writeback
smoke, and the fixed clean-build witness from
`MEMORY_IO_ARCHITECTURE_v1.md`. Linux ext4, raw block, tmpfs, and Tx ext4 use
the same declared geometry where applicable.

A performance claim is invalid when trace loss is unknown, required counters
are absent, geometry differs without declaration, the candidate has a
correctness failure, or the receipt cannot verify every bound artifact.

### Acceptance thresholds

The first production candidate must satisfy all of these:

- aligned ordinary file payload extra-copy bytes are zero and normal-path
  bounce bytes are zero;
- no correctness or error-rate regression in any workload;
- single-hart hot-resident throughput is at least 98% of the locked baseline,
  while four-hart hot-resident throughput improves by at least 20% or measured
  resident-lock wait falls by at least 50%;
- cold sequential-read median throughput improves by at least 20%, median
  mapped request size exceeds one page, and useful readahead is reported;
- random-read p99 latency and device-read amplification regress by no more than
  5% versus readahead-disabled candidate mode;
- ordinary data write amplification is at most 1.3x, with journal and
  checkpoint amplification reported separately; and
- the clean-build witness meets the active first-convergence target of at most
  9000 seconds or improves the fixed Tx baseline by at least 15% without
  regressing the Linux-relative ratio.

Thresholds are evaluated over at least five interleaved baseline/candidate
pairs. Medians decide throughput gates; p95/p99 and dispersion remain in the
receipt. A failed performance threshold keeps the prior production mode even
when the candidate is functionally correct.

## Conditional Multi-Queue Gate

`BlockQueueShardSet` is considered only after the accepted single-queue
candidate shows all of:

- at least four active submitter harts and more than one advertised hardware
  queue;
- sustained queue/tag occupancy of at least 80%;
- measured L6 queue-lock wait or service-owner saturation accounting for at
  least 15% of end-to-end block service time; and
- no dominant upstream bottleneck in mapping, copying, readahead, or ext4
  serialization.

The sharded A/B candidate must preserve one device-wide barrier sequence and
exactly-once tag/completion ownership. It lands only if median throughput
improves by at least 15%, p99 latency does not regress by more than 5%, trace
loss does not increase, and all correctness receipts are identical. Otherwise
the implementation plan records the negative result and leaves one L6 queue.

Queue sharding does not shard journal authority, PageSlot state, resident
roots, or range reservations. Hardware queue selection is L6/driver policy;
filesystem planners never select a queue.

## Staged Rollout And Rollback

| Stage | Change | Required exit evidence | Rollback |
| --- | --- | --- | --- |
| C0 | Reconcile ext4 transaction frontier and supported production fsync settlement; close overlapping progress ledger state | focused fsync/frontier tests, Tier 1 receipt integrity verification, progress validation | no performance stage starts |
| P0 | Capture locked/single-page trace baseline and receipt geometry | verifiable baseline artifact with required counters and loss accounting | instrumentation can remain shadow-only |
| P1 | Extract L4 manager/handle and atomic custody transfer without behavior changes | admission-failure ownership tests, exactly-one settlement model tests, current read/writeback parity | revert manager wiring commit; no dual live owner |
| P2 | Extract device-scoped L6 manager and add `BlockIoLimits` | partition/device ownership, bounded split admission, tag/barrier/error tests | select prior binary; limits default conservatively |
| P3 | Split `PageStateDomain` and `RangeDomain` from resident/I/O state | lockdep/order tests, no-lock-across-yield lint, SMP same-page/range races | revert domain slice before resident publication |
| P4 | Strengthen `Published`, then publish `ResidentRoot` | retire-pressure tests, install/withdraw/pin races, VM pilot, SMP4 resident witness | prior locked-root binary remains A/B baseline |
| P5 | Extend the existing lease/completion path to multi-page reads | holes, extent boundaries, partial failure, stale generation, truncate/invalidate races | cap batch size to one page |
| P6 | Add logical clustering, limit-aware split/merge, and short plugging | split/merge property tests, barrier proofs, SG/copy counters, trace comparison | disable plug and cap cluster size to one |
| P7 | Add adaptive and explicit readahead | sequential/random/usefulness/cancel tests and syscall witness | runtime window cap zero disables optional work |
| P8 | Run final receipt and production selection | all correctness gates plus accepted performance receipt | retain prior accepted production binary/config |
| P9 | Optionally add multi-queue | conditional saturation evidence and successful A/B gate | remain on one queue |

A/B is performed with separate baseline and candidate builds or boot
configurations. One request is never executed by two authorities, and shadow
mode records decisions/counters only; it does not duplicate device I/O.

The six-hour ext4 crash campaign is not rerun after every stage. Stages use
focused host/model/SMP tests and verify the integrity of the existing immutable
Tier 1 baseline receipt. A fresh full Tier 1 campaign is required once for the
final production candidate because manager extraction and BIO ordering affect
the durable path. If that run fails, the next run resumes at the first failed
cut with `--resume --start-cut crash-cut-NNNN`; completed prefixes are not
replayed while their artifacts remain valid.

## Correctness Gates

Before P8 production selection, the combined candidate must prove:

- rejected L4/L6 admission returns all resources and leaves no queue, tag,
  graph, waiter, or slot residue;
- every successful L4 admission produces exactly one terminal settlement under
  success, cancel, timeout, planning error, split failure, device error, and
  PageContainer withdrawal;
- no lock or epoch guard crosses a yield, planner call, device call, or wait;
- resident install/withdraw linearization blocks stale-root pin acquisition and
  preserves already acquired pins;
- root allocation or retire-pressure failure leaves the old root authoritative;
- PageSlot remains the only dirty/writeback/redirty generation authority;
- multi-page completion handles holes, short final pages, split BIOs, partial
  failures, stale generations, truncate, reclaim, and direct-write invalidation;
- split/merge never violates device, partition, payload, barrier, FUA, or graph
  dependency limits;
- readahead cannot fail demand bytes, escape pressure accounting, pin pages
  against reclaim, or install VM PTEs;
- flush/FUA and ext4 ordered data-before-commit semantics are unchanged;
- static dependency scans keep `io_manager` free of concrete filesystem crates
  and keep L6 free of inode/extent interpretation; and
- focused host tests, `cargo -q xtask unit`, RV64 SMP4 I/O witnesses, final
  ext4 Tier 1 receipt verification, docs lint, progress validation, formatting,
  and scoped diff checks pass.

## Performance Gates

No stage may claim a speedup from queue length or cache hit rate alone. The
receipt must attribute the change to queue delay, service time, lock wait,
request/SG shape, split/merge outcomes, copies/bounces, readahead usefulness,
device bytes, write amplification, and trace loss.

Resident publication is accepted only when it improves measured concurrent
hot reads without a meaningful single-hart regression. Multi-page/readahead is
accepted only when sequential demand improves and random-read amplification
stays bounded. Plugging is accepted only when service/throughput improves
without delaying demand or barriers beyond the p99 gate. Multi-queue is
accepted only by its separate conditional gate.

## Progress-Plan Supersession

[`2026-07-11-io-manager-phase0-landing.json`](../../progress/plans/2026-07-11-io-manager-phase0-landing.json)
is stale but still marked `active`: Phase 0 staging landed while its ext4
planning step remains pending. The implementation-planning step must reconcile
that ledger before creating an overlapping active plan:

1. mark the old plan `canceled` with a note that completed staging steps remain
   historical facts and the pending ext4-planning step is superseded;
2. link it to the new I/O SubmissionManager performance plan ID;
3. create the new JSON plan as `proposed`, validate it, then make it `active`
   only after the old plan is no longer active; and
4. keep ext4 Tier 1 lifecycle closure as a prerequisite receipt, not as an
   overlapping performance plan.

No implementation commit may silently update both ledgers as active. Each
stage catch-up records what changed, verification run, next step, blocker, and
the receipt or trace artifact path.

## Completion Criteria

This program is complete only when:

- independent L4 and device-scoped L6 managers are the sole production owners
  of their admitted mutable state;
- PageContainer resident, slot-index, and range domains are separate and a
  cached hit uses the verified `Published<ResidentRoot>` path;
- multi-page read, generic split/merge/plugging, and real readahead are
  production reachable through ext4 and bdev-fs where applicable;
- all admission, lifetime, generation, barrier, fsync, and SMP correctness gates
  pass;
- one fresh candidate-bound ext4 Tier 1 receipt passes without replaying valid
  prefixes after a failure;
- `tx.io_submission.performance_receipt.v1` verifies and passes the declared
  thresholds; and
- multi-queue is either accepted by its gate or explicitly closed as
  unnecessary with the negative measurement recorded.

Until then, runnable staging I/O must be described as executable foundation,
not as completion of the manager-owned high-throughput data plane.
