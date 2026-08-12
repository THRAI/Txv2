# I/O Manager

<!-- txdoc:05-FILESYSTEM-IO-MANAGER-V1 -->

**Status.** v1 (aligned 2026-07-25). Draft architecture contract.

**Purpose.** Specify the txKernel file-I/O execution plane between
`PageContainer`, filesystem layout lowering, and device execution. The I/O
manager is not the global memory-pressure control plane, a data cache, or a
concrete filesystem. It owns submission queues, service futures, bounded
execution scheduling, graph execution, request completion, and block-request
dispatch. It keeps ordinary file data owned by `PageContainer`, global
reclaim/writeback policy owned by `MemoryPressureCoordinator`, filesystem
layout and transaction semantics owned by the mounted filesystem instance,
and hardware execution owned by the device/driver layer.

**Audience.** PageBacked, VFS, filesystem, device, and reactor implementers
working on the cold-file, mmap-fault, iozone, AIO, and block-device throughput
paths.

**Companion documents.**

- [`MEMORY_IO_ARCHITECTURE_v1.md`](../03_memory-vm/MEMORY_IO_ARCHITECTURE_v1.md) - canonical dual-plane ownership, `PageDataLease`, pure layout planning, zero-copy payload, memory pressure, and allocation slow-path contract.
- [`PAGE_BACKED_v1.md`](../03_memory-vm/PAGE_BACKED_v1.md) - `PageContainer`
  as the page-indexed content owner.
- [`OBJECT_API_LANES_v1.md`](../00_meta-framework/OBJECT_API_LANES_v1.md) -
  publication families, owner-private roots, and manager/state-machine
  exclusions.
- [`VM_v1_2.md`](../03_memory-vm/VM_v1_2.md) - VM recipes and pmap
  publication. VM consumes PageBacked materialization but does not own the file
  cache.
- [`MOUNT_v1.md`](MOUNT_v1.md) - mounted filesystem instance hosting and
  `MountPayload` lifetime.
- [`VFS_CHECKS_V2.1.md`](VFS_CHECKS_V2.1.md) - path and open-file witnesses.
- [`BDEV_FS.md`](BDEV_FS.md) - block devices exposed as page-backed files.
- [`TX_EXT4_PLAN_v1_2.md`](TX_EXT4_PLAN_v1_2.md) - ext4 as a concrete
  filesystem backend.
- [`DEVICE.md`](../06_devices/DEVICE.md) - static block-device registrations
  and driver/device routing.
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) - bounded steps,
  `StepOutcome`, and commit/publish discipline.
- [`COMPLETION_v1.md`](../02_execution/COMPLETION_v1.md) - completion objects
  as wait middleware.

---

## 1. Problem statement

<!-- txdoc:IO-MANAGER-PROBLEM-STATEMENT-1 -->

The current staged implementation has a correct PageBacked shape but a
serializing I/O path:

```text
read/fault -> PageContainer -> FsPageBacking::fetch_page
           -> concrete filesystem pager -> BlockDeviceOps::read_blocks
           -> virtio driver lock -> device
```

The path is inefficient for large or cold file workloads because it combines:

- single-page fetches;
- a coarse `PageContainer` state lock;
- a sparse page index currently backed by a `BTreeMap`;
- filesystem pager locks that serialize a mounted instance;
- synchronous block-device calls with no request tag or completion queue;
- repeated 4 KiB staging copies; and
- no generic file-data readahead or block-request merging.

The live tree is currently bifurcated. Planner-backed ext4 requests can pass
through L4 planning and L6 block submission, while compatibility ext4 and
bdev-fs paths still call filesystem/block-device operations synchronously and
bypass L6 scheduling. Publication work must not hide this ownership gap: the
manager boundary is complete only when ordinary file-page misses and
writeback commit requests into L4 and all block plans enter L6.

The I/O manager breaks this path into asynchronous submission and completion
boundaries without changing the content ownership model. A syscall, page-fault,
or writeback step commits a request and yields. Long-lived service futures
batch, map, dispatch, and complete the request.

---

## 2. Ownership rule

<!-- txdoc:IO-MANAGER-OWNERSHIP-RULE-1 -->

