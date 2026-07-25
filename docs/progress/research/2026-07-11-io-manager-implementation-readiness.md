# I/O manager implementation readiness

Date: 2026-07-11

Related design doc: `docs/design/05_filesystem/IO_MANAGER_v1.md`

## Verdict

Ready: mostly.

The architecture is ready for a first implementation slice, but only if the
first slice is staged as compatibility scaffolding plus PageContainer
synchronization work. It should not immediately replace `FsPageBacking`, should
not import concrete filesystem crates into `io_manager`, and should not attempt
TxArray/RCU before the slot and range semantics are stable.

## Current code facts

- `PageCacheIndex` is already a `BTreeMap<PageIndex, PageCacheEntry>` wrapper
  with a `SparseIndex` trait behind it
  (`crates/tx-subsystems/src/page_backed/mod.rs:207`,
  `crates/tx-subsystems/src/page_backed/sparse_index.rs:17`).
- `PageContainerState` still keeps the page index, in-flight fetch table, and
  page-ready wait table under one `SpinMutex`
  (`crates/tx-subsystems/src/page_backed/mod.rs:523`).
- File misses still call `FsPageBacking::fetch_page` directly from
  `PageContainer::materialize_file_page`
  (`crates/tx-subsystems/src/page_backed/mod.rs:997`).
- `FsPageBacking` is still a one-page `StepOutcome<T, NoProgress>` trait
  (`crates/tx-subsystems/src/page_backed/fs_page_backing.rs:34`).
- `BlockDeviceHandle` forwards directly to static `BlockDeviceOps`
  (`crates/tx-subsystems/src/device.rs:402` and `:414`).
- bdev-fs currently duplicates the same one-page read/write block loop across
  its staging surfaces (`crates/tx-fs/src/bdevfs/mod.rs:521` and `:977`).
- tx-ext4's kernel pager reads or writes one 4 KiB page through its current
  pager path (`crates/tx-ext4/src/pager.rs:103` and `:128`).

## Canonical gate

| Proposed item | Status | Canonical source |
|---|---|---|
| `SparseIndex` compatibility backend | existing staging | `IO_MANAGER_v1.md §7`, `page_backed/sparse_index.rs` |
| `PageSlot` | ready to add | `IO_MANAGER_v1.md §2`, §7, §12 |
| `RangeReservation` | ready to add | `IO_MANAGER_v1.md §7`, §8 |
| `PageIoRequest` / `PageIoCompletion` | ready to add | `IO_MANAGER_v1.md §4.1`, §13 |
| `Bio` / `BioPlan` | ready to add as values | `IO_MANAGER_v1.md §4.2`, §4.3, §13 |
| `io_manager::{page,backend,block,runtime}` | ready as empty/staged modules | `IO_MANAGER_v1.md §11` |
| `PagePager` successor to `FsPageBacking` | ready as target, not first behavior switch | `IO_MANAGER_v1.md §9`, `PAGE_BACKED_v1.md` current `FsPageBacking` staging contract |
| `kpageiod` / `kblockiod` service futures | ready after request queues exist | `IO_MANAGER_v1.md §5`, §12 |
| TxArray/RCU index backend | deferred | `IO_MANAGER_v1.md §12.7`, EBR substrate readiness |

## Blocking gaps

No architecture blocker blocks phase 0 or phase 1.

The following are blockers only for later behavior migration:

- `PagePager` exact trait spelling should be chosen only after `PageIoPlan` and
  `BioPlan` value tests exist.
- `BlockDeviceOps` still represents direct synchronous-style device calls; L6
  can model queues and merge first, but real driver tags and IRQ completion
  require a later device-facing change.
- Full RCU/TxArray requires stable `PageSlot` generation and state semantics.

## First implementation order

1. Add `io_manager` module skeleton and pure value types:
   `PageIoRequest`, `PageIoRange`, `PageIoPriority`, `PageIoCompletion`, `Bio`,
   `BioVec`, `BioPlan`, and small queue/budget helpers. Keep these detached
   from live PageBacked behavior until unit tests pass.
2. Add `page_backed::slot` and `page_backed::range` staging modules. Encode
   resident, fetching, dirty, writeback, error, and generation states in
   `PageSlot`; encode a locked interval table in `RangeReservation`.
3. Add focused host tests for same-page miss dedup, range conflict, direct-write
   invalidation policy, LBA adjacent merge, queue-depth blocking, and barrier
   ordering.
4. Refactor `PageContainerState` internally so the existing behavior can route
   through `PageSlot` and `RangeReservation` without changing public
   `materialize_page`, `step_read`, or `step_write` semantics.
5. Add L4 page-submission queue and completion application behind a feature or
   compatibility seam. At this point demand misses can still fall back to
   direct `FsPageBacking::fetch_page`.
6. Add L6 block queue and `BioPlan` production for bdev-fs first. Ext4 follows
   after the neutral IR is proven with raw block-device files.

## Concurrency rules for implementation

- Do not hold `PageContainer`, slot, index, or range locks while calling
  filesystem or device operations.
- Do not carry `RangeReservation` guards across `Yield`; commit, rollback, or
  release before yielding.
- Completion paths must re-check slot generation before publishing a fetched
  frame.
- Generic file-data readahead belongs to L4/PageContainer. ext4 may prefetch
  metadata windows, extent child nodes, and htree leaves, but it must not grow a
  second ordinary file-data cache.

## Suggested first verification ladder

```sh
cargo test -p tx-subsystems --lib page_backed -- --nocapture
cargo test -p tx-subsystems --lib io_manager -- --nocapture
cargo test -p tx-fs --lib bdevfs -- --nocapture
cargo xtask progress validate
cargo xtask lint docs
git diff --check -- <touched paths>
```
