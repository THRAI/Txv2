# Part 7 — The page cache and `mmap`

Chapter 6 ended a page-backed read at `page_backed::step_read_to_kernel` and
moved on. This chapter opens that box. The page cache is where a regular file's
*payload* actually lives — the thing Chapter 2 said is reached through
`RNodeBacking::PageBacked { pc }` and reclaimed independently of identity. It is
also the bridge between the filesystem and virtual memory: a file `mmap` and a
file `read` reach the *same* physical pages through the same container.

## `PageContainer`: the payload object

A regular file's `RNode` holds `PageBacked { pc: Cap<PageContainer> }`. The
`PageContainer` is the page cache for that one file (`page_backed/mod.rs:390`):

```rust
pub struct PageContainer {
    kind: PageContainerKind,        // Anon | File | Device — what backs the pages
    page_count: u64,                // capacity in pages
    size_bytes: AtomicU64,          // current logical size (the file's length)
    state: PageContainerStateCell,  // SpinMutex over the page map + in-flight fetches
}
```

The `kind` decides where a missing page comes from (`page_backed/mod.rs:279`):

```rust
pub enum PageContainerKind {
    Anon { swap_policy },                          // anonymous memory (heap, MAP_ANONYMOUS)
    File { mount: MountPayloadPin, fs_object_id }, // a file: pages fetched via FsPageBacking
    Device { base_ppn, page_count },               // MMIO aperture: pages are fixed, not allocated
}
```

For a file, `kind` is `File { mount, fs_object_id }` — and note `mount` is a
`MountPayloadPin`, the operational-evidence pin from Chapter 5. *The page cache
pins the mount payload it fetches from.* That is the lifetime rule made concrete:
while a file's pages are cached, the filesystem they came from cannot have its
payload reclaimed out from under them.

Inside the lock, the state is a sparse map plus fetch-coordination bookkeeping:

```rust
struct PageContainerState {
    pages: PageCacheIndex,                              // BTreeMap<PageIndex, PageCacheEntry>
    in_flight_file_pages: BTreeMap<PageIndex, FilePageFetch>, // fetches currently outstanding
    file_page_waits: BTreeMap<PageIndex, PageReadyWait>,// who to wake when a fetch lands
    next_file_fetch_id: u64,
}

struct PageCacheEntry { ppn: Ppn, pin: PageCachePin, marks: PageMarks }
struct PageMarks { dirty: bool, writeback: bool, referenced: bool, no_reclaim: bool }
```

`pages` is the cache: page index → physical page (`Ppn`) plus its `PageMarks`
(dirty, under writeback, recently referenced, pinned). This is the analogue of
Linux's `address_space.i_pages` (the xarray that was `radix_tree`), and
`PageMarks` are its `PG_dirty`/`PG_writeback`/`PG_referenced` page flags.

The whole object graph, from the inode down to a physical frame, and across to
the memory mapping that shares it:

```
   RNode (PageBacked)                         VmEntry (Page{offset})
      │ backing.pc                               │ owners.page (weak)
      ▼                                           ▼
   Cap<PageContainer> ◀───────────── shares the same container ──────────┐
      │                                                                   │
      ├ kind: File{ mount: MountPayloadPin, fs_object_id }  ──▶ pins MountPayload
      ├ size_bytes: AtomicU64        (authoritative file length)
      └ state: SpinMutex<PageContainerState>
                 │
                 ├ pages: BTreeMap<PageIndex, PageCacheEntry>
                 │            └─ PageCacheEntry { ppn: Ppn ─────▶ physical frame ◀── PTE maps here
                 │                               pin: PageCachePin   (one frame, two views)
                 │                               marks: dirty/writeback/referenced }
                 ├ in_flight_file_pages: BTreeMap<PageIndex, FilePageFetch>  (owner/joiner coord)
                 └ file_page_waits:      BTreeMap<PageIndex, PageReadyWait>  (joiners park here)
```

The thing to see: a file `read` reaches the frame via `RNode.backing.pc →
pages[i].ppn`; a file `mmap` reaches the *same* `ppn` via `VmEntry.owners.page →
pages[i].ppn` and installs it in a PTE. One frame, two paths — the basis for the
`mmap`/`read` coherence below.