The I/O manager is an execution plane. It must not become either a second page
cache or a second memory-pressure/writeback policy owner.

| Layer | Owns ordinary file data? | Owns |
|---|---:|---|
| `PageContainer` / `PageSlot` | Yes | published resident bindings, frame evidence, dirty/writeback generation state |
| `MemoryPressureCoordinator` | No | pressure episodes, dirty budgets, bounded reclaim/writeback intent, producer throttle decisions |
| `PageIoSubmissionManager` | No | `PageIoRequest`s, admitted batches, graph execution, admitted resource-bundle custody, local execution priority, readahead/writeback mechanics, request waiter routing |
| Pure filesystem pager | No | logical-file layout, allocation proposals, metadata after-images, journal/barrier dependencies |
| Tx filesystem adapter | No; holds pre-admission resources only | transaction admission, temporary opaque-key binding, `FileIoPlan<K>` lowering, and atomic resource-bundle transfer to L4 |
| `BlockSubmissionManager` | No | `Bio`, request tags, queue depth, LBA merge, barrier ordering |
| Driver/HAL | No | DMA mapping, hardware descriptors, IRQ or polling completion |

The only long-lived ordinary file-data cache is the `PageContainer`. Filesystem
metadata caches are allowed when they are private to a mounted filesystem and
do not duplicate ordinary file data. Driver bounce buffers are temporary DMA
staging objects and must be released after completion.

Global memory pressure, dirty thresholds, victim selection, and producer
throttling are not owned by the I/O manager. The memory-pressure coordinator
decides when, from which owner/domain, and how much background work to request.
PageBacked validates concrete PageSlot generations and signs
`PageDataLease`s. L4/L6 then provide bounded execution, queue/service feedback,
and typed completion. Queue saturation may delay execution; it never clears
dirty state or substitutes for the global dirty budget.

---

## 3. Global architecture

<!-- txdoc:IO-MANAGER-GLOBAL-ARCHITECTURE-1 -->

```mermaid
flowchart TB
  subgraph Entry["entry points"]
    SY["syscall future: read/write/pread/AIO/io_uring"]
    PF["page-fault future: mmap/file fault"]
    OD["O_DIRECT / direct I/O"]
    STR["stream/control I/O: TTY/pipe/socket/eventfd/ioctl"]
  end

  subgraph VFSVM["VFS / VM"]
    OF["OpenFile / RNode / DEntry"]
    VM["AddressSpace recipes + pmap"]
  end

  subgraph PC["PageContainer"]
    IDX["Published ResidentRoot / persistent sparse index"]
    RR["RangeReservation"]
    SLOT["PageSlot FSM"]
    DATA["PC-owned frames"]
    ADM["PageBacked owner admission / settlement"]
    PH["PageIoSubmissionHandle"]
  end

  subgraph IOM["io_manager execution plane"]
    PG["PageIoSubmissionManager (L4)"]
    GX["BackendBioGraph execution"]
    BLK["BlockSubmissionManager (L6)"]
    RT["service runtime: budget/wait/observe"]
  end

  subgraph FS["filesystem planning and lowering"]
    AD["Tx filesystem adapter: pre-admission binding + lowering"]
    EXT4["pure FileLayoutPlanner"]
    TMP["tmpfs / memfd / shm"]
    BDEV["bdev-fs adapter"]
  end

  subgraph MPC["memory-pressure control plane"]
    MC["MemoryPressureCoordinator"]
  end

  subgraph DEV["device execution"]
    HANDLE["BlockDeviceHandle / registration"]
    DRV["virtio/NVMe/SATA driver"]
    HAL["HAL DMA/IRQ/platform hooks"]
  end

  SY --> OF --> PC
  PF --> VM --> PC
  VM --> OF
  STR --> OF
  STR -->|"StructBacked or Projected route"| RT

  PC --> IDX
  PC --> RR
  PC --> SLOT
  SLOT --> DATA

  SLOT -->|"miss / owner candidate generation"| ADM
  ADM -->|"admitted generation + PageDataLease"| PH --> PG
  PG -->|"request + opaque payload key"| AD
  AD --> EXT4
  EXT4 -->|"FileIoPlan K"| AD
  AD -->|"existing BackendBioGraph"| GX
  BDEV -->|"existing BackendBioGraph"| GX
  GX --> BLK
  TMP -->|"memory completion"| PG
  BLK --> HANDLE --> DRV --> HAL
  HAL --> DRV -->|"completion"| BLK --> GX --> PG -->|"owned terminal settlement"| ADM
  ADM -->|"generation-checked PageSlot transition"| SLOT
  MC -->|"bounded writeback intent / owner domain"| ADM
  PG -->|"page execution feedback"| MC
  BLK -->|"queue and service feedback"| MC

  OD --> OF --> RR
  RR -->|"flush/wait/invalidate overlapping slots"| SLOT
  RR -->|"direct Bio, no PC data cache"| BLK
```

