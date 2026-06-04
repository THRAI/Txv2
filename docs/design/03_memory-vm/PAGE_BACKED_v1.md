# Page Backed

<!-- txdoc:03-MEMORY-VM-PAGE-BACKED-V1 -->

**Status.** v1 (2026-04-19). Draft.

**Purpose.** Specify `PageContainer` — the offset-keyed Frame store — and the three-variant `RNodeBacking` that factors every RNode into (page-backed, struct-backed, projected). This document supersedes the retired "Inode + FileOps injection" model, eliminates the `FileOps` and `InodeOps` vtables, and unifies persistent files, tmpfs, shm, memfd, anonymous mmap, and MMIO-mapped devices into a single page-backed shape.

**Scope.** Everything above the page substrate (frame allocator, FrameMeta, kernel heap, pmap — see [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md)) and below the syscall scripts. Specifically:

- `PageContainer` structure, lifecycle, and API.
- `PageContainerKind` — the three variants (Anon, File, Device).
- `RNodeBacking` — the three-variant classification of what content an RNode has.
- The uniform step functions for page-backed RNodes (read, write, mmap, truncate, fsync).
- The narrow `FsPageBacking` trait for the File variant's dispatch into filesystem code.
- Cross-variant operations (splice, copy_file_range, sendfile).
- Reflink and sharing semantics.

This document does *not* cover:

- Frame allocation or FrameMeta CAS discipline — see [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md).
- Pmap operations (PTE install, teardown, shootdown) — see [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md) §7.
- Struct-backed subsystems (pipe, socket, tty, eventfd, timerfd, signalfd, epoll) — see their individual subsystem specs.
- Projected RNode content (procfs, sysfs, devpts) — see the projection FS specs.
- VmEntry, AddressSpace, pmap, or the VM subsystem's step functions — see forthcoming `VM_v1_2.md`.
- The filesystem oracle contract — see forthcoming `FSOPS.md`; this document specifies only the narrow page-backing slice.

**Audience.** VM subsystem implementers, VFS implementers, filesystem-instance authors, anyone writing a step function that reads, writes, or mmaps page-backed content.

**Key commitments** (established in prior rounds):

1. **No shadow objects.** COW is expressed via PTE manipulation, not via stacked PageContainers. A MAP_PRIVATE mapping that writes generates a fresh private Frame that is installed in the VmEntry's private-frame tracking, not in the source PageContainer.
2. **No Pager trait.** Per-variant logic is handled by match-on-kind. The only genuine polymorphism is `FsPageBacking` for filesystem-specific page fetch, which is a narrow trait used only by `PageContainerKind::File`.
3. **Eager prefault is preserved.** Syscall scripts prefault user buffers during the observe phase. Step functions do not take kernel-mode page faults on user buffers; they either have the pages materialized and memcpy, or return `Blocked` waiting for materialization.
4. **No swap.** Anonymous pages are pinned until explicit teardown. The Anon variant has no flush-to-disk path; its `evict` does nothing (the CachePin drop + map_count transitions handle reclamation naturally).
5. **Stackless coroutines.** Every step function returns `StepOutcome<T>`. Page fetches that need I/O return `Blocked(carrier, mask)`; the script composes a wait; the step retries after wake.

6. **Page-backed code stays above raw address arithmetic.** This document may
   talk about page indexes, file offsets, `Frame` evidence, and typed user
   buffers, but it does not own pmap layout, direct-map arithmetic, or user
   pointer dereference. Page-backed operations obtain content through
   `Cap<PageContainer>`, `CachePin`/Frame evidence, and VM/user-access helper
   gates.

**Companion documents.**

- [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md) — frame allocator, FrameMeta, pmap substrate this doc consumes.
- [`object_model.md`](../00_meta-framework/object_model_v2.md) §3.3 — compound payload predicates (Frame.payload_live = map_count > 0 ∨ cache_ref > 0 ∨ pin_count > 0).
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — step outcome algebra, five-phase discipline.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — STEP-4, OBL-*, BIF-*.
- [`MODULE_MAP_v1.md`](../00_meta-framework/MODULE_MAP_v1.md) §5.1 (vm subsystem), §7 (FS instances).

### Zone-derived type policy
<!-- txdoc:PAGE-BACKED-ZONE-DERIVED-TYPE-POLICY -->

PAGE_BACKED follows the common policy-zone rule, but only for entities with
independent reclamation:

| Page-backed declaration | Zone-derived public type | Reclamation role |
|---|---|---|
| `RNode` | `Cap<RNode>`, `Weak<RNode>`, `IdentRef<'g, RNode>` | namespace/content identity for VFS |
| `PageContainer` | `Cap<PageContainer>` plus `CachePin` participation from frames | page-indexed content owner |
| `Frame` | typed contributors such as `MapPin`, `CachePin`, `DmaToken` | compound payload entity owned by page substrate |
| `RNodeBacking::StructBacked` payload | owning subsystem's role-shaped cap, such as `Cap<TtyIdentity>` | no PAGE_BACKED policy choice |
| `RNodeBacking::Projected` key | projection key plus schema reference | not retention; target is re-observed under guard |
| `FsPageBacking` / pseudo-device schemas | `&'static` trait object or schema | static fact, no zone |

This document does not introduce `Zone<T, Policy>` at dispatch sites. Backing
variants carry the role-shaped evidence supplied by their owning subsystem.

### Address boundary policy
<!-- txdoc:PAGE-BACKED-ADDRESS-BOUNDARY-POLICY -->

PAGE_BACKED lives above PAGE_SUBSTRATE and below syscall scripts. Its native
coordinates are semantic and content-relative: RNode evidence, PageContainer
evidence, page indexes, byte offsets, and Frame role evidence. It should not
manufacture kernel pointers from `Ppn`/`PhysAddr`, compute direct-map addresses
inline, or treat user buffers as ordinary Rust references.

When bytes move:

- user buffers arrive as typed user-buffer/user-pointer values owned by the
  syscall, then move through the eager-walk
  `AddressSpace::copy_*_user` methods (which materialise each user
  page through its `VmEntry.backing` and copy through the kernel
  direct-map view);
- page content is reached by asking PAGE_SUBSTRATE/VM helpers for a direct-map
  copy source/destination or by materializing a Frame for pmap installation;
- gifted user pages arrive only as VM-produced `UserPageGift` tokens. The token
  proves that VM has materialized the page, retained the frame with substrate
  transfer evidence, and frozen the old writable user materialization before
  PAGE_BACKED sees the frame;
- device-backed page containers may carry device `Ppn` facts, but those facts
  remain mapping inputs, not allocator ownership or ordinary pointer authority.

Thus PAGE_BACKED still speaks txKernel's semantic language at its public
surface: `Cap<RNode>`, `Cap<PageContainer>`, `Weak<T>`, `IdentRef<'g, T>`, and
role-shaped Frame tokens. Address values appear only at the handoff to
PAGE_SUBSTRATE, VM, or user-access helpers.

---

## 1. Motivation
<!-- txdoc:PAGE-BACKED-1-MOTIVATION -->

Traditional Unix kernels attach two vtables to every Inode:

- `InodeOps`: lookup, create, unlink, rename, symlink, readlink, getattr.
- `FileOps`: read, write, lseek, mmap, ioctl, poll, fsync, release.

These are bundled per-Inode with per-open `FileOps` swap at `open()` time to handle device nodes and special files. The model is deeply conventional but has structural problems:

