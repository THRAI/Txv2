# Ext4 I/O Manager Write-Path Design

**Status:** approved design, implementation pending (2026-07-13)

## Purpose

Define the Phase 6 migration from ext4's compatibility `FsPageBacking` path
to the neutral I/O-manager plan path. The design covers buffered writes,
`mmap` dirtying, writeback, `fsync`/`fdatasync`, and later `O_DIRECT` without
giving the I/O manager ownership of file data or filesystem semantics.

## Ownership and boundary

`PageContainer` owns ordinary file-data frames. `PageSlot` owns page state and
generation accounting. L4 owns queues, coalescing, priority, in-flight frame
leases, completion routing, and generic readahead. L5 is implemented by ext4:
it maps logical ranges, allocates extents, records metadata intent, and owns
JBD2 ordering. L6 owns per-device merge, queue depth, tags, and fences.
Drivers own DMA and hardware completion only.

No PC, slot, sparse-index, range-reservation, inode, allocator, or journal
lock may be held while submitting or waiting for filesystem/device I/O.
`io_manager` and `fs_iface` remain neutral: they must not import `tx-ext4`,
`tx-fs`, or a concrete driver.

## Required IR changes

The current `BackendPageRequest` has no source buffer and `BackendPlan` is a
single-stage result. Phase 6A must introduce the following neutral concepts.

- `IoDataSource`: `None` for reads, PC-backed frame segments for buffered
  writeback, and pinned user segments for direct I/O.
- A lease identifier accompanying every DMA-visible source. L4 retains the
  corresponding PC pin or user-page pin until the terminal completion.
- A dependency graph of bio nodes. Nodes carry a data source, block operation,
  LBA range, flags, and completion route; edges express must-complete-before
  submission ordering.
- A planner resume input, based on the existing resume token, so L5 can wait
  for metadata reads, allocator availability, journal credits, or prior graph
  nodes without exposing ext4 state to L4 or L6.

The old `Complete`, `SubmitBios`, and `MetadataFirst` shapes remain adapters
during migration. The new graph is the only representation allowed for a
durable ext4 write or fsync transaction.

## Page state and concurrency

`PageSlot` must distinguish `content_generation` from the generation submitted
for writeback. A write during an in-flight writeback redirties the slot instead
of making the old completion stale and leaving it in `Writeback` forever.

1. A write to a resident frame increments `content_generation` and marks it
   dirty under the per-slot lock.
2. L4 captures a frame lease and transitions the page to writeback with the
   submitted generation.
3. A later writer increments the content generation and sets `redirtied`.
4. Completion of the submitted generation returns the slot to `Resident` only
   if it was not redirtied; otherwise it returns to `Dirty`.

`fsync` snapshots a file-range generation frontier. It only completes when all
dirty generations at or below that frontier have completed the required durable
transaction. Later writes are not accidentally included. Range reservations
serialize truncate, fallocate/hole-punch, direct I/O, and fsync fences over
logical ranges; they do not replace per-page state.

## Data paths

### Buffered write and mmap

`write(2)` and a file-backed `mmap` store modify a PC-owned frame, mark its
slot dirty, and normally return before device I/O. File growth records ext4
allocation and metadata intent but does not perform a synchronous device call.
L4 later submits the frame segment to L5. Ext4 maps it to extents, produces data
bios, and records all inode/bitmap/extent mutations in the relevant journal
transaction. The driver DMA-reads the leased frame rather than an ext4-private
4 KiB staging copy.

### Writeback and completion

L4 selects dirty slots under its bounded cursor and priority policy, creates
an immutable request with source leases, and releases all locks before planning
or dispatch. L6 may merge only adjacent, same-device, same-operation bios that
do not cross a fence. Completion routes device tag to L6, graph node to L4,
then slot generation handling. Failure retains a retryable dirty/error state;
it never clears dirty data silently.

### fsync and power loss

Ordered JBD2 is required before ext4 advertises durable `fsync`:

1. Submit and wait for data writes covered by the fsync frontier.
2. Write the journal descriptor and metadata copies.
3. Make the commit record durable with FUA, or a write followed by flush when
   FUA is unavailable.
4. Complete fsync only after the commit record is durable.
5. Checkpoint journaled metadata to home locations later.

`fdatasync` may omit unrelated timestamp metadata but includes size, extent,
and allocation metadata. `syncfs` forms the same fence for the mount. Before
JBD2 ordered mode exists, ext4's write path is experimental and must not report
successful POSIX durability merely because a data bio completed. Mount replay
must expose only committed transactions after a power loss.

### O_DIRECT

Direct I/O bypasses PC data caching, not coherence. It acquires a range
reservation, blocks overlapping buffered changes, drains dirty/writeback slots,
uses pinned user segments as the source or target, and invalidates or updates
overlapping PC slots after completion. It still routes through ext4 mapping and
journal semantics, then L6 scheduling.

## Migration slices

| Slice | Change | Exit evidence |
|---|---|---|
| 6A | Neutral source lease, plan graph, resume contract | IR graph/fence/lease unit tests |
| 6B | Slot redirty and fsync-frontier semantics | same-page concurrent writeback tests |
| 6C | `Ext4BackendPlanner` read mapping and holes | read planner parity against compatibility path |
| 6D | Buffered writeback mapping and allocation intent | frame-source data writeback integration tests |
| 6E | JBD2 ordered transaction graph and replay | fault injection, remount, `e2fsck -n` |
| 6F | O_DIRECT, `msync`, `syncfs` coherence | overlap/reservation/direct-I/O tests |
| 6G | Callsite-by-callsite hot-path cutover | old/new parity and removal audit |

`FsPageBacking` remains the compatibility oracle through 6F. No ext4 direct
fetch/flush hot path is deleted before 6G. TxArray/EBR sparse-index migration is
separate follow-up work after PageSlot completion semantics are stable.

## Initial module topology

```text
crates/tx-subsystems/src/
  io_manager/page/{request,writeback,completion,service}.rs
  io_manager/block/{plan,queue,dispatch}.rs
  fs_iface/{plan,source,dependency}.rs
  page_backed/{slot,range_reservation}.rs
crates/tx-ext4/src/
  planner/{mod,read,write,transaction}.rs
  pager.rs                    # compatibility adapter until 6G
```

`tx-ext4-format` remains an on-disk parser/mutator helper. It must not gain
kernel PageContainer, VFS, reactor, or driver imports.

## Verification ladder

Each code slice runs focused crate tests, `cargo check` for the affected crate
set, `cargo xtask progress validate`, documentation lint when active docs
change, and `git diff --check`. Slices 6E-6G additionally require QEMU
fault-injection/remount witnesses, `e2fsck -n`, and Linux ext4 interoperability.

## Explicit deferrals

This design does not implement TxArray/RCU, change virtio IRQ/tag completion,
add multi-queue driver sharding, introduce a second ordinary-file-data cache,
or replace all `FsPageBacking` backends at once.