Buffered file I/O and file-backed mmap use the `PageContainer` path.
`O_DIRECT` bypasses the PC data cache but not PC coherency. Stream, message,
and control I/O do not use `PageContainer` unless their object is explicitly
page-backed.

---

## 4. Layer responsibilities

<!-- txdoc:IO-MANAGER-LAYER-RESPONSIBILITIES-1 -->

### 4.1 L4 - page submission

<!-- txdoc:IO-MANAGER-L4-PAGE-SUBMISSION-1 -->

The L4 page-submission layer schedules requests keyed by
`(PageContainer, PageIndex range)`. It knows page-cache semantics and does not
know device LBA layout.

Its target owner type is `PageIoSubmissionManager`; PageContainer holds only a
typed `PageIoSubmissionHandle`. The current `PageService` plus
`PageRequestQueue` implementation is the staging backend. Its submission,
completion, backend-resume, metadata-continuation, graph, and waiter state must
move out of `PageContainerState` before resident publication removes the coarse
PC lock.

It owns:

- `PageIoRequest`, `PageIoBatch`, and `PageIoCompletion`;
- execution ordering within demand-read, mmap-fault, writeback, fsync, and
  readahead classes supplied by request admission;
- same-page miss deduplication and waiter routing;
- page-range batching and short plug windows;
- generic file-data readahead pattern detection and optional-request mechanics,
  bounded by current memory/queue pressure;
- progress cursors for already-admitted writeback work;
- custody of the admitted request resource bundle until terminal routing;
- execution state for the existing `BackendBioGraph`; and
- construction and delivery of one owned terminal settlement through
  `PageContainer` public APIs.

It does not own:

- ordinary file-data frames after completion;
- global dirty thresholds, reclaim victim choice, or writeback quotas;
- `PageSlot` dirty/writeback generation transitions;
- filesystem extent or allocation state;
- block-device queue depth or tags; or
- driver DMA descriptors.

### 4.2 L5 - filesystem planning and Tx lowering boundary

<!-- txdoc:IO-MANAGER-L5-BACKEND-PLANNING-1 -->

L5 is a cross-crate boundary, not an I/O-manager-owned concrete filesystem
module. The pure filesystem pager implements `FileLayoutPlanner` and returns
`FileIoPlan<K>`. Before graph admission, a Tx filesystem adapter outside
`io_manager` temporarily retains the actual `PageDataLease` or direct-I/O
pins, supplies opaque payload keys, admits transaction capabilities, and
lowers the pure plan into the existing `BackendBioGraph`.

The pure planner receives:

- filesystem/object identity and immutable format inputs;
- logical file ranges, operation, durability intent, and transaction domain;
- caller-provided opaque payload keys; and
- immutable metadata observations or a typed resume token requesting more
  immutable input.

It returns only the canonical pure DTO:

```rust
pub trait FileLayoutPlanner {
    type PayloadKey: Copy + Eq;

    fn plan(
        &self,
        request: FileLayoutRequest<Self::PayloadKey>,
    ) -> Result<FileIoPlan<Self::PayloadKey>, FileLayoutError>;
}
```

`FileIoPlan<K>` contains file/LBA ranges, holes, allocation proposals,
metadata after-images, dependencies, barrier domains, and opaque `K`. It
contains no PPN, `PageFrameRef`, `BioVec`, queue tag, waiter, reactor object,
PageBacked pointer, lease capability, or Tx completion callback.