- **God-class Inode.** The same struct must serve persistent files, anonymous memory, pipes, sockets, ttys, and more. Each class needs different fields; Linux packs them into unions and tagged variants, producing a ~1 KB Inode struct with 70% of fields unused per instance.
- **Vtable swap on open.** The Inode declares default `FileOps`, but device nodes replace them at open time. Any reasoning about "what does read do on this file?" must track whether the Inode, the device subsystem, or some other registrar has the last word.
- **Anonymous memory is a fake file.** Anonymous mmap and memfd synthesize an Inode with a stub FileOps just to satisfy the mmap machinery. The Inode is a ghost; it has no on-disk presence, no dentry (or only a synthetic one), no meaningful InodeOps.
- **Page cache as Inode field.** The page cache is a field on Inode (`i_mapping` / `address_space`). Anonymous memory gets one anyway; tmpfs gets one; shm gets one. Each is special-cased.

The observation that unlocks a cleaner model: **the namespace axis (what this RNode is named) and the content axis (what data lives here) are orthogonal**. Two classifications, composed.

**Namespace axis** (RNode presence): persistent FS path, tmpfs path, device node path, anonymous (no path), synthetic (procfs/sysfs), or named-but-anonymous-ish (memfd, shm).

**Content axis** (what the data looks like): offset-keyed pages (files of all kinds, anonymous memory, MMIO); stream/event-shaped subsystem data (pipes, sockets, ttys, eventfds); or no stored content, just a view of other state (procfs, sysfs).

These two axes are independent. A file has a path-namespace presence and page-indexed content; an anonymous mmap has no namespace and page-indexed content; a pipe has an optional synthetic path (via pipefs) and stream content; `/proc/<pid>/status` has a path and projected content.

`PageContainer` is the content-axis primitive for the page-indexed case. `RNodeBacking` classifies which of the three content types an RNode has.

---

## 2. RNodeBacking
<!-- txdoc:PAGE-BACKED-2-RNODEBACKING -->

The three-variant classification.

```rust
pub enum RNodeBacking {
    /// Page-indexed data. Reads/writes/mmap are uniform across all instances.
    /// The PageContainer is the payload; its kind distinguishes anon / file / device.
    PageBacked {
        pc: Cap<PageContainer>,
    },

    /// Subsystem-specific payload with custom step functions. Pipes, sockets,
    /// ttys, etc. No shared read/write code; each subsystem implements its own.
    StructBacked {
        payload: StructPayload,
    },

    /// No stored content. Reads and (rarely) writes go through a projection
    /// schema that computes content from other subsystem state. Procfs, sysfs,
    /// devpts, and pseudo-devices (/dev/null, /dev/zero, etc.).
    Projected {
        schema: &'static dyn ProjectionSchema,
        key: ProjectionKey,
    },
}

pub enum StructPayload {
    Pipe(Cap<PipeData>),
    Socket(Cap<SocketIdentity>),
    Tty(Cap<TtyData>),
    EventFd(Cap<EventFdData>),
    TimerFd(Cap<TimerFdData>),
    SignalFd(Cap<SignalFdData>),
    Epoll(Cap<EpollData>),
    // Char devices with custom semantics (/dev/random is the main example).
    CharDevice(Cap<CharDeviceBinding>),
}

pub struct ProjectionKey {
    pub object_id: ObjectId,  // the subsystem object being projected (e.g., ProcessIdentity slot+gen)
    pub file_type: u32,       // which projection within the schema (e.g., "status" = 0, "maps" = 1)
}
```

`RNode` holds a `RNodeBacking` field set at creation time. There is no "default backing" or "backing injection at open" — the backing is part of the RNode's identity.

### 2.1 Dispatch pattern
<!-- txdoc:PAGE-BACKED-2-1-DISPATCH-PATTERN -->

Every file-op step function on an RNode is a match on its backing variant:

```rust
pub fn step_read(
    ctx: &ThreadContext,
    of: &OpenFile,
    buf: UserBuf,
    len: usize,
) -> StepOutcome<usize> {
    let rnode = of.rnode.upgrade()?;
    match &rnode.backing {
        RNodeBacking::PageBacked { pc } => {
            page_backed::step_read(pc, &of, buf, len, ctx)
        }
        RNodeBacking::StructBacked { payload } => {
            match payload {
                StructPayload::Pipe(p)      => pipe::step_read(p, buf, len, ctx),
                StructPayload::Socket(s)    => net::step_recv(s, buf, len, 0, ctx),
                StructPayload::Tty(t)       => tty::step_read(t, buf, len, ctx),
                StructPayload::EventFd(e)   => eventfd::step_read(e, buf, ctx),
                StructPayload::TimerFd(t)   => timerfd::step_read(t, buf, ctx),
                StructPayload::SignalFd(s)  => signalfd::step_read(s, buf, ctx),
                StructPayload::Epoll(_)     => Err(Errno::EINVAL),  // epoll is not read via read(2); use epoll_wait
                StructPayload::CharDevice(c) => c.ops.step_read(c, buf, len, ctx),
            }
        }
        RNodeBacking::Projected { schema, key } => {
            projection::step_read(*schema, key, &of, buf, len, ctx)
        }
    }
}
```

No vtable on the RNode. The match is in the script (step_read lives in scripts/file_io.rs or in vfs/execution/step_read.rs, both acceptable under MODULE_MAP).

### 2.2 Which ops exist per variant
<!-- txdoc:PAGE-BACKED-2-2-WHICH-OPS-EXIST-PER-VARIANT -->

Not every op makes sense for every variant. The match-on-backing pattern naturally handles "this op is not applicable" by returning the appropriate errno:

| Op | PageBacked | StructBacked | Projected |
|---|---|---|---|
| read | uniform | per-payload | projection invoke |
| write | uniform (Anon, File) / EINVAL (Device by default) | per-payload | projection invoke or EINVAL |
| lseek | uniform (bounded by PC.size) | per-payload (mostly ESPIPE) | fixed (treat as stream or as bounded text) |
| mmap | uniform (install VmEntry) | per-payload (usually ENODEV) | EINVAL (mostly) |
| ioctl | EINVAL | per-payload | EINVAL |
| poll | readiness of first page (usually always ready) | per-payload | projection-defined |
| fsync | uniform (PC writeback for File variant, noop for Anon/Device) | per-payload (mostly noop) | noop |
| truncate | uniform (PC range-invalidate) | EINVAL | EINVAL |
| fallocate | uniform (File variant) | ENOSPC | EINVAL |

"Uniform" here means there is one implementation in `page_backed::step_*` shared across all PageBacked instances. Per-variant behavior within PageBacked (anon vs file vs device) is a match on `PageContainerKind` inside that uniform implementation.

### 2.3 RNode lifecycle under backing
<!-- txdoc:PAGE-BACKED-2-3-RNODE-LIFECYCLE-UNDER-BACKING -->

`RNode` is a zone-allocated entity with its own SlotMeta and retention. The backing field references another entity (`Cap<PageContainer>`, `Cap<PipeData>`, etc.) that has its own lifecycle.

**PageBacked:** the RNode holds a `Cap<PageContainer>`. The PC persists as long as at least one Cap is held. When the RNode is reclaimed (last holder drops), the PC's refcount decrements; if the PC was only held by this RNode, the PC reclaims too. Multiple RNodes may hold Caps on the same PC (for reflink across persistent filesystems — rare but representable).

**StructBacked:** the RNode holds a `Cap<PipeData>` etc. The subsystem payload typically outlives the RNode in some cases (a pipe created by `pipe()` has PipeData from the moment of creation, but pipefs lazily synthesizes an RNode on `/proc/<pid>/fd/` access; the RNode is then a view). The Cap ensures the payload stays alive while the RNode references it.