> **Traditional VFS vs txKernel.** A Linux file's page cache hangs off
> `inode->i_mapping` — embedded in the inode, sharing its lifetime. A txKernel
> file's pages hang off a *separate* `PageContainer` reached through `backing`,
> with its own retention (`Cap<PageContainer>`) and its own pin on the mount
> payload. The unlinked-but-open file (Chapter 10's capstone) works because the
> container is an independent object: it outlives every name as long as the open
> edge holds it.

## Demand paging: `materialize_page`

Pages do not exist until touched. `materialize_page` (`page_backed/mod.rs:759`)
is the demand-paging entry point — "give me a physical page for page index *N*,
fetching it if necessary." It dispatches on `kind`:

```rust
fn materialize_page(&self, page: PageIndex, access: MaterializeAccess, guard)
    -> StepOutcome<MaterializedPage, NoProgress>
{
    match self.kind {
        Anon { .. }   => self.materialize_anon(page, access),        // allocate a zeroed frame
        File { .. }   => self.materialize_file_page(page, access, guard), // fetch from the fs
        Device { .. } => self.materialize_device_page(page),         // index the fixed aperture
    }
}
```

The result, `MaterializedPage { ppn, map_pin, newly_installed, dirty }`, carries
the physical page number *and a `MapPin`* — proof that the VM layer may install
this frame into a page table. That `map_pin` is the seam to `mmap` (below).

For an anonymous page the work is local: reserve a zeroed frame, acquire a
`CachePin`, install it in the map. For a **file** page it is the interesting
case, because the bytes must come from the backend and the backend may block.

### Fetching a file page, and coalescing concurrent faults

`materialize_file_page` cannot just call the backend, because *several* tasks
may fault the same page at once (two threads reading the same file, a reader and
an mmap fault). If each issued its own disk read, the cache would thrash and
race. The function is a single arc: decide my role, fetch if I'm the owner,
clean up on every exit. Faithfully (`page_backed/mod.rs:853`):

```rust
fn materialize_file_page(&self, page, access, mount: &MountPayloadPin, fs_object_id, guard)
    -> StepOutcome<MaterializedPage, NoProgress>
{
    let fetch_id = match self.begin_file_page_fetch(page, access) {
        FilePageFetchStart::Cached(materialized) =>           // someone already filled it
            return materialized.map_or_else(|e| Err(e.into()), Done),
        FilePageFetchStart::Joined(source_id) =>              // a fetch is in flight — park
            return notification::yield_on_page_ready_source(NoProgress, source_id),
        FilePageFetchStart::Owner(fetch_id) => fetch_id,      // I own this fetch
    };

    reclaim_clean_file_pages_if_low();                        // opportunistic eviction

    let offset = page.as_u64().checked_mul(USER_PAGE_SIZE)
        .ok_or_else(|| self.finish_file_page_fetch_without_install(page, fetch_id))?;

    match mount.payload().fs_page_backing.fetch_page(fs_object_id, offset, guard) {
        Done(frame) => self.install_fetched_file_page_from_owner(page, access, frame, fetch_id),
        Continue { .. } => {                                  // fs says "retry, no frame yet"
            self.finish_file_page_fetch_without_install(page, fetch_id);
            Err(EAGAIN)
        }
        Yield { shape, .. } => {                              // disk I/O outstanding
            self.finish_file_page_fetch_without_install(page, fetch_id);   // release ownership
            match notification::wait_source_parts(&shape) {
                Some((carrier, interests)) => notification::yield_on_wait_source(NoProgress, carrier, interests),
                None => Err(EIO),
            }
        }
        Err(e) => { self.finish_file_page_fetch_without_install(page, fetch_id); Err(e) }
    }
}
```

`begin_file_page_fetch` (`page_backed/mod.rs:916`) is the role election, under
the state lock:

```rust
if let Some(cached) = state.pages.get(page) { return Cached(..); }   // already present
if let Some(fetch) = state.in_flight_file_pages.get_mut(&page) {     // someone's fetching
    fetch.joined = true;
    return Joined(fetch.source_id);                                  // → park on its source
}
let fetch_id = state.next_file_fetch_id.bump();                      // I'm first
state.in_flight_file_pages.insert(page, FilePageFetch { id: fetch_id, .. });
return Owner(fetch_id);
```

Exactly one task becomes the **owner** and calls the backend; everyone else
**joins** and parks on the in-flight fetch's wait-source. The detail that makes
it correct: *every* non-success exit calls
`finish_file_page_fetch_without_install`, which removes the in-flight entry and
wakes the joiners — so if the owner's fetch yields or errors, the joiners don't
wait forever on a dead fetch; they wake, re-run `begin_file_page_fetch`, and one
of them becomes the new owner. Ownership is a baton, not a lock held to
completion.

On success the owner installs and wakes the joiners
(`install_fetched_file_page_from_owner`, `page_backed/mod.rs:1020`): convert the
`Frame` to a cached frame (acquire a `CachePin`), acquire a `MapPin`, then under
the lock `install_if_absent`. If a racing task already installed it, the owner
drops its frame and falls back to the cached one — the `install_if_absent`
guard makes the race benign. Finally it fires the `PageReadyWait`.

> **Traditional VFS vs txKernel.** Linux serialises concurrent faults on the
> same page with the per-page lock bit (`lock_page`/`PG_locked`): the first
> faulter locks the page, issues `readpage`, and others block on the lock.
> txKernel's `in_flight_file_pages` + owner/joiner split is the same idea
> expressed as explicit fetch coordination — one owner does the I/O, joiners
> park on a wait-source — but no task holds a lock across the I/O; they suspend
> as futures, and ownership is released (the baton passes) on yield/error.

## The read/write loop, page by page

`step_read`/`step_write` (`page_backed/mod.rs:1239`, `:1265`) clip the request
to the file size, then hand off to `step_range`, which walks the byte range one
page at a time (`page_backed/mod.rs:1305`):

```rust
fn step_range(pc, of, len, kind, guard) -> StepOutcome<usize, ByteProgress> {
    let mut advanced = 0; let mut offset = of.offset();
    while advanced < len {
        let page = PageIndex::new(offset / PAGE_SIZE);
        let within = offset % PAGE_SIZE;
        let chunk  = min(len - advanced, PAGE_SIZE - within);
        match pc.materialize_page(page, access, guard) {
            Done(_) => { advanced += chunk; offset += chunk; }       // page ready; copy at call site
            Yield { shape, .. } => {                                  // page needs I/O
                of.set_offset(offset);
                return yield_on_wait_source(ByteProgress::new(advanced), shape); // partial progress
            }
            Err(e) => return if advanced == 0 { Err(e) } else { of.set_offset(offset); Done(advanced) },
        }
    }
    of.set_offset(offset); Done(advanced)
}
```

Two details earn their place:

- **Partial progress is preserved.** If page 0 is resident but page 1 must be
  fetched, the loop copies page 0, records `advanced` in the `ByteProgress`, and
  yields. When resumed, it continues from the saved offset — a half-completed
  `read` does not restart. This is the `P = ByteProgress` half of the step model
  (vs `NoProgress` for the one-shot `FsOps` queries in Chapter 3).
- **Size grows after the bytes land.** `step_write` calls `grow_size_to(start +
  advanced)` after a successful chunk (`page_backed/mod.rs:1223`), a CAS loop
  that only ever raises `size_bytes`. The authoritative file length lives in the
  `PageContainer`, not in the cached `InodeMeta` — which is exactly why
  `tmpfs::load_inode_meta` (Chapter 3) reads the size back from the container.

The actual byte copy happens at the call-site layer — `step_read_to_kernel`
(kernel buffer) or `step_read_to_user`/`copy_chunk_user` (user buffer, via
`AddressSpace::copy_to_user`, which can itself yield if the *user* page is not
present). The page cache produces a resident frame; a separate, explicit step
moves bytes across the user/kernel boundary.

## `mmap`: the file and the address space share frames

A file `read` copies cache pages into a buffer. A file `mmap` maps the *same*
cache pages straight into the process address space — no copy. The link is in
the VM layer's `VmEntry` (Chapter on VM covers it fully; here is the seam):

```rust
pub enum VmEntryBacking {
    None,
    PrivateAnon,
    Page { offset: u64 },   // backed by a PageContainer at this offset
}
```

A file mapping is a `VmEntry` whose backing is `Page { offset }`, holding a
(weak) reference to the file's `PageContainer`. On a page fault in that range,
the VM fault handler computes the page index and calls **the same**
`materialize_page` path the read uses (`vm/structure/types.rs:1072`):

```rust
match pc.materialize_page_for_fault_step(page_index, access, guard) {
    Done(materialized) => pmap.install(fault_va, materialized.ppn, prot), // map the cache frame
    Yield { .. }       => suspend,                                        // fetch in flight
    ...
}
```

So a `MAP_SHARED` file mapping and a `read()` of the same file reach one
physical frame: the fault handler installs the very `Ppn` the page cache holds.
Write through the mapping dirties that cache page; a later `read` sees it; an
`fsync` writes it back. There is one copy of the file's bytes in memory, shared
between the file abstraction and the memory abstraction — because both go
through the `PageContainer`.

`MAP_PRIVATE` differs only at write time: reads still come from the shared cache
frame, but the first *write* triggers copy-on-write — the fault handler
allocates a private frame, copies the cache page into it, and maps the private
copy. The shared cache page is untouched. (This is the
`VmEntry.private_pages` COW path noted in project memory — the reflink/COW
machinery lives in the VM and fs layers, not in the page container itself.)

> **Traditional VFS vs txKernel.** This is structurally the Linux story:
> `mmap` of a file points the VMA at `inode->i_mapping`, and `filemap_fault`
> populates PTEs from the same page cache that `read` uses. txKernel keeps the
> identity — one set of frames, shared between file and memory views — but the
> shared object is the standalone `PageContainer` rather than the inode-embedded
> `address_space`, so its lifetime is governed by `Cap`/pin retention rather
> than the inode's.

## Writeback, `fsync`, and truncate

A write only dirties a cache page (`PageMarks.dirty = true`); it does not touch
the backend. Persistence is `step_fsync` (`page_backed/lifecycle.rs:92`):

```rust
fn step_fsync(pc, guard) -> StepOutcome<(), PageProgress> {
    let File { mount, fs_object_id } = pc.kind() else { return Done(()) };  // anon: nothing to flush
    for (page, ppn) in pc.dirty_pages_snapshot() {                          // snapshot dirty set
        match mount.payload().fs_page_backing.flush_page(fs_object_id, page.offset(), &Frame::new(ppn), guard) {
            Done(()) => { pc.clear_dirty_if_match(page, ppn); pages_done += 1; }
            Yield { shape, .. } => return yield_on_wait_source(PageProgress::new(pages_done), shape),
            Err(e) => return Err(e),
        }
    }
    // then persist size via FsPageBacking::truncate(size)
}
```

`PageProgress` counts flushed pages, so a large `fsync` that yields mid-way
resumes where it left off — the same partial-progress discipline as reads, with
pages as the unit instead of bytes. `clear_dirty_if_match` only clears the dirty
mark if the `Ppn` still matches, so a write that races the flush keeps the page
dirty (it will be flushed next time) rather than silently losing the update.
`sync_filesystem` (the `syncfs` backend) defaults to `fsync_file(ROOT)`;
journaling backends override it to barrier all dirty inodes at once.

`truncate` (`FsPageBacking::truncate`) shrinks or grows the file: shrinking drops
the cache pages past the new end and tells the backend; growing just raises
`size_bytes` (reads past old EOF now return zeros, materialised on demand).

## Source anchors

- `PageContainer` / `PageContainerKind`: `crates/tx-subsystems/src/page_backed/mod.rs:390,279`
- `PageContainerState` / `PageCacheEntry` / `PageMarks`: `crates/tx-subsystems/src/page_backed/mod.rs:408,114,107`
- `materialize_page`: `crates/tx-subsystems/src/page_backed/mod.rs:759`
- fetch coalescing (`begin_file_page_fetch`, owner/joiner, install): `crates/tx-subsystems/src/page_backed/mod.rs:916,877,1020`
- `step_read`/`step_write`/`step_range`/`grow_size_to`: `crates/tx-subsystems/src/page_backed/mod.rs:1239,1265,1305,1223`
- user-buffer copy (`step_read_to_user`, `copy_chunk_user`, `step_read_to_kernel`): `crates/tx-subsystems/src/page_backed/user_buffer.rs:15,285,421`
- `step_fsync` / `dirty_pages_snapshot`: `crates/tx-subsystems/src/page_backed/lifecycle.rs:92,49`
- `VmEntryBacking::Page` + fault materialise: `crates/tx-subsystems/src/vm/structure/types.rs:353,1072`
- mmap syscall: `crates/tx-shims/src/linux_syscall/vm.rs`
- COW discipline: project memory `[[feedback_cow_design]]`
- Spec: `docs/design/03_memory-vm/PAGE_BACKED_v1.md`