The Tx adapter receives the pure plan plus its private key-to-retained-segment
table. It revalidates mutation preconditions, admits any
`FrozenMetadataLease`, and lowers data and metadata nodes into the sole
`BackendBioGraph`. Only this adapter can translate retained segments into
`PageFrameRef`/`BioVec`; the pure pager cannot. The adapter owns the temporary
resource bundle only through planning and lowering. Successful graph admission
atomically transfers that bundle to the L4 graph execution; failed admission
unwinds it back to PageBacked/direct-I/O ownership.

Examples:

- ext4 maps file pages through inode and extent metadata, reports holes,
  proposes allocation on writeback, and returns layout/transaction dependencies
  which `tx-ext4` lowers into graph nodes.
- tmpfs, memfd, and shm complete from memory and usually do not produce block
  bios.
- bdev-fs maps page offsets directly and may lower to the existing graph
  without an ext4-style pure pager.

Concrete filesystem crates must not be imported by `io_manager`. They
implement or host neutral planning/adaptation surfaces consumed through the
mounted filesystem payload. The current `FsPageBacking`, `BackendPlan`,
`PageIoPlan`, and `BioPlan` paths are compatibility staging vocabulary, not a
second target interface family.

Filesystem mapping caches may independently use publication when they expose
immutable mapping facts. In particular, an ext4 mapping/extent root is a strong
conditional candidate after journal commit, truncate, hole conversion, and
block-reuse invalidation define their generation rules. Filesystem parser,
journal, allocation, and compatibility pager state are not RCU roots.

Observational filesystem read caches are a separate conditional family. An
ext4 lookup/directory/metadata snapshot may be published only after positive
and negative entry generations share one invalidation boundary and read-side
LRU mutation is removed or split into independent atomic accounting. Such a
snapshot accelerates lookup; it does not become filesystem namespace or inode
allocation authority.

### 4.3 L6 - block submission

<!-- txdoc:IO-MANAGER-L6-BLOCK-SUBMISSION-1 -->

The L6 block-submission layer schedules requests keyed by
`(device, op, LBA range, flags)`. It knows device queueing and ordering, not
file pages or inodes.

Its target owner type is `BlockSubmissionManager`. The current `BlockQueue`,
`QueueDepth`, `BlockTagTable`, `BlockServiceDriver`, and completion trackers are
the staging backend. They move behind a manager handle rather than into a
PageContainer root. Their queue and completion state remains mutable.

It owns:

- `Bio`, `BioVec`, `BlockRequest`, and request IDs;
- front/back adjacent LBA merge;
- queue-depth accounting;
- tag allocation and tag-to-request completion lookup;
- flush, FUA, and barrier ordering;
- request timeout and retry policy; and
- local execution fairness between admitted foreground demand I/O, fsync,
  writeback, readahead, and raw block-device requests.

The existing `BackendBioGraph` is the sole BIO DAG. It is extended in place
with retained lease slices, barrier domains, inherited priority, and typed
completion routes. A second graph type is forbidden. Graph nodes may carry an
existing PageBacked page-cache source/target or a direct-I/O DMA-pinned
source/target; the variants retain their distinct lifetime and coherency rules.
L4 owns graph-level request execution and terminal aggregation; L6 owns only
ready BIO-node queueing, tags, dispatch, and node completion. L6 does not
interpret file generations or transaction meaning.

The first production scheduler should be deliberately small: FIFO with
adjacent merge, read-deadline bias, queue-depth limits, tag completion, and
strict barrier fences. More complex BFQ/Kyber-like policies are later
optimizations.

### 4.4 L7 - driver execution

<!-- txdoc:IO-MANAGER-L7-DRIVER-EXECUTION-1 -->

The driver layer owns hardware protocol details only:

- DMA pin/map or bounce-buffer setup;
- descriptor construction;
- virtqueue or hardware-queue submission;
- device notification;
- IRQ or polling completion harvest; and
- hardware status translation.