**Projected:** the RNode holds a schema reference (`&'static dyn ProjectionSchema`) and a projection key (identifying the target object). The target object is not held by the RNode; projection reads re-observe the target's state via the subsystem's projection functions under an epoch guard. If the target is reaped, subsequent reads on the projected RNode return `ENOENT` or `ESRCH` depending on projection semantics.

---

## 3. PageContainer
<!-- txdoc:PAGE-BACKED-3-PAGECONTAINER -->

The zone-allocated entity that owns an offset-keyed collection of Frames.

### 3.1 Structure
<!-- txdoc:PAGE-BACKED-3-1-STRUCTURE -->

```rust
pub struct PageContainer {
    /// Zone slot metadata (refcount, generation, SENTINEL_DEAD).
    meta: SlotMeta,

    /// Offset → Frame. Offsets are byte offsets (aligned to page size).
    ///   - For File and Anon variants: offset is content offset.
    ///   - For Device variant: offset is into the MMIO region, starting from 0.
    pages: PageCacheIndex<PageIndex, Cap<Frame>>,

    /// Valid byte range; reads and writes beyond this return SIGBUS on mmap
    /// access or EOF/0-len on read. Updated by truncate, fallocate, and
    /// PC creation.
    ///
    /// AtomicU64 because concurrent readers may observe size transitions;
    /// truncate is the only writer under normal operation.
    size: AtomicU64,

    /// PageContainer-wide flags.
    ///
    /// Bit 0: has_dirty_pages (any page in `pages` is dirty; hint for writeback
    ///         scheduling, not authoritative)
    /// Bit 1: writeback_in_progress (an async writeback is running; observational)
    /// Bit 2: truncate_in_progress (a truncate is running; observers re-check)
    flags: AtomicU32,

    /// Variant-specific data.
    kind: PageContainerKind,
}
```

**Zone and SlotMeta.** Each PC occupies one slot in a `BitmapZone<PageContainer>`. Retention via `Cap<PageContainer>`; SENTINEL_DEAD on the final Cap drop. No Identity/Payload split — PC is co-located (no "degraded-but-addressable" state; PC is either fully alive or fully gone).

**Pages index.** `PageCacheIndex<PageIndex, Cap<Frame>>` is the one place in v1 where an XArray-like sparse index is intentional. It is VM/PageContainer-local: sparse ordered lookup by page index, install-if-absent / install-if-match publication, withdrawal, iteration for reclaim/writeback, and optional non-authoritative marks such as dirty, writeback, referenced, or no-reclaim. Offsets are page-aligned byte offsets shifted right by PAGE_SHIFT (12), then keyed by page index. The index is the single linearization point for "is there a materialized Frame at this offset?"

Each entry in the page index is a Frame reference. The Frame's cache_ref is incremented when inserted, decremented on removal. The PC holds CachePins on every Frame in its page index.

**Conservative XArray scope.** The XArray-like substrate is not a general
kernel object directory and not the implementation contract for pid namespaces,
fd tables, or zone metadata. Those layers may use their own simpler
reservation/index structures. The PC page index gets this richer shape because
file and device page caches need sparse offset lookup, ordered reclaim walks,
and per-entry marks.

**Size.** Governs the legal offset range for this PC. Truncate-down invalidates pages beyond the new size; truncate-up extends the valid range (pages are not pre-materialized). Atomic reads let concurrent readers observe size without a lock.

**Flags.** Coarse-grained hints for writeback and reclaim schedulers. Not authoritative state; per-page dirty/io-locked flags on FrameMeta are the ground truth.

### 3.2 PageContainerKind
<!-- txdoc:PAGE-BACKED-3-2-PAGECONTAINERKIND -->

```rust
pub enum PageContainerKind {
    /// Anonymous memory: zero-filled on first access, never written back.
    /// Covers MAP_ANONYMOUS, memfd, SYSV shm, POSIX shm, tmpfs.
    /// 
    /// The `swap_policy` flag distinguishes tmpfs (eviction permitted only
    /// via truncate/unlink, not under memory pressure) from other anon
    /// uses (eviction under memory pressure would be "drop and retake
    /// zero" — under no-swap, this is the same as not evicting, so the
    /// flag is reserved for future use).
    Anon {
        swap_policy: AnonSwapPolicy,
    },

    /// File-backed: pages fetched from and (if dirty) written back to a
    /// filesystem's backing store via FsPageBacking.
    /// 
    /// Holds the filesystem-instance reference (to dispatch fs-specific
    /// calls) and the fs-internal identifier (e.g., inode number for ext4,
    /// object id for object-stores).
    File {
        fs: Cap<MountPayload>,
        fs_object_id: u64,
    },

    /// MMIO mapping: Frames are device-owned physical pages, never allocated
    /// or freed by the frame allocator. The PC page index maps offsets into
    /// a device's MMIO region; each Frame is a passthrough wrapper.
    /// 
    /// Used for /dev/fb0 (framebuffer), DRI card nodes for direct GPU
    /// memory access, /dev/mem (if enabled). Also /dev/zero via a special
    /// sub-case (see §3.3).
    Device {
        device: Cap<DevNode>,
        base_ppn: PPN,       // first physical page of the device region
        page_count: u32,     // number of pages in the region
    },
}

pub enum AnonSwapPolicy {
    /// Pages are freed when their cache_ref and map_count both reach zero.
    /// This is the default for anonymous mmap, memfd, SYSV shm.
    Reclaimable,
    
    /// Pages persist until explicitly removed (truncate, unlink-and-close).
    /// Memory pressure does not evict. Used for tmpfs.
    Persistent,
}
```

### 3.3 Notes on specific uses
<!-- txdoc:PAGE-BACKED-3-3-NOTES-ON-SPECIFIC-USES -->

**Anonymous memory.** `PageContainerKind::Anon { swap_policy: Reclaimable }`. Created by `MAP_ANONYMOUS`, `memfd_create`, `shmget`, `shm_open`. Under no-swap, `Reclaimable` and `Persistent` are nearly equivalent — there's no swap to evict to, so eviction would mean "lose the data," which is not an option for live pages. The flag is a hint to future memory-pressure logic; in v1, all Anon variants behave the same way (pages persist until teardown).

**tmpfs.** `PageContainerKind::Anon { swap_policy: Persistent }`. Same as anonymous mmap except the PC is attached to an RNode with a path-namespace presence. File operations (read, write, mmap) work uniformly.

**Persistent filesystem (ext4).** `PageContainerKind::File { fs, fs_object_id }`. Pages fetched via `FsPageBacking::fetch_page()`. Dirty pages tracked; writeback issued periodically or on fsync.

**Device framebuffer, DRI.** `PageContainerKind::Device { device, base_ppn, page_count }`. No allocation; the Frames in the PC page index wrap pre-existing device-owned PPNs.

**/dev/zero.** Two implementation options:

- *Option A (Projected).* RNode with `Projected` backing, schema that returns zero bytes on read, discards writes, and (if mmap support is desired) synthesizes a read-only VmEntry pointing at a well-known zero Frame.
- *Option B (PageBacked with AnonPager-alike variant).* A PC with Anon kind whose pages are never allocated; on access, the well-known zero Frame is installed read-only.

I lean Option A (Projected). It's simpler — no PC, no zone allocation for a stateless synthetic device. `mmap(/dev/zero, PROT_READ | PROT_WRITE, MAP_PRIVATE)` works because MAP_PRIVATE means the first write generates a private Frame (per VmEntry tracking), and reads from not-yet-written pages read from the zero Frame. Both ends handled by the VM layer's fault handler, not by a real PC.

