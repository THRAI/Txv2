# Chapter 9 — Page-backed mappings and the page cache

Chapter 8's anonymous memory had *authoritative* contents — bytes born from a
write, recorded nowhere else. This chapter is the other kind: memory whose
contents are a **cache** of something durable — a file on disk, a tmpfs/shm
object in RAM, a device's MMIO window. Materializing it means fetching from (or
pointing at) that source. This is `filemap_fault` and the page cache in Linux;
in txKernel it is `VmBacking::Page` resolving through a `PageContainer`. It is
also where the `cache_ref` disjunct from Chapter 2 finally earns its keep.

This chapter overlaps the [VFS series' page-cache chapter](../vfs/07_page-cache-
and-mmap.md) — the page cache is shared machinery between the file system and the
VM. Here we look at it from the VM's side: how a fault turns a recipe's
`Page { offset }` into a frame.

## The backing: a `PageContainer`

A page-backed recipe's `owners.page` (Chapter 2) is a `Cap<PageContainer>`. A
`PageContainer` is the unit of cached pages — the analog of Linux's
`address_space` (the page-cache object hung off an inode), and the same object
the VFS series describes as a file's payload:

```rust
// page_backed/mod.rs:390
pub struct PageContainer {
    kind: PageContainerKind,
    page_count: ...,
    size_bytes: ...,
    state: ...,   // the page-index: offset → cached frame
}

// page_backed/mod.rs:279
pub enum PageContainerKind {
    Anon   { swap_policy },               // shared anonymous (MAP_SHARED | MAP_ANONYMOUS, shm)
    File   { mount, fs_object_id },       // a file on a mounted filesystem
    Device { base_ppn, page_count },      // an MMIO device window
}
```

The recipe stores only an *offset* into this container (`VmEntryBacking::Page
{ offset }`); the container itself is shared. The same `PageContainer` can back
many mappings in many address spaces (every process that `mmap`s the same file)
and the file system's own `read`/`write` path — which is the entire point of a
*cache*. The `VmCap<PageContainer>` (Chapter 2) is how a mapping holds it without
owning it.

`PageContainerKind` is the routing the VFS series called `RNodeBacking`, seen from
the page layer: anonymous-shared pages live only in RAM (no writeback target),
file pages fetch and flush through the mount's filesystem, device pages are a
fixed physical window with no allocation at all.

## Materializing a page-backed fault

When the fault handler (Chapter 7) hits a `Page`-backed recipe, the materialize
step calls into the container:

```rust
// page_backed/mod.rs:759 (and the fault-specific :690 / :702 step variants)
pub fn materialize_page_for_fault_step(&self, offset, access, guard)
    -> StepOutcome<MaterializedPage, ...>;
```

It returns:

```rust
// page_backed/mod.rs:306
pub struct MaterializedPage {
    pub ppn: Ppn,                    // the physical frame holding the content
    pub map_pin: MaterializedPagePin,// the MapPin the PTE will own
    pub newly_installed: bool,       // was this a cache miss?
    pub dirty: bool,
}
```

The dispatch by `kind`:

- **`File`** — look up `offset` in the container's page-index. **Hit:** the frame
  is already cached (another mapping or a `read` brought it in); return it,
  bumping a pin. **Miss:** issue a filesystem fetch through the mount — and *this
  can block on disk I/O*. That block is precisely why the fault handler drops its
  `RangeLock` reservation across materialization (Chapter 6/7): a disk read must
  not serialize the range. The fetched frame is installed into the page-index
  with `install_if_absent` (one frame per offset wins; a concurrent fault for the
  same page coalesces onto the winner), then returned.
- **`Anon`** (shared) — like file, but a miss allocates a fresh zeroed frame
  instead of fetching; there is no backing store to read from. (`MAP_SHARED |
  MAP_ANONYMOUS` and POSIX shm.)
- **`Device`** — no cache and no allocation: the frame is `base_ppn + offset`,
  the fixed physical page in the device window. Return a device pin.

The returned `map_pin` is then consumed by `publish_page_with_replacement`
(Chapter 4), which installs the PTE. The frame is now alive by *two* disjuncts: a
`CachePin` (the page-index holds it, `cache_ref > 0`) and a `MapPin` (the PTE holds
it, `map_count > 0`).

## `cache_ref` vs `map_count`: the disjunction, concretely

This is the chapter where Chapter 2's Frame disjunction stops being abstract. The
counters are real fields on `FrameMeta`, with real increment/decrement sites:

```rust
// page_allocator/frame_meta.rs
fn increment_map_count(&self) -> ...;    // :170  — PTE installed   (MapPin)
fn decrement_map_count(&self) -> ...;    // :174  — PTE torn down
fn increment_cache_ref(&self) -> ...;    // :178  — page-index inclusion (CachePin)
fn decrement_cache_ref(&self) -> ...;    // :182  — evicted from page-index
```