Drivers must not implement generic file-data readahead, page-cache state,
filesystem metadata policy, or block scheduler fairness.

---

## 5. Service futures

<!-- txdoc:IO-MANAGER-SERVICE-FUTURES-1 -->

I/O manager services are long-lived kernel service futures. They are
actor-like because they own queues and are woken by messages, but they are not
pure actors: hot resident page reads still observe `PageSlot` state directly.

```mermaid
stateDiagram-v2
  [*] --> Sleeping
  Sleeping --> Runnable: submit / completion / timer / pressure
  Runnable --> DrainCompletions
  DrainCompletions --> DrainSubmissions
  DrainSubmissions --> Coalesce
  Coalesce --> Dispatch
  Dispatch --> Maintenance
  Maintenance --> Runnable: backlog and budget
  Maintenance --> Sleeping: no work or budget exhausted
```

Service futures include:

| Service | Queue owner | Primary wake sources |
|---|---|---|
| `kpageiod` | page requests and page completions | demand miss, mmap fault, `readahead`, page completion |
| `kwriteback` | admitted writeback batches and graph progress | fsync frontier; coordinator-issued bounded writeback intent; page/block completion |
| `kblockiod` | block bios and requests | bio submission, hardware completion, queue-space timer |
| driver poll/completion service | hardware completion rings | IRQ, polling timer, outstanding request count |

The reactor should give completion processing a small hard-priority budget
before running new submissions. A typical service turn is:

```text
drain completions
admit new submissions
pick work by priority and budget
coalesce page ranges or LBA ranges
dispatch until queue depth or budget is exhausted
run background maintenance
sleep only after rechecking queues
```

### 5.1 Coordinator work and feedback contract

<!-- txdoc:IO-MANAGER-COORDINATOR-FEEDBACK-1 -->

The coordinator never pushes raw pages, PPNs, owner locks, or callbacks into
the I/O manager. A background writeback episode has this direction:

```text
MemoryPlan bounded intent
  -> PageBacked owner scan + generation-checked admission
  -> PageDataLease + PageIoRequest
  -> Tx filesystem adapter pre-admission bundle + BackendBioGraph
  -> L4 admitted graph + resource-bundle custody -> L6 BIO execution
  -> L4 owned terminal settlement -> PageBacked generation validation
  -> PageSlot typed transition
  -> immutable progress/service feedback -> coordinator
```

The I/O manager reports facts, not policy decisions: admitted/completed bytes,
queue delay, service time, graph/node backlog, request-size and SG distributions,
merge results, queue-depth saturation, retry/timeout/error counts, bounce bytes
by reason, and terminal completion generations. PageBacked separately reports
dirty generations cleaned and frames that actually became allocator-free. The
coordinator combines these receipts on its next `MemoryPolicy` invocation.

PageBacked remains the semantic owner of file data throughout. The adapter has
temporary custody before graph admission; L4 has custody after admission. L6
completion alone cannot release a lease. L4 aggregates all participating nodes
and transfers exactly one owned terminal settlement bundle to PageBacked, which
performs the final object/range/generation validation before changing
`PageSlot` state and settling, retrying, or rolling back the bundle.

---

## 6. Plugging, priority, and readahead

<!-- txdoc:IO-MANAGER-PLUG-PRIORITY-READAHEAD-1 -->

Plugging and readahead solve different problems:

- plug controls when queued requests flush downward;
- readahead controls which optional future pages are requested.

Demand requests may use a very short plug window bounded by the current future,
reactor turn, batch threshold, queue-idle state, or impending yield. Readahead
and background writeback may wait longer and are cancellable.

The coordinator owns admission quotas and pressure-driven class budgets; the
I/O manager owns only ordering among already-admitted work. Initial local
priority order:

1. completion processing;
2. demand page faults and foreground reads;
3. fsync-required writeback and barriers;
4. normal foreground writes;
5. readahead;
6. background writeback.

The first L4 readahead detector is adaptive but conservative:

```text
first miss: demand page plus a small optional window
sequential hit: grow the window up to a cap
readahead marker hit: trigger the next asynchronous window
random access: shrink or disable the window
memory or queue pressure: cancel or drop optional tail
```