Spec decision: **/dev/zero is Projected, not PageBacked.** Documented here for clarity; the details live in the Projected spec.

**/dev/null.** Projected. Read returns 0 (EOF); write discards all bytes; mmap returns EINVAL (no sensible semantics).

**Shared memory objects held only by fds, no path (memfd).** `PageContainerKind::Anon`. The memfd has an RNode synthesized at creation (required so fd operations work; the RNode carries the backing). The RNode has no path-namespace presence. `memfd_create` returns an fd; the PC is held by the OpenFile via the RNode.

---

## 4. PageContainer lifecycle
<!-- txdoc:PAGE-BACKED-4-PAGECONTAINER-LIFECYCLE -->

### 4.1 Creation
<!-- txdoc:PAGE-BACKED-4-1-CREATION -->

`PageContainer::new(kind, initial_size)` constructs a new PC in a zone slot:

```rust
pub fn new_page_container(
    kind: PageContainerKind,
    initial_size: u64,
) -> Result<Cap<PageContainer>, Errno> {
    let slot = zone::reserve::<PageContainer>()?;
    let pc = PageContainer {
        meta: SlotMeta::new(),
        pages: PageCacheIndex::new(),
        size: AtomicU64::new(initial_size),
        flags: AtomicU32::new(0),
        kind,
    };
    let cap = zone::sign(slot, pc);
    Ok(cap)
}
```

Creation contexts:

- **mmap(MAP_ANONYMOUS):** `new_page_container(Anon { swap_policy: Reclaimable }, len)` called from vm's step_mmap.
- **memfd_create:** same as anon mmap, plus an RNode wrapping it.
- **open(O_CREAT) on persistent fs:** filesystem creates on-disk inode, then wraps it in PC via `new_page_container(File { fs, fs_object_id }, file_size_from_inode)`.
- **open() on existing persistent file:** fs looks up its PC for this inode (may be cached; the fs instance maintains an inode → PC map); if no PC exists yet, creates one. Subsequent opens of the same file share the PC.
- **open() on tmpfs file:** same as persistent but with Anon/Persistent.
- **open() on device node (/dev/fb0):** device driver provides `Device { device, base_ppn, page_count }`; PC is created to wrap the device's MMIO region.

The PC is *not* automatically populated with Frames. Frames materialize lazily on first access (read, fault, etc.).

### 4.2 Cap<PageContainer> holders
<!-- txdoc:PAGE-BACKED-4-2-CAP-PAGECONTAINER-HOLDERS -->

A PC is referenced by:

- **RNode.backing.PageBacked.pc** — one Cap per RNode that names this content.
- **VmEntry.backing.pc** (via an indirect reference — VmEntry carries an RNode Cap which carries the PC Cap, or a direct PC Cap for anonymous mappings with no RNode).
- **Reflink holders** — rare; if two persistent filesystems both point at the same PC (filesystem reflink operation), each holds a Cap.

When the refcount reaches zero, the PC enters reclaim: all Frames in its page index are released (their cache_refs decremented), then the PC's zone slot is freed.

### 4.3 Reclamation
<!-- txdoc:PAGE-BACKED-4-3-RECLAMATION -->

PC reclamation:

1. Last Cap<PageContainer> drops. SENTINEL_DEAD CAS on the PC slot.
2. PC's `Drop` impl iterates the page index, releasing each Frame's CachePin.
3. For each Frame released:
   - cache_ref decrements.
   - If the Frame's state hits zero (no map_count, no refcount, no pin_count either), the Frame's PPN returns to the frame allocator.
4. PC zone slot returns to its zone.

**Bounded-work discipline.** If the PC has a large number of pages (gigabyte file cache, for instance), releasing all at once would exceed the step's latency budget. The reclaim queue (object_model §6.3) handles this: the PC's `Drop` enqueues a reclaim request; the reclaim queue drains Frames in bounded batches, yielding between batches.

Under no-swap, reclaim is relatively rare for anonymous PCs (they persist until explicit teardown). File PC reclaim is common (file caches evict under memory pressure); see §8 on reclaim.

### 4.4 Size and bounds
<!-- txdoc:PAGE-BACKED-4-4-SIZE-AND-BOUNDS -->

PC.size is the authoritative valid byte range. Operations consult it:

- **Read at offset ≥ size:** returns 0 (EOF).
- **Write at offset ≥ size (for Anon and File):** extends size; new pages beyond old size are allocated or fetched on write.
- **Write at offset ≥ size (for Device):** returns EINVAL (device region is fixed).
- **mmap with offset+len > size:** may be allowed by flags (MAP_POPULATE), otherwise causes SIGBUS on access to pages beyond size.
- **Truncate to smaller size:** invalidates pages beyond new size (see §5.3).

---

## 5. Range operations
<!-- txdoc:PAGE-BACKED-5-RANGE-OPERATIONS -->

### 5.1 Read
<!-- txdoc:PAGE-BACKED-5-1-READ -->

Reads are multi-step: one step per page (or chunk of pages) within the user buffer.

```rust
pub fn step_read(
    pc: &Cap<PageContainer>,
    of: &OpenFile,
    buf: UserBuf,
    len: usize,
    ctx: &ThreadContext,
) -> StepOutcome<usize> {
    let guard = epoch::guard();
    let pc = pc.upgrade(&guard)?;
    
    let start_offset = of.offset.load(Ordering::Acquire);
    let valid_end = pc.size.load(Ordering::Acquire);
    
    if start_offset >= valid_end {
        return Done(0);  // EOF
    }
    
    let effective_len = core::cmp::min(
        len as u64,
        valid_end - start_offset,
    ) as usize;
    
    let mut advanced = 0;
    let mut offset = start_offset;
    
    while advanced < effective_len {
        let page_offset = offset & !(PAGE_SIZE as u64 - 1);
        let within_page = (offset & (PAGE_SIZE as u64 - 1)) as usize;
        let to_copy = core::cmp::min(
            effective_len - advanced,
            PAGE_SIZE - within_page,
        );
        
        match materialize_page(&pc, page_offset, &guard) {
            Done(frame) => {
                // Copy via direct map. User buf was prefaulted during script observe.
                let src = ppn_to_vaddr(frame.ppn()).add(within_page);
                copy_to_user(buf.advance(advanced), src, to_copy);
                advanced += to_copy;
                offset += to_copy as u64;
            }
            Blocked(c, m) => {
                if advanced > 0 {
                    of.offset.store(offset, Ordering::Release);
                    return AdvancedThenBlocked(Progress(advanced), c, m);
                }
                return Blocked(c, m);
            }
            Err(e) => {
                if advanced > 0 {
                    of.offset.store(offset, Ordering::Release);
                    return Done(advanced);
                }
                return Err(e);
            }
        }
    }
    
    of.offset.store(offset, Ordering::Release);
    Done(advanced)
}
```

The key call is `materialize_page(pc, offset, guard) -> StepOutcome<Frame>`. This is where the per-variant logic lives:

```rust
fn materialize_page<'g>(
    pc: &Cap<PageContainer>,
    offset: u64,
    guard: &'g Guard,
) -> StepOutcome<IdentRef<'g, Frame>> {
    let page_index = (offset >> PAGE_SHIFT) as u64;
    
    // Fast path: already in the PC page index.
    if let Some(frame) = pc.pages.lookup(page_index, guard) {
        return Done(frame);
    }
    
    // Miss. Dispatch by kind.
    match &pc.kind {
        PageContainerKind::Anon { .. } => {
            // Allocate zeroed frame, install.
            match install_new_frame(pc, page_index, /* zero = */ true, guard) {
                Ok(frame) => Done(frame),
                Err(e) => Err(e),
            }
        }
        PageContainerKind::File { fs, fs_object_id } => {
            // Dispatch to filesystem's page fetcher.
            match fs.upgrade(guard)?.page_backing().fetch_page(*fs_object_id, offset, guard) {
                Done(frame_ready) => {
                    install_prefilled_frame(pc, page_index, frame_ready, guard)
                }
                Blocked(c, m) => Blocked(c, m),
                Err(e) => Err(e),
            }
        }
        PageContainerKind::Device { device, base_ppn, page_count } => {
            // Device region: wrap the appropriate PPN directly.
            if page_index >= *page_count as u64 {
                return Err(Errno::EINVAL);
            }
            let ppn = PPN(base_ppn.0 + page_index as u32);
            match install_device_frame(pc, page_index, ppn, guard) {
                Ok(frame) => Done(frame),
                Err(e) => Err(e),
            }
        }
    }
}
```

Three dispatch arms:
- **Anon**: allocate a frame from the frame allocator, zero it, install in the PC page index. Synchronous; the only failure is allocation failure (ENOMEM).
- **File**: call into the filesystem's `FsPageBacking::fetch_page`, which may return `Blocked` on disk I/O. If it returns a ready frame, install it. The frame was allocated by the fs or by this code; the fs-returned frame is already populated with disk content.
- **Device**: compute the device PPN, construct a Frame wrapping it (no allocation), install in the PC page index.

Installation is via `install_if_match` on the PC page-index slot: if another thread installed first, we drop our candidate frame and re-observe.

### 5.2 Write
<!-- txdoc:PAGE-BACKED-5-2-WRITE -->

Write is symmetric to read, with three additions:

1. **Extending size.** If offset + len exceeds PC.size, size is bumped via CAS. For Device variant, writes beyond size return EINVAL.
2. **Dirty tracking.** Written pages have FrameMeta.flags.dirty set. For File variant, this marks the page for writeback. For Anon variant, dirty has no meaning (no writeback target).
3. **Copy-on-write for shared pages.** If the target offset currently holds a Frame that is shared (cache_ref > 1, indicating reflink), the write must allocate a new Frame, copy the shared content, install the new Frame. This is reflink-induced CoW; see §7.

Other than that, step_write mirrors step_read in shape.

### 5.3 Truncate
<!-- txdoc:PAGE-BACKED-5-3-TRUNCATE -->

Truncate adjusts PC.size and drops pages beyond the new size.

```rust
pub fn step_truncate(
    pc: &Cap<PageContainer>,
    new_size: u64,
    ctx: &ThreadContext,
) -> StepOutcome<()> {
    let guard = epoch::guard();
    let pc = pc.upgrade(&guard)?;
    
    // For File variant, ask fs first — some filesystems limit truncate
    // (e.g., read-only mounts return EROFS; some fs check quotas).
    match &pc.kind {
        PageContainerKind::File { fs, fs_object_id } => {
            fs.upgrade(&guard)?
              .page_backing()
              .truncate(*fs_object_id, new_size, &guard)?;
        }
        PageContainerKind::Device { .. } => {
            return Err(Errno::EINVAL);  // can't truncate MMIO regions
        }
        _ => {}
    }
    
    let old_size = pc.size.swap(new_size, Ordering::AcqRel);
    pc.flags.fetch_or(FLAG_TRUNCATE_IN_PROGRESS, Ordering::AcqRel);
    
    if new_size < old_size {
        // Drop pages beyond new_size.
        let first_drop_index = (new_size + PAGE_SIZE as u64 - 1) >> PAGE_SHIFT;
        let last_drop_index = (old_size - 1) >> PAGE_SHIFT;
        
        for page_index in first_drop_index..=last_drop_index {
            if let Some(frame) = pc.pages.remove(page_index, &guard) {
                // Drop of `frame` releases this PC's CachePin on the Frame.
                // If that was the last cache_ref and no map_count or refcount
                // holds the Frame, FrameMeta.state reaches zero and
                // free_frame is called.
                drop(frame);
            }
        }
    }
    
    pc.flags.fetch_and(!FLAG_TRUNCATE_IN_PROGRESS, Ordering::AcqRel);
    Done(())
}
```

For File variant, the filesystem must also update its on-disk metadata (file size, block allocation). That happens inside `FsPageBacking::truncate` before the in-memory PC shrinks.

**Concurrent readers during truncate.** A reader observing size before the truncate sees the old size; after the truncate, the new size. Pages in the truncated range may still be in the reader's hand (epoch-protected), but the reader's offset check against size catches it: if the reader's offset is now beyond size, it returns short. No page materialization is attempted for offsets ≥ size.

**Concurrent mmaped writers during truncate.** Pages mapped into address spaces (map_count > 0) have MapPins independent of the PC's CachePin. Removing the page from the PC page index decrements cache_ref but the MapPin keeps the Frame alive. The mmap holders continue to access the Frame through their PTEs until they unmap or get SIGBUS on next fault beyond the new size. (POSIX permits but does not require SIGBUS on access to truncated-away mappings; implementations vary. We implement SIGBUS-on-fault-beyond-size via the fault handler checking PC.size.)

### 5.4 Fsync
<!-- txdoc:PAGE-BACKED-5-4-FSYNC -->

Writeback all dirty pages in the PC to backing storage. Only meaningful for File variant.

```rust
pub fn step_fsync(
    pc: &Cap<PageContainer>,
    ctx: &ThreadContext,
) -> StepOutcome<()> {
    let guard = epoch::guard();
    let pc = pc.upgrade(&guard)?;
    
    match &pc.kind {
        PageContainerKind::File { fs, fs_object_id } => {
            fs.upgrade(&guard)?
              .page_backing()
              .fsync(*fs_object_id, &guard)
        }
        PageContainerKind::Anon { .. } => Done(()),  // no backing
        PageContainerKind::Device { .. } => Done(()),  // device handles its own sync
    }
}
```

The filesystem's `fsync` implementation walks the PC's pages (passed implicitly via `fs_object_id`), issues writeback for any dirty ones, and (for File) flushes any on-disk metadata. Returns `Blocked` while disk I/O is in flight.

### 5.5 Fallocate
<!-- txdoc:PAGE-BACKED-5-5-FALLOCATE -->

Reserve space for future writes. Only meaningful for File and Anon variants.

For File: the filesystem may pre-allocate on-disk blocks. The PC's size grows to the allocated bound; pages are not materialized until accessed.

For Anon: the PC's size grows; pages are still lazily allocated on access. Anonymous fallocate is mostly a hint.

For Device: returns EINVAL.

---

## 6. FsPageBacking
<!-- txdoc:PAGE-BACKED-6-FSPAGEBACKING -->

The narrow trait for filesystem-specific page fetch and writeback. This is the only polymorphism needed for the File variant.

```rust
pub trait FsPageBacking {
    /// Fetch the page at `offset` into a freshly-allocated Frame.
    /// Returns Done(frame) if content is available (from cache or
    /// synchronous fetch); Blocked if disk I/O is needed.
    fn fetch_page<'g>(
        &self,
        fs_object_id: u64,
        offset: u64,
        guard: &'g Guard,
    ) -> StepOutcome<Frame>;

    /// Write a dirty page back to backing storage.
    fn flush_page<'g>(
        &self,
        fs_object_id: u64,
        offset: u64,
        frame: &Frame,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Truncate the backing file to `new_size`. May fail (EROFS, EDQUOT, etc.)
    /// before any page-level state is touched.
    fn truncate<'g>(
        &self,
        fs_object_id: u64,
        new_size: u64,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Flush all dirty pages for this object to backing storage and
    /// commit any outstanding metadata (for ordered / metadata-journal
    /// filesystems).
    fn fsync<'g>(
        &self,
        fs_object_id: u64,
        guard: &'g Guard,
    ) -> StepOutcome<()>;

    /// Capability query: does this fs support reflink across to `other`?
    /// Default: no.
    fn supports_reflink(&self, other: &PageContainer) -> bool {
        false
    }
}
```