Trace one file page through its life and watch the two counts move independently:

| Event | `cache_ref` | `map_count` | live? |
|---|---|---|---|
| File `read` brings the page into the cache | 1 | 0 | yes (cached) |
| Process A `mmap`s and faults it | 1 | 1 | yes |
| Process B `mmap`s and faults it | 1 | 2 | yes |
| A `munmap`s | 1 | 1 | yes |
| B `munmap`s | 1 | **0** | **yes — still cached** |
| Cache reclaim evicts it | **0** | 0 | no → freed |

The row that matters is the second-to-last: **`map_count == 0` but `cache_ref >
0`** — nobody has it mapped, but it is still in the page cache, ready for the next
`read` or `mmap` without touching disk. In Linux this correctness is encoded in
the rules for one overloaded `_refcount`/`_mapcount` pair; in txKernel it is two
independent counters whose disjunction (`map_count > 0 ∨ cache_ref > 0`,
`object_model_v2.md:131`) *is* the liveness rule. "Unmap" and "evict" are
different events touching different counters — which is why `munmap` never has to
think about the page cache, and reclaim never has to walk page tables.

## Shared vs private file mappings

A file `mmap` is `MAP_SHARED` or `MAP_PRIVATE`, and the difference is entirely in
what a *write* fault does:

- **`MAP_SHARED`** — writes go to the cached frame itself. Every other sharer sees
  them, and they are flushed back to the file by `msync`/writeback. The recipe's
  `flags.shared` is true; the fault publishes a writable PTE straight onto the
  cached frame.
- **`MAP_PRIVATE`** — writes must *not* reach the file or other sharers. A write
  fault copies the cached page into a private frame and records it in the
  mapping's `PrivatePageSet` (Chapter 8), exactly like an anonymous CoW break —
  the page-backed frame was the read source, the private frame is the writable
  copy. This is why Chapter 8's machinery is shared between anonymous and
  private-file mappings: a private file page that has been written *is*
  authoritative, born from a write, recorded only in the set.

So the two backings meet here: a `MAP_PRIVATE` file mapping reads through the page
cache (`cache_ref`) until the first write, then diverges into a private frame
(`PrivatePageSet`) that behaves exactly like anonymous memory.

## Writeback: `msync`

Dirty `MAP_SHARED` file pages reach disk through `msync`, which the VM forwards to
the page layer per container:

```rust
// vm/execution.rs:1287  (simplified)
pub fn msync(&self, range, guard) -> StepOutcome<...> {
    for pc in unique_page_containers_overlapping(range) {
        match step_fsync(pc, guard) { ... }      // page_backed::step_fsync, may block
    }
}
```

`step_fsync` walks the container's dirty pages and flushes them through the
filesystem, which can block on I/O — so `msync` is a step that may yield, like the
fault handler. Anonymous and device containers have nothing to flush (no backing
store / direct MMIO); only `File` containers do real writeback. The VM's role
ends at "ask the page layer to flush this container's range"; the actual block I/O
is the filesystem's, across the same substrate boundary the VFS series describes.

## Where we go from here

That completes the two backings: anonymous (authoritative, Chapter 8) and
page-backed (cached, this chapter), meeting at the `MAP_PRIVATE`-file CoW break.
With the whole internal machine assembled — recipes, pmap, HAL, RangeLock, fault
handler, both backings — Chapter 10 turns outward to the syscall seam: how a raw
`mmap(2)` from userspace becomes a `VmMapOp` driven through all of this, and the
exec/ELF, gift, and userfaultfd surfaces built on top.

## Source anchors

- `PageContainer` / `PageContainerKind`: `crates/tx-subsystems/src/page_backed/mod.rs:390, 279`
- `materialize_page` (+ fault step variants): same file, `:759, 690, 702`
- `MaterializedPage` / `MaterializedPagePin`: same file, `:306, 352`
- `PageMarks` (dirty/writeback): same file, `:107`
- Frame counters: `crates/tx-substrate/src/page_allocator/frame_meta.rs:170, 174, 178, 182`
- `msync` → `step_fsync`: `crates/tx-subsystems/src/vm/execution.rs:1287`; `crates/tx-subsystems/src/page_backed/` (`step_fsync`)
- Frame disjunction (`map_count ∨ cache_ref`): `docs/design/00_meta-framework/object_model_v2.md:131`; `docs/design/01_substrate/PAGE_SUBSTRATE_v1.md` §3.1
- Page-backed model spec: `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
- Sibling page-cache chapter: `docs/tutorials/vfs/07_page-cache-and-mmap.md`