Readahead installs pages into `PageContainer` only. VM PTE prefault is a
separate VM policy and must not be implied by file-data readahead.
Readahead pages remain ordinary PageBacked candidates; L4 does not protect them
from reclaim or charge them outside the coordinator's pressure accounting.

---

## 7. `PageContainer` synchronization boundary

<!-- txdoc:IO-MANAGER-PC-SYNC-BOUNDARY-1 -->

The PageBacked hot path must not become four nested locks. The structures are
semantic layers, not lock layers.

The live staging implementation currently places resident bindings,
`PageSlot`s, L4 `PageService`, direct-I/O state, range reservations, and L6
block runtime under one `PageContainerState` lock. A normal cached file hit can
enter that lock for range-conflict observation, hit probing, resident pin
snapshot, and post-pin revalidation. This shared lock is an implementation
seam to remove, not the target synchronization model.

| Structure | Role | Initial implementation | Later implementation |
|---|---|---|---|
| `PageContainer` | content-owner boundary, size, backend reference, manager handles | coarse staging state plus atomics | same public API with split private owners |
| resident root | page-to-stable-cell binding | locked `BTreeMap` sparse backend | `Published<ResidentRoot>` with persistent path-copy sparse index |
| `PageSlot` / resident cell | per-page generation and state | per-slot lock plus duplicated marks | one authoritative state cell; atomic hot observation plus serialized transitions |
| `RangeReservation` | range semantic exclusion | locked interval table | optimized range lock |
| `PageIoSubmissionManager` | L4 requests, completions, graphs, request waiters | `PageService` embedded under PC state | dedicated single-owner/service state behind typed handle |
| `BlockSubmissionManager` | L6 queue, depth, tags, trackers | `BlockQueue` runtime embedded under PC state | dedicated single-owner/service state behind typed handle |
| `PageDataLease` | multi-page payload lifetime and PageSlot generations | staged one-page `PageLease` plus `IoDataSource`/`IoDataTarget` | PageBacked-issued immutable multi-page capability lowered into existing graph nodes |

Resident read hit target:

```text
pin epoch/guard
resident.read(guard).lookup(page)
resident.hot_state.load(Acquire)
acquire owned MapPin
Resident -> copy/map frame
```

The resident-hit path does not enter either submission manager. Read-side
range-conflict hints may later use a bounded atomic or immutable summary, but
the authoritative `RangeReservation` table and reserve/release transitions
remain serialized.

Only miss, write, truncate, direct I/O, and writeback paths should take
stronger locks. No `PageContainer`, slot, index, or range lock may be held
while executing filesystem or device I/O.

`RangeReservation` does not replace slot state. It protects logical file
ranges, including pages that do not yet have slots. It is required for
`O_DIRECT`, truncate, fallocate/hole punch, and fsync fences.

Resident installation validates the `PageSlot` completion generation before
publishing the new root. Reclaim, truncate, direct-write invalidation, and
explicit withdrawal mark the stable cell withdrawn and publish root removal
before releasing the governing range/direct-I/O reservation. Waiter wakeup is
after publication. Previously acquired `MapPin`s and installed PTEs follow
their normal VM teardown/shootdown lifetime; RCU is not their revocation
mechanism.

---

## 8. `O_DIRECT` and coherency

<!-- txdoc:IO-MANAGER-ODIRECT-COHERENCY-1 -->

`O_DIRECT` bypasses the PC data cache but not PC coherency. It uses
`RangeReservation` plus page-slot invalidation/writeback, not special logic in
`SparseIndex`.

Direct read:

1. reserve the direct-read range;
2. wait for overlapping writeback and flush overlapping dirty pages;
3. submit direct bios into user/iov pages or pinned direct buffers;
4. preserve or invalidate overlapping clean resident slots according to the
   mounted filesystem's coherency policy; and
5. release the range and wake waiters.

Direct write:

1. reserve the direct-write range;
2. block new buffered faults/writes in the range;
3. wait for or flush overlapping dirty/writeback slots;
4. submit direct bios from user/iov pages;
5. on success, invalidate overlapping resident clean slots or update full-page
   aligned slots if that optimization is explicitly implemented; and