Implementations:

- **rsext4::PageBacking:** connects to ext4 on-disk format. `fetch_page` issues a block read via the block device; `flush_page` writes back. `reflink` is false (ext4 doesn't support reflink).
- **Future tmpfs-as-fs:** not needed — tmpfs uses Anon variant, not File, so it doesn't participate in this trait.

Most filesystems we care about for Linux 2.6 parity are local block-backed. Network filesystems (nfs, cifs, fuse) would add their own impls when we get to them.

**The trait is narrow.** Four step-returning methods plus one capability predicate. No vtable for read, write, lseek, ioctl — those go through the uniform page_backed step functions and only dispatch into fs at fetch/flush/truncate/fsync points.

---

## 7. Reflink and sharing
<!-- txdoc:PAGE-BACKED-7-REFLINK-AND-SHARING -->

Reflink lets two PCs share Frames at specific offsets. The shared Frames are CoW from the writer's perspective — first write on either side allocates a new Frame and redirects.

### 7.1 When reflink happens
<!-- txdoc:PAGE-BACKED-7-1-WHEN-REFLINK-HAPPENS -->

**`copy_file_range` with supporting filesystems:** btrfs, xfs support reflink. If the source and dest PCs are both File-variant with the same fs that supports reflink, the script can request reflink semantics; the fs implementation installs source Frames into dest's page index with cache_ref increment.

**`ioctl(FICLONE)` / `ioctl(FICLONERANGE)`:** explicit user-requested reflink.

Under our model, the source PC is unaffected. The dest PC's page index gets entries pointing at the source PC's Frames. Each shared Frame's cache_ref goes from 1 (source) to 2 (source + dest).

PTEs pointing at newly-shared Frames must be downgraded to read-only, so that a write fault gives us a chance to CoW. This is the pmap side of reflink: for each VmEntry pointing at the source PC that has pages in the shared range, walk PTEs and clear the write bit. Expensive; proportional to mapped pages.

### 7.2 CoW on write
<!-- txdoc:PAGE-BACKED-7-2-COW-ON-WRITE -->

A write into a VmEntry backed by a shared Frame (cache_ref > 1, or PTE is read-only despite the VMA being writable) takes a fault. The fault handler:

1. Identifies the target page via VmEntry + offset -> PC -> page in the PC page index.
2. Checks the Frame's cache_ref. If > 1 (shared), allocate a new Frame.
3. Copy source Frame content to new Frame.
4. Install new Frame in the PC page index at the same offset (via `install_if_match`; concurrent CoW from another writer on another PC is resolved by the page index's linearization).
5. Install writable PTE pointing at new Frame in the faulting VmEntry's pmap.
6. The old shared Frame's cache_ref decrements (we removed our entry).

Under this protocol, each PC eventually has its own Frame for the written page; the source PC's Frame is freed when all reflinks have been written away.

### 7.3 Non-reflink copy
<!-- txdoc:PAGE-BACKED-7-3-NON-REFLINK-COPY -->

If the fs doesn't support reflink, `copy_file_range` falls back to page-by-page copy: for each page in the range, fetch source page, allocate dest Frame, copy bytes, install in dest's page index. No sharing. Semantically equivalent to reflink from the user's perspective, just without the memory-saving.

### 7.4 Reflink is not a PC-level operation
<!-- txdoc:PAGE-BACKED-7-4-REFLINK-IS-NOT-A-PC-LEVEL-OPERATION -->

There is no `PageContainer::reflink_into(other_pc, src_range, dst_range)` primitive. Reflink is an fs-level operation (filesystem knows whether it supports reflink and how to record the on-disk aliasing). The PC's role is to participate in the shared-Frame state: multiple PCs with entries pointing at the same Frame.

---

## 8. Reclaim under memory pressure
<!-- txdoc:PAGE-BACKED-8-RECLAIM-UNDER-MEMORY-PRESSURE -->

Under no-swap, reclaim options are limited. This section describes what's available; the actual reclaim policy (when to reclaim, which PC's pages to pick) is a separate concern deferred to a future doc.

### 8.1 What can be reclaimed
<!-- txdoc:PAGE-BACKED-8-1-WHAT-CAN-BE-RECLAIMED -->

| Kind | Can reclaim? | Mechanism |
|---|---|---|
| Anon (Reclaimable) | Only if map_count and cache_ref both reach zero | Explicit teardown (munmap, PC drop) |
| Anon (Persistent, tmpfs) | Only via truncate or PC drop | Explicit |
| File, clean page | Yes | Drop from PC page index; CachePin decrements; Frame freed if unmapped |
| File, dirty page | After writeback | Flush via FsPageBacking, then same as clean |
| Device | No | Frames are device-owned; not allocator-managed |

Under memory pressure, the reclaim target is almost exclusively **clean file pages**. Dirty file pages require a writeback step first. Anonymous pages cannot be reclaimed (in v1, under no-swap).

### 8.2 Reclaim trigger
<!-- txdoc:PAGE-BACKED-8-2-RECLAIM-TRIGGER -->

Reclaim is synchronous: an `alloc_frame` failure triggers the allocation path to request reclaim.

```rust
fn alloc_frame_with_reclaim() -> Option<PPN> {
    if let Some(ppn) = alloc_frame() { return Some(ppn); }
    
    // Try to reclaim clean file pages.
    reclaim::reclaim_clean_file_pages(CLEAN_RECLAIM_BUDGET);
    
    if let Some(ppn) = alloc_frame() { return Some(ppn); }
    
    // Try harder: writeback-then-reclaim dirty pages.
    reclaim::writeback_and_reclaim(DIRTY_RECLAIM_BUDGET);
    
    alloc_frame()  // final attempt; None if still out
}
```

The reclaimer walks a global list of PC-owned pages (LRU-like, approximated by a per-PC dirty list and global age counters), picks candidates, invalidates their page-index entries, potentially issues writeback.

### 8.3 Reclaim vs concurrent access
<!-- txdoc:PAGE-BACKED-8-3-RECLAIM-VS-CONCURRENT-ACCESS -->

Reclaiming a page removes it from the PC page index. A concurrent reader observing the same offset may:

- Find the page (observed before reclaim's remove linearization) and continue.
- Find an empty slot (observed after remove) and trigger re-materialization via `materialize_page`. For File variant, this refetches from disk; for Anon, this would reallocate and zero — but anonymous reclaim doesn't happen in v1, so this path is unreachable. For Device, the device PPN is stable; reclaim doesn't remove Device entries.

This is architecturally fine: the PC page index is the linearization point. Any state visible through it is safe to use; anything missing triggers re-fetch.

---

## 9. Cross-variant scripts
<!-- txdoc:PAGE-BACKED-9-CROSS-VARIANT-SCRIPTS -->

Some syscalls cross variant boundaries. Splice, sendfile, copy_file_range all have this shape. They live in scripts/file_io.rs (or wherever scripts are placed per MODULE_MAP §8), not in any single subsystem.

### 9.1 splice
<!-- txdoc:PAGE-BACKED-9-1-SPLICE -->

Between two fds; at least one must be a pipe. Pipe owns ordered waitable
transport; PageBacked owns page leases and install/copy policy. Common cases:

- **Pipe → File (page-backed):** if the front pipe descriptor carries a full
  page-aligned `PageLease`, PageBacked installs it into the destination PC when
  the target page is absent, or copies into the resident destination frame when
  policy requires fallback. Ordinary anonymous pipe buffers and unaligned
  ranges use the byte-copy path.
- **Gifted user page → Pipe → File (page-backed):** `vmsplice(SPLICE_F_GIFT)`
  asks VM for `UserPageGift` tokens. Pipe stores those tokens as ordered
  descriptor payloads; it does not inspect, freeze, or install the frame.
  PageBacked consumes each token through
  `PageContainer::install_user_gift_or_copy`: install the gifted frame into an
  absent destination slot when policy permits, or copy into the resident
  destination frame and drop the gift when policy requires fallback.
- **File (page-backed) → Pipe:** full page-aligned file ranges export a
  PageBacked-owned `PageLease` and enqueue it as a pipe descriptor. Partial,
  unaligned, and unsupported ranges use the byte-copy path.
- **Pipe → Pipe / tee:** moves or duplicates pipe descriptors while preserving
  stream order and wait-source semantics.

The script dispatches:

```rust
pub fn step_splice(ctx, in_fd, in_off, out_fd, out_off, len) -> StepOutcome<usize> {
    let in_of = resolve_fd_readable(in_fd)?;
    let out_of = resolve_fd_writable(out_fd)?;
    
    match (&in_of.rnode.backing, &out_of.rnode.backing) {
        (RNodeBacking::StructBacked { payload: StructPayload::Pipe(p) },
         RNodeBacking::PageBacked { pc }) => {
            splice::pipe_to_pc(p, pc, out_of, len, ctx)
        }
        (RNodeBacking::PageBacked { pc },
         RNodeBacking::StructBacked { payload: StructPayload::Pipe(p) }) => {
            splice::pc_to_pipe(pc, in_of, p, len, ctx)
        }
        (RNodeBacking::StructBacked { payload: StructPayload::Pipe(p1) },
         RNodeBacking::StructBacked { payload: StructPayload::Pipe(p2) }) => {
            pipe::step_splice(p1, p2, len, ctx)
        }
        // POSIX requires at least one end to be a pipe:
        _ => Err(Errno::EINVAL),
    }
}
```

Pipe capacity is a resizable descriptor ring (`F_GETPIPE_SZ` /
`F_SETPIPE_SZ`), defaulting to 16 page slots. Ordinary `write(2)` uses
anonymous pipe pages with tail-slot merge for small writes; `PIPE_BUF` writes
reserve all required capacity before publishing bytes. Notification/watchqueue
pipes are a separate future pipe mode and do not share the byte-stream storage
variants.

`vmsplice(SPLICE_F_GIFT)` is stealable only after VM returns `UserPageGift`
tokens per `VM_v1_2.md` `txdoc:VM-9-12-USER-PAGE-GIFTS-FOR-VMSPLICE`.
Until that primitive is wired in code, the syscall may accept the flag as
compatibility and copy user iov bytes into pipe buffers, but it must not
advertise the copied buffers as gifted pages. Once wired, release ownership
flows with the token: pipe drops the descriptor on ordinary stream discard, and
PageBacked consumes it on install/copy completion.

### 9.2 sendfile
<!-- txdoc:PAGE-BACKED-9-2-SENDFILE -->

From a file (typically) to a socket or pipe. Source is always PageBacked; dest is usually StructBacked. Copy is page-by-page.

### 9.3 copy_file_range
<!-- txdoc:PAGE-BACKED-9-3-COPY-FILE-RANGE -->

Between two files. If both are PageBacked and same fs with reflink support, use reflink path; else page-by-page copy.

```rust
pub fn step_copy_file_range(ctx, in_fd, in_off, out_fd, out_off, len)
    -> StepOutcome<usize>
{
    let in_of = resolve_fd_readable(in_fd)?;
    let out_of = resolve_fd_writable(out_fd)?;
    
    let (in_pc, out_pc) = match (&in_of.rnode.backing, &out_of.rnode.backing) {
        (RNodeBacking::PageBacked { pc: p1 },
         RNodeBacking::PageBacked { pc: p2 }) => (p1, p2),
        _ => return Err(Errno::EINVAL),  // copy_file_range requires both ends files
    };
    
    // Check reflink eligibility.
    let guard = epoch::guard();
    if let PageContainerKind::File { fs, .. } = &in_pc.upgrade(&guard)?.kind {
        if fs.upgrade(&guard)?.page_backing().supports_reflink(out_pc.upgrade(&guard)?) {
            return copy_file_range::reflink(in_pc, in_off, out_pc, out_off, len, ctx);
        }
    }
    
    // Fallback: page-by-page copy.
    copy_file_range::copy(in_pc, in_off, out_pc, out_off, len, ctx)
}
```

---

## 10. VM-subsystem interface
<!-- txdoc:PAGE-BACKED-10-VM-SUBSYSTEM-INTERFACE -->

The VM subsystem uses PageContainers through two patterns: the fault handler reads pages for installation, and mmap / munmap manage VmEntries that reference PCs.

### 10.1 mmap
<!-- txdoc:PAGE-BACKED-10-1-MMAP -->

mmap creates a VmEntry referencing a PC. For MAP_ANONYMOUS, a fresh PC is created. For file-backed mmap, the fd's OpenFile's RNode's PC is referenced.

```rust
// Sketch: step_mmap
pub fn step_mmap(ctx, addr, len, prot, flags, fd, offset) -> StepOutcome<VAddr> {
    let pc = if flags & MAP_ANONYMOUS != 0 {
        new_page_container(PageContainerKind::Anon { swap_policy: Reclaimable }, len)?
    } else {
        let of = resolve_fd(fd)?;
        match &of.rnode.backing {
            RNodeBacking::PageBacked { pc } => pc.clone(),
            _ => return Err(Errno::ENODEV),  // can't mmap struct-backed or projected (mostly)
        }
    };
    
    let vm_entry = vm::new_vm_entry(pc, offset, len, prot, flags);
    let vaddr = vm::install_vm_entry(ctx.addr_space, addr, vm_entry)?;
    Done(vaddr)
}
```

### 10.2 Fault
<!-- txdoc:PAGE-BACKED-10-2-FAULT -->

On access to an unmapped page in a VmEntry, the fault handler:

1. Computes the offset into the VmEntry's PC.
2. Calls `materialize_page(pc, offset, guard)` — same function used by read/write.
3. On Done(frame), installs a PTE pointing at the frame with appropriate permissions.
4. On Blocked, returns Blocked to the script (which is the page-fault handling script running as a Future under the reactor; it composes the wait and retries).

### 10.3 MAP_PRIVATE and CoW
<!-- txdoc:PAGE-BACKED-10-3-MAP-PRIVATE-AND-COW -->

MAP_PRIVATE means writes don't propagate to the PC. The fault handler detects MAP_PRIVATE + write, allocates a private Frame (from frame allocator), copies from the PC's Frame (if any), installs the private Frame in the PTE without inserting it into the PC page index.

The private Frame is tracked per-VmEntry (see the VM subsystem's own data structures). It's not in any PC page index; its cache_ref is never incremented. Its map_count is 1 (installed in this pmap) and increments on fork (if the child inherits the mapping). On munmap or process exit, the PTE is torn down, map_count decrements; when it reaches zero, the Frame is freed.

---

## 11. What this replaces and what survives
<!-- txdoc:PAGE-BACKED-11-WHAT-THIS-REPLACES-AND-WHAT-SURVIVES -->

### 11.1 Retired
<!-- txdoc:PAGE-BACKED-11-1-RETIRED -->

- **FileOps vtable on Inode.** The per-instance read/write/lseek/mmap/ioctl function pointer table. Now: match on RNodeBacking.
- **InodeOps vtable on Inode.** The per-fs lookup/create/unlink function pointer table. Now: FsOps on the fs instance (MODULE_MAP §7), not per-Inode.
- **FileOps injection at open time.** Used by devices to swap FileOps per open. Now: backing variant is fixed at RNode creation; device nodes are Device-variant PageBacked or CharDevice StructBacked from the start.
- **`address_space_operations` analog.** Linux's per-Inode page-cache operations table. Now: FsPageBacking on the fs instance.
- **Fake Inodes for anonymous memory.** SYSV shm, POSIX shm, memfd all synthesized Inodes to plug into the FileOps machinery. Now: direct RNode with PageBacked(Anon) backing, no filesystem involved.
- **Separation between file mmap and anonymous mmap code paths.** Linux has distinct code paths for file-backed vs anonymous mmap, converging late. Now: one step_mmap, one fault handler, two cases (fresh PC for anon, shared PC for file).

### 11.2 Kept
<!-- txdoc:PAGE-BACKED-11-2-KEPT -->

- **RNode identity.** Zone-allocated, refcounted, generation-tagged. Same machinery.
- **OpenFile per-open state.** Offset, flags, cloexec. Same.
- **FsOps on fs instance.** For path-lookup, directory operations, etc. — unchanged; orthogonal to page-backing.
- **Inode number.** As `fs_object_id` in PageContainerKind::File. Same meaning.
- **Page cache concept.** Just moved from Inode field to PageContainer entity.

### 11.3 Net change
<!-- txdoc:PAGE-BACKED-11-3-NET-CHANGE -->

Lines of code: significantly less. Linux's address_space_operations has ~20 methods per filesystem; FsPageBacking has 4. Per-instance FileOps per Inode (1 KB+): eliminated. The machinery for "what does read do on this file?" collapses from "look up FileOps, maybe injected, vtable-call read" to "match on RNodeBacking, call uniform or subsystem-specific step_read."

---

## 12. Open questions and future work
<!-- txdoc:PAGE-BACKED-12-OPEN-QUESTIONS-AND-FUTURE-WORK -->

### 12.1 Reclaim policy
<!-- txdoc:PAGE-BACKED-12-1-RECLAIM-POLICY -->

What's the heuristic for which clean file pages to evict under memory pressure? LRU is standard but requires maintaining LRU lists (overhead). Random replacement is simpler but less effective. CLOCK or second-chance is a reasonable middle. Deferred to a reclaim-specific doc.

### 12.2 Writeback scheduling
<!-- txdoc:PAGE-BACKED-12-2-WRITEBACK-SCHEDULING -->

When are dirty file pages flushed to disk? Options: on fsync only (simple, but risks data loss), periodic (every N seconds, bounded staleness), pressure-driven (before reclaim), all-of-the-above. Linux uses all; we can start simple.

### 12.3 Truncate race against reclaim
<!-- txdoc:PAGE-BACKED-12-3-TRUNCATE-RACE-AGAINST-RECLAIM -->

Truncate removes pages from the PC page index. Reclaim also removes pages. Concurrent truncate and reclaim on the same page: first-wins via page-index linearization; the loser observes an already-empty slot and proceeds. Safe by construction, but worth a test.

### 12.4 Reflink under concurrent truncate
<!-- txdoc:PAGE-BACKED-12-4-REFLINK-UNDER-CONCURRENT-TRUNCATE -->

If a PC is being reflinked while simultaneously being truncated, the reflink-source pages in the truncate range are dropped from the source PC page index but remain in the dest PC page index (they already got copied in). This is correct behavior: dest has its own reference. Just worth noting.

### 12.5 FsPageBacking for network filesystems
<!-- txdoc:PAGE-BACKED-12-5-FSPAGEBACKING-FOR-NETWORK-FILESYSTEMS -->

NFS, 9P, etc. have different fetch semantics (server-side state, attribute caching, write-behind). Needs per-fs spec. Not in v1 scope.

### 12.6 Large pages
<!-- txdoc:PAGE-BACKED-12-6-LARGE-PAGES -->

Would a PageContainer entry ever be a 2 MB or 1 GB superpage rather than a 4 KiB frame? For file caches, usually no (alignment rarely matches). For anonymous mmap with MAP_HUGETLB, yes. The PC page index would need to accommodate variable-size entries. Deferred; v1 is all 4 KiB.

### 12.7 Character device `CharDeviceBinding` semantics
<!-- txdoc:PAGE-BACKED-12-7-CHARACTER-DEVICE-CHARDEVICEBINDING-SEMANTICS -->

The minimal per-char-device vtable. What's the minimum interface? Probably: step_read, step_write, step_ioctl, step_mmap (optional), step_poll. Not urgent; add as specific devices are implemented.

---

## 13. Summary
<!-- txdoc:PAGE-BACKED-13-SUMMARY -->

**Three variants of RNodeBacking** cover every kind of content:

- **PageBacked** — offset-keyed pages, uniform implementation.
- **StructBacked** — subsystem-specific payload, per-subsystem step functions.
- **Projected** — no stored content, view via projection schema.

**PageContainer** is the zone entity for offset-keyed page storage. Three kinds: Anon, File, Device. No Pager trait; dispatch on kind is a match. The only polymorphism is FsPageBacking, a narrow 4-method trait for filesystem-specific fetch/flush.

**Uniform step functions** handle read, write, lseek, mmap, truncate, fsync, fallocate for all PageBacked content. Per-variant behavior is a match inside the uniform code. Struct-backed content routes to subsystem-specific implementations. Projected content invokes projection schemas.

**No shadow objects, no FileOps vtable, no InodeOps vtable.** Linux's layered caching and vtable swapping are replaced by static variant dispatch.

**Reflink, splice, sendfile, copy_file_range** are scripts that match on both ends' backing variants and dispatch accordingly.

The model unifies file-like things at the substrate level without forcing pipes, sockets, or /proc entries into a foreign shape. Each payload type keeps its own semantics; the three-variant split is the interface through which VFS talks to all of them uniformly.

---

## References
<!-- txdoc:PAGE-BACKED-REFERENCES -->

- [`PAGE_SUBSTRATE_v1.md`](../01_substrate/PAGE_SUBSTRATE_v1.md) — frame allocator, FrameMeta, pmap, slab.
- [`object_model.md`](../00_meta-framework/object_model_v2.md) §3.3 (compound payloads), §5 (reference hierarchy), §6 (reclamation), §7 (operational contributions).
- [`03_STEP_MODEL_v2.md`](../../Txv3/03_STEP_MODEL_v2.md) — step outcome algebra, multi-step operations, wait primitives.
- [`02_INVARIANTS_v5.md`](../../Txv3/02_INVARIANTS_v5.md) — STEP-4, OBL-*, BIF-*.
- [`MODULE_MAP_v1.md`](../00_meta-framework/MODULE_MAP_v1.md) §5.1 (vm subsystem), §6.1 (vfs subsystem), §7 (FS instances), §8 (scripts).
- [`SUBSYSTEM_ANATOMY_v2_1.md`](../00_meta-framework/SUBSYSTEM_ANATOMY_v2_1.md) — four-module layout; PageContainer and RNodeBacking structural content lives in vm and vfs subsystems' structure/ modules.
- Linux kernel source for reference: `include/linux/fs.h` (file_operations, inode_operations, address_space_operations — the things this spec replaces).