6. release the range and wake waiters.

The conservative first implementation should invalidate overlapping resident
slots after successful direct writes instead of trying to update them in place.

---

## 9. Filesystem isolation

<!-- txdoc:IO-MANAGER-FILESYSTEM-ISOLATION-1 -->

The I/O manager must not import concrete filesystem crates. The target
isolation boundary is the pure `tx-pager-api` DTO surface plus a Tx filesystem
adapter hosted by the mounted filesystem payload.

```mermaid
flowchart LR
  VFS["VFS / Mount"] --> AD["Tx filesystem adapter"]
  PB["PageBacked + PageDataLease"] --> AD
  AD --> API["tx-pager-api: request + FileIoPlan K"]
  EXT4["pure ext4 pager"] -.implements.-> API
  AD --> BG["existing BackendBioGraph"]
  BG --> IOM["io_manager execution"]
  TMP["tmpfs"] -->|"memory completion"| IOM
  BDEV["bdev-fs adapter"] --> BG
```

The target interface split is:

- `FsOps`: namespace and metadata operations consumed by VFS and Mount.
- `FileLayoutPlanner`: pure range/layout/transaction planning over
  caller-supplied opaque payload keys.
- `FileIoPlan<K>`: the sole pure planning DTO.
- `BackendBioGraph`: the sole Tx block-I/O execution DAG after adapter
  lowering.

The existing `FsPageBacking::fetch_page` / `flush_page` surface is the current
staging form. Existing `PageIoPlan`, `BackendPlan`, and `BioPlan` values remain
valid compatibility code only while callers migrate. They must converge into
`FileIoPlan<K>` at the pure boundary and the existing `BackendBioGraph` at the
execution boundary; they are not promoted into parallel target abstractions.

---

## 10. Stream and control I/O

<!-- txdoc:IO-MANAGER-STREAM-CONTROL-IO-1 -->

Not every I/O object uses `PageContainer` or I/O manager page submission.

| Object class | PC path? | Route |
|---|---:|---|
| regular ext4 file | yes | PageContainer -> page submission -> ext4 planner -> block submission |
| file-backed mmap | yes | VM recipe -> PageContainer -> page submission |
| tmpfs/memfd/shm | yes | PageContainer -> memory pager completion |
| block device file (`/dev/vda`) | yes | bdev-fs PageContainer -> block submission |
| framebuffer/PageBackedDevice | yes | prepopulated device pages, usually no block submission |
| TTY, pipe, socket | no | subsystem queue/ring plus readiness wait |
| eventfd/timerfd/signalfd | no | small subsystem state plus wait source |
| ioctl/control | no | typed control operation |

Stream and message subsystems may have their own service futures and wait
sources. They must not be forced through page submission unless the object is
explicitly page-backed.

---

## 11. Proposed module topology

<!-- txdoc:IO-MANAGER-MODULE-TOPOLOGY-1 -->

The implementation should keep content ownership, planning adaptation, and
I/O execution in separate modules/crates:

```text
crates/tx-subsystems/src/page_backed/
    container.rs
    slot.rs
    range.rs
    index/
        sparse.rs
        btree.rs
        txarray.rs
    materialize.rs
    user_buffer.rs

crates/tx-subsystems/src/io_manager/
    page/
        request.rs
        queue.rs
        service.rs
        readahead.rs       # optional request mechanics, not pressure policy
        writeback.rs       # execution of admitted batches, not victim policy
        completion.rs
    graph/
        execution.rs
        completion.rs
    block/
        bio.rs
        request.rs
        queue.rs
        tag.rs
        barrier.rs
        service.rs
        completion.rs
    runtime/
        budget.rs
        wait.rs
        priority.rs
        observe.rs

crates/tx-subsystems/src/fs_iface/
    ops.rs
    pager.rs               # current compatibility bridge
    plan.rs                # staging values plus BackendBioGraph

crates/tx-pager-api/       # target; may begin module-local
    request.rs
    plan.rs
    key.rs
```

Concrete filesystems stay outside `io_manager`:

```text
crates/tx-ext4/src/
    adapter.rs             # pre-admission binding, transaction admission, lowering
    metadata_cache.rs

crates/tx-ext4-pager/      # target; pure layout/transaction planning
    mapper.rs
    planner.rs

crates/tx-fs/src/bdevfs/
    backing.rs
    coherence.rs
    partition.rs
```

This topology is a target shape, not a requirement to perform one large
mechanical move. Behavior-preserving file splits should precede semantic
changes when a current module is already over the source-size guardrail.
In the current checkout, compatibility backend code still lives under
`io_manager/backend/`, `BackendGraphExecution` still lives in
`io_manager/page/service.rs`, and `BackendBioGraph` is declared in
`fs_iface/plan.rs`; the tree above does not claim those moves have landed.

---

## 12. Staged migration

<!-- txdoc:IO-MANAGER-STAGED-MIGRATION-1 -->

1. **Interface seam.** Freeze `PageDataLease`, pure
   `FileLayoutRequest`/`FileIoPlan<K>`, and the existing `BackendBioGraph` as
   the only target pipeline. Keep `FsPageBacking`, `PageIoPlan`, `BackendPlan`,
   and `BioPlan` explicitly compatibility-only.
2. **PageSlot and range reservation.** Move PC state from a coarse state lock
   toward per-slot state and a separate range-reservation table. A locked
   BTree/SparseIndex backend is acceptable in this stage.
3. **Manager ownership.** Move the staging `PageService` and block runtime out
   of `PageContainerState`. Introduce `PageIoSubmissionManager`,
   `BlockSubmissionManager`, and typed handles; submission commits ownership
   into bounded manager queues.
4. **Publication substrate.** Add pre-reserved retirement and `Published<T>`;
   use VM recipe publication as the correctness pilot.
5. **Resident publication.** Replace the locked resident `BTreeMap` with a
   persistent sparse root, unify PageSlot generation/dirty authority, and make
   cached resident reads independent of manager locks.
6. **Page and block services.** Run `kpageiod`/`kwriteback` and `kblockiod`
   service futures with coordinator-bounded work admission, demand-page
   priority, short plugging, same-page deduplication, generation-checked
   completion, adjacent merge, tags, queue depth, barrier handling, and
   immutable service feedback.
7. **Filesystem planning.** Extract pure ext4 `FileLayoutPlanner`; place lease
   binding, transaction admission, and `FileIoPlan<K>` to `BackendBioGraph`
   lowering in `tx-ext4`. Migrate bdev-fs directly to graph lowering.
8. **Readahead and direct I/O.** Implement generic L4 readahead and the
   `O_DIRECT` range-coherency protocol.
9. **Filesystem mapping publication.** Migrate measured immutable mapping
   roots such as ext4 extent lookup only after journal/invalidation generation
   rules are explicit. Compatibility pager caches and driver queues are not
   publication targets.

---

## 13. Implementation readiness

<!-- txdoc:IO-MANAGER-IMPLEMENTATION-READINESS-1 -->

Ready to implement first:

- `PageIoRequest` / `Bio` value types;
- locked `PageSlot` and `RangeReservation`;
- manager extraction and typed page/block submission handles;
- retire-capacity reservation and the generic publication primitive;
- page and block service-future skeletons;
- completion generation checks;
- manager feedback snapshots/receipts that contain no policy callbacks or
  owner locks; and
- focused tests for same-page deduplication, range conflict, direct-write
  invalidation, LBA merge, queue-depth blocking, and barrier ordering.

Deferred until the prerequisites above land:

- persistent resident sparse-root publication;
- pmap and filesystem mapping publication without retention/invalidation
  contracts or measurements;
- complex BFQ/Kyber-style block scheduling;
- delayed allocation in ext4;
- full `O_DIRECT` update-in-place optimizations; and
- multi-hardware-queue driver sharding.

The first implementation should optimize the currently measured bottleneck:
breaking the synchronous single-page path. More complex fairness and allocation
algorithms belong after the request/completion boundary exists. I/O manager
readiness does not by itself prove global writeback/reclaim readiness; that also
requires the coordinator work/feedback contract and PageBacked owner admission.
